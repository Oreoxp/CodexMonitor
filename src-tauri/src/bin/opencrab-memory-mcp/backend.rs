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
    let mut conn = Connection::open(memory_db)?;
    // WAL persists in the file header once set, but re-asserting it on every
    // open keeps cross-process coexistence safe (see CLAUDE.md storage rule:
    // any future SQLite client on this file MUST also pragma WAL).
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    migrate(&mut conn)?;
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

/// Schema version this binary writes. The DB file records its current
/// schema in `PRAGMA user_version`; `migrate` advances it one step at a
/// time up to this value.
const SCHEMA_VERSION: i64 = 1;

/// Bring `conn`'s schema up to [`SCHEMA_VERSION`].
///
/// v0 baseline (every `IF NOT EXISTS`) runs unconditionally so a fresh DB
/// gets the initial objects; against an already-initialised DB it is a
/// no-op. After that we read `PRAGMA user_version` and apply the v1
/// step iff the file is still at v0.
fn migrate(conn: &mut Connection) -> Result<(), BackendError> {
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

    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current < SCHEMA_VERSION {
        // `ALTER TABLE ADD COLUMN` is not idempotent — a partial v1 run
        // would crash the next open() on `duplicate column name: origin`.
        // Wrap the whole step in one IMMEDIATE transaction with the
        // `user_version` bump as the last statement, so any mid-batch
        // failure rolls back cleanly and the next open() retries from v0.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Re-read under the write lock to guard the rare case of two
        // first-opens racing on the upgrade.
        let locked: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if locked < 1 {
            tx.execute_batch(
                "ALTER TABLE log ADD COLUMN origin TEXT NOT NULL DEFAULT 'self';
                 ALTER TABLE log ADD COLUMN project_hash TEXT;
                 CREATE INDEX IF NOT EXISTS idx_log_ts ON log(ts);
                 PRAGMA user_version = 1;",
            )?;
        }
        tx.commit()?;
    }
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

    // -----------------------------------------------------------------
    // Phase 6 Step 1 — schema versioning + provenance columns
    // -----------------------------------------------------------------

    /// Frozen copy of the v0 DDL — the schema as shipped at Phase 6 Step 0.
    /// Inlined here (not reused from `migrate`) so the upgrade-path tests
    /// reflect a *real* old-shaped DB even if `migrate` changes later.
    const V0_DDL_FROZEN: &str = "CREATE TABLE log (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        ts      INTEGER NOT NULL,
        summary TEXT    NOT NULL,
        detail  TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(
        detail,
        content='log',
        content_rowid='id'
    );
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;";

    fn read_user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn log_columns(conn: &Connection) -> std::collections::BTreeSet<String> {
        let mut stmt = conn.prepare("PRAGMA table_info(log)").unwrap();
        let names = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
        names.map(|n| n.unwrap()).collect()
    }

    fn expected_v1_log_columns() -> std::collections::BTreeSet<String> {
        ["id", "ts", "summary", "detail", "origin", "project_hash"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn schema_object_exists(conn: &Connection, ty: &str, name: &str) -> bool {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                params![ty, name],
                |r| r.get(0),
            )
            .unwrap();
        n == 1
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct V0Row {
        id: i64,
        ts: i64,
        summary: String,
        detail: Option<String>,
    }

    /// Lay down a v0-shape DB at `db_path` and seed three rows: monotonic
    /// `ts`, distinct summaries, one `detail`-null + two distinct non-null
    /// details. Returns the seeded rows so the upgrade-path tests can
    /// byte-compare them.
    fn build_v0_db(db_path: &std::path::Path) -> Vec<V0Row> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db_path).unwrap();
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(V0_DDL_FROZEN).unwrap();
        // Explicit: this DB is at v0. (Default user_version is 0, but
        // setting it documents the intent so a future reader can grep.)
        conn.execute_batch("PRAGMA user_version = 0;").unwrap();

        let seeds: [(i64, &str, Option<&str>); 3] = [
            (1001, "early note", None),
            (
                1002,
                "mid note about apricot",
                Some("Wrote thoughts about apricot harvests and bananas."),
            ),
            (
                1003,
                "late note about cherry",
                Some("A separate cherry-themed planning chunk."),
            ),
        ];
        let mut rows = Vec::new();
        for (ts, summary, detail) in seeds {
            conn.execute(
                "INSERT INTO log(ts, summary, detail) VALUES (?1, ?2, ?3)",
                params![ts, summary, detail],
            )
            .unwrap();
            rows.push(V0Row {
                id: conn.last_insert_rowid(),
                ts,
                summary: summary.to_string(),
                detail: detail.map(|s| s.to_string()),
            });
        }
        drop(conn);
        rows
    }

    // S1.A.1 — fresh DB lands on full v1 schema.
    #[test]
    fn s1_a1_fresh_db_lands_on_v1_schema() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), 1);
        assert_eq!(log_columns(&conn), expected_v1_log_columns());
        assert!(schema_object_exists(&conn, "table", "log_fts"));
        assert!(schema_object_exists(&conn, "trigger", "log_ai"));
        assert!(schema_object_exists(&conn, "trigger", "log_ad"));
        assert!(schema_object_exists(&conn, "trigger", "log_au"));
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
    }

    // S1.A.2 — fresh DB log_progress defaults origin='self', project_hash NULL.
    #[test]
    fn s1_a2_fresh_db_log_progress_sets_default_provenance() {
        let (_tmp, db) = db_path();
        let id = log_progress(&db, "first entry", Some("body")).unwrap();
        let conn = open(&db).unwrap();
        let (ts, summary, detail, origin, project_hash): (
            i64,
            String,
            Option<String>,
            String,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT ts, summary, detail, origin, project_hash \
                 FROM log WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert!(ts > 0);
        assert_eq!(summary, "first entry");
        assert_eq!(detail.as_deref(), Some("body"));
        assert_eq!(origin, "self");
        assert!(project_hash.is_none());
    }

    // S1.B.1 — open() three times is idempotent; user_version stays 1.
    #[test]
    fn s1_b1_open_thrice_is_idempotent_and_user_version_stays_one() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), 1);
        }
    }

    // S1.B.2 — re-open a persisted v1 DB preserves rows and version.
    #[test]
    fn s1_b2_reopen_v1_db_preserves_rows_and_version() {
        let (_tmp, db) = db_path();
        let id_a = log_progress(&db, "kept summary", Some("kept body")).unwrap();
        let id_b = log_progress(&db, "second kept", None).unwrap();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), 1);
        let a = get(&db, id_a).unwrap();
        assert!(a.found);
        assert_eq!(a.summary, "kept summary");
        assert_eq!(a.detail.as_deref(), Some("kept body"));
        let b = get(&db, id_b).unwrap();
        assert!(b.found);
        assert_eq!(b.summary, "second kept");
        assert_eq!(b.detail, None);
    }

    // S1.C.1 — old v0 DB upgrades in place; columns, index, user_version,
    // and every old row's id/ts/summary/detail are preserved byte-for-byte;
    // origin defaults to 'self' and project_hash to NULL.
    #[test]
    fn s1_c1_upgrades_v0_db_to_v1_preserving_old_rows() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), 1);
        assert_eq!(log_columns(&conn), expected_v1_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));

        let mut stmt = conn
            .prepare(
                "SELECT id, ts, summary, detail, origin, project_hash \
                 FROM log ORDER BY id",
            )
            .unwrap();
        let upgraded: Vec<(i64, i64, String, Option<String>, String, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(upgraded.len(), seeded.len());
        for (after, before) in upgraded.iter().zip(seeded.iter()) {
            assert_eq!(after.0, before.id);
            assert_eq!(after.1, before.ts);
            assert_eq!(after.2, before.summary);
            assert_eq!(after.3, before.detail);
            assert_eq!(after.4, "self");
            assert!(after.5.is_none());
        }
    }

    // S1.C.2 — after the v0→v1 upgrade, FTS still finds an old row's
    // detail. Guards against ADD COLUMN breaking external-content FTS5.
    #[test]
    fn s1_c2_fts_still_finds_old_rows_after_upgrade() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);
        drop(open(&db).unwrap());

        let hits = search(&db, "apricot", 6).unwrap();
        assert_eq!(hits.len(), 1, "hits = {hits:?}");
        let expected = &seeded[1];
        assert_eq!(hits[0].id, expected.id);
        assert_eq!(hits[0].ts, expected.ts);
        assert_eq!(hits[0].summary, expected.summary);
        assert!(
            hits[0].snippet.contains("apricot"),
            "snippet = {}",
            hits[0].snippet
        );
    }

    // S1.C.3 — after the upgrade, log_progress writes a new row with
    // origin='self', project_hash NULL, and FTS surfaces both the old
    // and new row for a shared keyword (proves the INSERT trigger still
    // populates log_fts after ALTER TABLE ADD COLUMN).
    #[test]
    fn s1_c3_log_progress_after_upgrade_writes_provenance_and_fts() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);
        let new_id = log_progress(
            &db,
            "post-upgrade apricot follow-up",
            Some("Another apricot dive after the schema migration."),
        )
        .unwrap();

        let conn = open(&db).unwrap();
        let (origin, project_hash): (String, Option<String>) = conn
            .query_row(
                "SELECT origin, project_hash FROM log WHERE id = ?1",
                params![new_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin, "self");
        assert!(project_hash.is_none());

        let hits = search(&db, "apricot", 6).unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        let old_apricot_id = seeded[1].id;
        assert!(
            ids.contains(&old_apricot_id),
            "expected old apricot row {old_apricot_id} in hits {ids:?}"
        );
        assert!(
            ids.contains(&new_id),
            "expected new apricot row {new_id} in hits {ids:?}"
        );
    }
}
