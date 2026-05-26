// Phase 6 Step 1 — per-agent SQLite log store for `opencrab-memory-mcp`.
//
// Storage layer: one SQLite file per agent at
// `~/.opencrab/agents/<id>/memory.db`. This server is the file's sole
// owner — no other process reads or writes it — so the schema is created
// idempotently here at open time, and the FTS5 index is persistent (no
// per-call rebuild). WAL is asserted on every open so that any later
// read-only client (a future Phase 6/7 step) can attach without blocking us.
//
// Tools surface (consumed by `server.rs`):
//   * `log_progress(summary, detail?)` — write tool, structured (no text
//     tags). One row per call; `ts` is captured here as epoch ms.
//   * `search(query, limit)` — full-text search over `detail` via the
//     `log_fts` virtual table; bm25-ranked hits join back to `log` to
//     return id / ts / summary / detail snippet.
//   * `get(id)` — fetch one full `log` row by primary key.
//
// FTS5 uses external-content (`content='log', content_rowid='id'`): `log`
// stays the truth source, `log_fts` is just an index, and three standard
// triggers (`AFTER INSERT|UPDATE|DELETE ON log`) keep it in sync.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::Serialize;

/// Default number of search hits when the caller does not pass `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 6;
/// Hard cap on `limit` so one call cannot pull the whole archive.
const MAX_SEARCH_LIMIT: usize = 25;
/// Per-snippet character cap. The `detail` column is free-form, so a
/// pathological entry should not flood one search response.
const MAX_SNIPPET_CHARS: usize = 800;

/// One search hit: a row of `log`, with `detail` truncated to a snippet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryHit {
    pub id: i64,
    pub ts: i64,
    pub summary: String,
    pub snippet: String,
}

/// One full log entry, as returned by `get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryEntry {
    pub id: i64,
    pub found: bool,
    pub ts: i64,
    pub summary: String,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub enum BackendError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    /// `log_progress` was called with an empty summary.
    EmptySummary,
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Io(e) => write!(f, "memory I/O error: {e}"),
            BackendError::Sqlite(e) => write!(f, "memory store error: {e}"),
            BackendError::EmptySummary => write!(f, "summary must be a non-empty string"),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<std::io::Error> for BackendError {
    fn from(e: std::io::Error) -> Self {
        BackendError::Io(e)
    }
}

impl From<rusqlite::Error> for BackendError {
    fn from(e: rusqlite::Error) -> Self {
        BackendError::Sqlite(e)
    }
}

/// Clamp a caller-supplied `limit` into `1..=MAX_SEARCH_LIMIT`, defaulting
/// when absent.
pub fn clamp_limit(requested: Option<u64>) -> usize {
    match requested {
        Some(n) => (n as usize).clamp(1, MAX_SEARCH_LIMIT),
        None => DEFAULT_SEARCH_LIMIT,
    }
}

/// Open `memory_db`, set WAL, and ensure the schema (idempotent).
/// Creates the parent directory if it does not exist.
pub fn open(memory_db: &Path) -> Result<Connection, BackendError> {
    if let Some(parent) = memory_db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(memory_db)?;
    // WAL persists in the file header once set, but re-asserting it on every
    // open keeps cross-process coexistence safe (see CLAUDE.md storage rule:
    // any future SQLite client on this file MUST also pragma WAL).
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    migrate(&conn)?;
    Ok(conn)
}

/// Insert one log row. Returns the new row's id. `ts` is captured here.
pub fn log_progress(
    memory_db: &Path,
    summary: &str,
    detail: Option<&str>,
) -> Result<i64, BackendError> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Err(BackendError::EmptySummary);
    }
    let conn = open(memory_db)?;
    let ts = current_time_ms();
    conn.execute(
        "INSERT INTO log(ts, summary, detail) VALUES (?1, ?2, ?3)",
        params![ts, summary, detail],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Full-text search the agent's log store. Returns the most relevant rows
/// (bm25-ranked, best first). A query with no usable terms returns an empty
/// list rather than an error.
pub fn search(
    memory_db: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<MemoryHit>, BackendError> {
    let Some(match_expr) = build_fts_match(query) else {
        return Ok(Vec::new());
    };
    let conn = open(memory_db)?;
    let mut stmt = conn.prepare(
        "SELECT log.id, log.ts, log.summary, log.detail \
         FROM log_fts JOIN log ON log.id = log_fts.rowid \
         WHERE log_fts MATCH ?1 \
         ORDER BY bm25(log_fts) \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![match_expr, limit as i64], |row| {
        let id: i64 = row.get(0)?;
        let ts: i64 = row.get(1)?;
        let summary: String = row.get(2)?;
        let detail: Option<String> = row.get(3)?;
        Ok(MemoryHit {
            id,
            ts,
            summary,
            snippet: truncate_snippet(detail.as_deref().unwrap_or("")),
        })
    })?;
    let mut hits = Vec::new();
    for hit in rows {
        hits.push(hit?);
    }
    Ok(hits)
}

/// Fetch one log row by id. A missing id is a normal `found: false` result,
/// not an error.
pub fn get(memory_db: &Path, id: i64) -> Result<MemoryEntry, BackendError> {
    let conn = open(memory_db)?;
    let mut stmt =
        conn.prepare("SELECT id, ts, summary, detail FROM log WHERE id = ?1")?;
    let row = stmt
        .query_row(params![id], |row| {
            let id: i64 = row.get(0)?;
            let ts: i64 = row.get(1)?;
            let summary: String = row.get(2)?;
            let detail: Option<String> = row.get(3)?;
            Ok(MemoryEntry {
                id,
                found: true,
                ts,
                summary,
                detail,
            })
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(row.unwrap_or(MemoryEntry {
        id,
        found: false,
        ts: 0,
        summary: String::new(),
        detail: None,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create the `log` table, the `log_fts` virtual table, and the three
/// sync triggers if they do not already exist.
fn migrate(conn: &Connection) -> Result<(), BackendError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS log (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            ts      INTEGER NOT NULL,
            summary TEXT    NOT NULL,
            detail  TEXT
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS log_fts USING fts5(
            detail,
            content='log',
            content_rowid='id'
        );
        CREATE TRIGGER IF NOT EXISTS log_ai AFTER INSERT ON log BEGIN
            INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
        END;
        CREATE TRIGGER IF NOT EXISTS log_ad AFTER DELETE ON log BEGIN
            INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        END;
        CREATE TRIGGER IF NOT EXISTS log_au AFTER UPDATE ON log BEGIN
            INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
            INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
        END;",
    )?;
    Ok(())
}

fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Turn a free-text query into an FTS5 MATCH expression. Each whitespace
/// token is wrapped in a double-quoted string literal (so FTS5 query
/// operators inside the token are treated as content, not syntax — the only
/// in-literal escape is `"` → `""`), and tokens are OR-joined so the search
/// is recall-oriented with bm25 surfacing the best matches. Tokens with no
/// alphanumeric character are dropped; a query that yields none returns
/// `None` (the caller treats that as "no results").
fn build_fts_match(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|token| token.chars().any(|c| c.is_alphanumeric()))
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

fn truncate_snippet(body: &str) -> String {
    if body.chars().count() <= MAX_SNIPPET_CHARS {
        body.to_string()
    } else {
        let head: String = body.chars().take(MAX_SNIPPET_CHARS).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested/memory.db");
        (tmp, path)
    }

    #[test]
    fn schema_creation_is_idempotent() {
        let (_tmp, db) = db_path();
        // First open: creates parent dir, file, schema.
        drop(open(&db).unwrap());
        assert!(db.exists(), "open must create the db file");
        // Second open against the same file: must not error and must not
        // recreate or duplicate any object.
        let conn = open(&db).unwrap();
        let table_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='log'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 1);
        let trigger_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'log_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger_count, 3);
    }

    #[test]
    fn open_sets_wal_journal_mode() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn log_progress_inserts_summary_only() {
        let (_tmp, db) = db_path();
        let id = log_progress(&db, "wrote a kickoff plan", None).unwrap();
        assert!(id > 0);
        let entry = get(&db, id).unwrap();
        assert!(entry.found);
        assert_eq!(entry.id, id);
        assert_eq!(entry.summary, "wrote a kickoff plan");
        assert_eq!(entry.detail, None);
        assert!(entry.ts > 0);
    }

    #[test]
    fn log_progress_inserts_summary_and_detail() {
        let (_tmp, db) = db_path();
        let id = log_progress(
            &db,
            "shipped auth refactor",
            Some("Replaced the legacy session middleware with the JWT/OAuth flow."),
        )
        .unwrap();
        let entry = get(&db, id).unwrap();
        assert!(entry.found);
        assert_eq!(entry.summary, "shipped auth refactor");
        assert_eq!(
            entry.detail.as_deref(),
            Some("Replaced the legacy session middleware with the JWT/OAuth flow.")
        );
    }

    #[test]
    fn log_progress_trims_summary_and_rejects_empty() {
        let (_tmp, db) = db_path();
        assert!(matches!(
            log_progress(&db, "", None),
            Err(BackendError::EmptySummary)
        ));
        assert!(matches!(
            log_progress(&db, "   \n\t  ", None),
            Err(BackendError::EmptySummary)
        ));
        // Trim is applied so leading/trailing whitespace does not survive.
        let id = log_progress(&db, "  trimmed ok  ", None).unwrap();
        assert_eq!(get(&db, id).unwrap().summary, "trimmed ok");
    }

    #[test]
    fn fts_round_trip_search_then_get() {
        let (_tmp, db) = db_path();
        let id_a = log_progress(
            &db,
            "auth refactor day 1",
            Some("Refactored the OAuth middleware to use the new token model."),
        )
        .unwrap();
        let _id_b = log_progress(
            &db,
            "lunch break notes",
            Some("Unrelated standup notes about lunch and the cafeteria."),
        )
        .unwrap();

        let hits = search(&db, "OAuth", 6).unwrap();
        assert_eq!(hits.len(), 1, "hits = {hits:?}");
        assert_eq!(hits[0].id, id_a);
        assert_eq!(hits[0].summary, "auth refactor day 1");
        assert!(
            hits[0].snippet.contains("OAuth"),
            "snippet = {}",
            hits[0].snippet
        );
        assert!(hits[0].ts > 0);

        let entry = get(&db, id_a).unwrap();
        assert!(entry.found);
        assert_eq!(
            entry.detail.as_deref(),
            Some("Refactored the OAuth middleware to use the new token model.")
        );
    }

    #[test]
    fn search_ranks_more_relevant_first_via_bm25() {
        let (_tmp, db) = db_path();
        let id_two = log_progress(
            &db,
            "review alpha vs beta",
            Some("We chose the alpha approach over beta after the review."),
        )
        .unwrap();
        let id_one =
            log_progress(&db, "tiny note", Some("Logged a few alpha notes.")).unwrap();

        let hits = search(&db, "alpha beta", 6).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, id_two, "two-term match must rank first");
        assert_eq!(hits[1].id, id_one);
    }

    #[test]
    fn search_returns_empty_for_query_with_no_searchable_terms() {
        let (_tmp, db) = db_path();
        log_progress(&db, "real entry", Some("Real content here.")).unwrap();
        assert_eq!(search(&db, "  !!!  ??? ", 6).unwrap(), Vec::new());
    }

    #[test]
    fn search_returns_empty_on_fresh_db() {
        let (_tmp, db) = db_path();
        assert_eq!(search(&db, "anything", 6).unwrap(), Vec::new());
    }

    #[test]
    fn search_respects_the_limit() {
        let (_tmp, db) = db_path();
        for n in 0..10 {
            log_progress(
                &db,
                &format!("entry {n}"),
                Some("shared keyword line one keyword line"),
            )
            .unwrap();
        }
        let hits = search(&db, "keyword", 3).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn search_skips_rows_with_null_detail() {
        let (_tmp, db) = db_path();
        // summary-only rows are intentionally not full-text searchable.
        log_progress(&db, "keyword in summary only", None).unwrap();
        assert_eq!(search(&db, "keyword", 6).unwrap(), Vec::new());
    }

    #[test]
    fn get_reports_not_found_for_unknown_id() {
        let (_tmp, db) = db_path();
        let entry = get(&db, 999).unwrap();
        assert!(!entry.found);
        assert_eq!(entry.id, 999);
        assert_eq!(entry.summary, "");
        assert_eq!(entry.detail, None);
    }

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None), DEFAULT_SEARCH_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(1)), 1);
        assert_eq!(clamp_limit(Some(9999)), MAX_SEARCH_LIMIT);
    }
}
