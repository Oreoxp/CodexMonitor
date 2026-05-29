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

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;

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

/// Register the `vec0` virtual-table module via `sqlite3_auto_extension`
/// exactly once per process. Idempotent — safe to call from every
/// possible mount point (`main`, `open`, individual tests).
///
/// `sqlite3_auto_extension` registers a callback that fires on EVERY
/// subsequent `sqlite3_open`, so this is process-wide. Crucially, it
/// does NOT need rusqlite's `load_extension` feature — auto-extension
/// is always compiled in, even with the `bundled` SQLite that has
/// runtime `.load` disabled.
pub fn ensure_vec_extension() {
    static REGISTERED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    REGISTERED.get_or_init(|| {
        // The cast is the documented hack: sqlite3_auto_extension takes
        // a `void(*)(void)` slot but SQLite actually calls it with the
        // extension entry-point signature `int(sqlite3*, char**, …)`.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Open `memory_db`, set WAL, and ensure the schema (idempotent).
/// Creates the parent directory if it does not exist.
pub fn open(memory_db: &Path) -> Result<Connection, BackendError> {
    // Belt-and-braces: even though `main` registers vec0 once at
    // startup, tests don't go through `main`. Guarded by OnceLock so
    // this costs one atomic load on the hot path after the first call.
    ensure_vec_extension();
    if let Some(parent) = memory_db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(memory_db)?;
    // WAL persists in the file header once set, but re-asserting it on every
    // open keeps cross-process coexistence safe (see CLAUDE.md storage rule:
    // any future SQLite client on this file MUST also pragma WAL).
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    // 5-second busy_timeout: in-process the MCP server and the background
    // ingester (Phase 6 Step 2-ingest) each open their own connection; WAL
    // permits concurrent reads but writes still serialize. The busy_timeout
    // lets a contending writer block briefly instead of immediately failing
    // with SQLITE_BUSY.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
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

/// Reciprocal-rank-fusion mixing constant. RRF score for one entry is
/// `Σ 1 / (RRF_K + rank)` summed over the ranked lists it appears in.
/// `60` is the value Cormack/Clarke/Buettcher recommended in the original
/// paper and is the default in most implementations.
pub const RRF_K: usize = 60;

/// Candidate pool size for the hybrid search: `limit * SEARCH_POOL_MULTIPLIER`
/// clamped to a sensible floor. The pool is what each underlying ranker
/// (FTS, KNN) returns; RRF picks the best `limit` from the union.
pub const SEARCH_POOL_MULTIPLIER: usize = 4;
pub const SEARCH_POOL_MIN: usize = 20;

/// Hard wall-clock budget for embedding the user's query at search
/// time. If the embedder takes longer than this, we abort and fall
/// back to pure FTS — `memory_search` MUST stay snappy even if the
/// provider is slow.
pub const QUERY_EMBED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Hybrid search: FTS bm25 + (optional) vector KNN, merged via RRF.
///
/// Returns the most relevant rows in the merged ranking (best first).
/// Degrades gracefully to pure-FTS in every failure mode of the vector
/// path: no `embedder`, embed error, embed timeout, dim mismatch, or
/// empty `log_vec`. The `MemoryHit` shape is unchanged from the
/// pre-S4 FTS-only API.
///
/// **Signature note:** we take `memory_db: &Path` rather than the
/// `&Connection` originally drafted because `rusqlite::Connection`
/// is `!Sync`, so holding `&Connection` across the embed `.await`
/// would make this function's future non-`Send` — and rmcp's
/// `ServerHandler::call_tool` requires `Send + 'static`. Opening once
/// per phase is cheap (WAL DB, no schema work on already-migrated
/// files; `ensure_vec_extension` is OnceLock-guarded).
pub async fn search(
    memory_db: &Path,
    embedder: Option<&dyn Embedder>,
    query: &str,
    limit: usize,
) -> Result<Vec<MemoryHit>, BackendError> {
    search_with_timeout(memory_db, embedder, query, limit, QUERY_EMBED_TIMEOUT).await
}

/// Same as [`search`] but with an injectable embed timeout for tests.
///
/// Three phases keep the !Sync `Connection` out of the future's
/// captured state across `.await`:
///   1. sync — open conn, FTS bm25 ranking, drop conn
///   2. async, no conn — embed the query under a timeout
///   3. sync — re-open conn, KNN over log_vec, RRF merge, fetch
async fn search_with_timeout(
    memory_db: &Path,
    embedder: Option<&dyn Embedder>,
    query: &str,
    limit: usize,
    embed_timeout: std::time::Duration,
) -> Result<Vec<MemoryHit>, BackendError> {
    if query.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let pool_size = (limit * SEARCH_POOL_MULTIPLIER).max(SEARCH_POOL_MIN);

    // ---- Phase 1 (sync): FTS bm25; conn scoped to this block.
    let fts_ranked: Vec<i64> = {
        let conn = open(memory_db)?;
        match build_fts_match(query) {
            Some(match_expr) => fts_ranked_ids(&conn, &match_expr, pool_size)?,
            None => Vec::new(), // query had no FTS-usable tokens — vector may still hit
        }
    }; // conn dropped here — does NOT live across the embed await below

    // ---- Phase 2 (async, no DB access): embed the query ----
    let query_vec: Option<Vec<f32>> = match embedder {
        Some(emb) => match embed_query_with_timeout(emb, query, embed_timeout).await {
            Ok(v) => Some(v),
            Err(reason) => {
                eprintln!("[search] vector path skipped: {reason}");
                None
            }
        },
        None => None,
    };

    // ---- Phase 3 (sync): re-open, KNN, merge, fetch.
    let conn = open(memory_db)?;
    let vec_ranked: Vec<i64> = match query_vec {
        Some(v) => match knn_ranked_ids(&conn, &v, pool_size) {
            Ok(ids) => ids,
            Err(reason) => {
                eprintln!("[search] vector path skipped: {reason}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    let merged = rrf_merge(&[&fts_ranked, &vec_ranked], limit);
    fetch_hits_in_order(&conn, &merged)
}

/// FTS bm25 ranking only, returns ordered ids (best first).
fn fts_ranked_ids(
    conn: &Connection,
    match_expr: &str,
    pool_size: usize,
) -> Result<Vec<i64>, BackendError> {
    let mut stmt = conn.prepare(
        "SELECT log.id FROM log_fts JOIN log ON log.id = log_fts.rowid \
         WHERE log_fts MATCH ?1 \
         ORDER BY bm25(log_fts) \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![match_expr, pool_size as i64], |row| {
        row.get::<_, i64>(0)
    })?;
    let mut out = Vec::with_capacity(pool_size);
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Async, no DB access: embed `query` under a wall-clock budget and
/// validate the returned vector's dim matches the embedder's
/// `dimensions()`. Returns `Err(reason)` for every failure mode the
/// caller should silently degrade to FTS on (timeout, embed error,
/// dim mismatch). Crucially this function does NOT take `&Connection`,
/// so the caller's `&Connection` borrow doesn't have to survive the
/// await (rusqlite's `Connection` is !Sync; that would break the
/// `Send`-ness of any future holding it across `.await`).
async fn embed_query_with_timeout(
    embedder: &dyn Embedder,
    query: &str,
    timeout: std::time::Duration,
) -> Result<Vec<f32>, String> {
    let texts = vec![query.to_string()];
    let vectors = match tokio::time::timeout(timeout, embedder.embed(&texts)).await {
        Ok(Ok(v)) => v,
        Ok(Err(err)) => return Err(format!("embed error: {err}")),
        Err(_) => return Err(format!("embed timed out after {timeout:?}")),
    };
    let q_vec = vectors
        .into_iter()
        .next()
        .ok_or_else(|| "embedder returned 0 vectors for 1 input".to_string())?;
    if q_vec.len() != embedder.dimensions() {
        return Err(format!(
            "query embedding dim {} != embedder.dimensions() {}",
            q_vec.len(),
            embedder.dimensions()
        ));
    }
    Ok(q_vec)
}

/// Sync: KNN over log_vec given an already-computed query embedding.
/// Returns the rowids in distance-ascending order. vec0 needs either
/// `LIMIT` or `k = ?` on its own scan — we use `k = ?` because it
/// composes safely with JOINs at the call site, even though here
/// we're just selecting rowid.
fn knn_ranked_ids(
    conn: &Connection,
    q_vec: &[f32],
    pool_size: usize,
) -> Result<Vec<i64>, String> {
    let q_json = vec_to_match_json(q_vec);
    let mut stmt = conn
        .prepare(
            "SELECT rowid FROM log_vec \
             WHERE embedding MATCH ?1 AND k = ?2 \
             ORDER BY distance",
        )
        .map_err(|e| format!("prepare KNN: {e}"))?;
    let rows = stmt
        .query_map(params![q_json, pool_size as i64], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|e| format!("KNN query: {e}"))?;
    let mut out: Vec<i64> = Vec::with_capacity(pool_size);
    for r in rows {
        out.push(r.map_err(|e| format!("KNN row: {e}"))?);
    }
    Ok(out)
}

/// Pure: Reciprocal Rank Fusion across N ranked lists.
///
/// Score for an id is `Σ 1 / (RRF_K + (rank + 1))` summed over the
/// lists it appears in; `rank` starts at 0 inside each list. Ties on
/// score are broken by id ascending so the order is deterministic.
fn rrf_merge(ranked_lists: &[&[i64]], limit: usize) -> Vec<i64> {
    use std::collections::HashMap;
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in ranked_lists {
        for (rank, &id) in list.iter().enumerate() {
            *scores.entry(id).or_insert(0.0) += 1.0 / (RRF_K as f64 + (rank + 1) as f64);
        }
    }
    let mut pairs: Vec<(i64, f64)> = scores.into_iter().collect();
    pairs.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    pairs.into_iter().take(limit).map(|(id, _)| id).collect()
}

/// JOIN final ids back to `log` and project into `MemoryHit`. Order
/// preserved as given (i.e. the RRF order).
fn fetch_hits_in_order(
    conn: &Connection,
    ids: &[i64],
) -> Result<Vec<MemoryHit>, BackendError> {
    let mut stmt =
        conn.prepare("SELECT ts, summary, detail FROM log WHERE id = ?1")?;
    let mut hits = Vec::with_capacity(ids.len());
    for id in ids {
        let row = stmt
            .query_row(params![id], |r| {
                let ts: i64 = r.get(0)?;
                let summary: String = r.get(1)?;
                let detail: Option<String> = r.get(2)?;
                Ok(MemoryHit {
                    id: *id,
                    ts,
                    summary,
                    snippet: truncate_snippet(detail.as_deref().unwrap_or("")),
                })
            })
            .ok();
        if let Some(hit) = row {
            hits.push(hit);
        }
        // A merged id that no longer matches a log row (race vs delete?
        // future GC?) is silently dropped — caller just gets a shorter
        // list. Cleaner than surfacing an "id 42 vanished" error.
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
// Phase 6 Step 2-ingest — rollout JSONL → raw_thread / raw_event
// ---------------------------------------------------------------------------

/// One rollout file's attribution, derived purely from its path. No payload
/// parsing happens here — that's S3's job. See [`parse_rollout_attribution`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutAttribution {
    pub thread_id: String,
    pub team_id: String,
    pub project_hash: String,
}

/// One ingest pass's summary, for logging + test assertions.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IngestStats {
    pub files_seen: u64,
    pub threads_touched: u64,
    pub events_inserted: u64,
}

/// Default polling interval for the background ingest loop.
pub const INGEST_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Pull thread / team / project attribution out of a rollout file path.
///
/// The codex-cli filename shape (see `codex-rs/rollout/src/recorder.rs`) is
/// `rollout-<19-char-iso-ts>-<36-char-uuid>.jsonl`. The ISO timestamp itself
/// contains `-`, so we **don't** split on `-` — instead we take the last 36
/// characters of the file stem and validate them as the canonical UUID
/// 8-4-4-4-12 shape. `team_id` / `project_hash` come from the two path
/// segments immediately under `team_sessions/` (matching OpenCrab's
/// per-agent rollout layout per the recon in S2-R).
///
/// Returns `None` if any of: the filename does not end in `.jsonl`, the
/// stem's last 36 chars are not UUID-shaped, `team_sessions` is absent
/// from the path, or there are not exactly two segments between
/// `team_sessions/` and the file.
pub fn parse_rollout_attribution(path: &std::path::Path) -> Option<RolloutAttribution> {
    let file_name = path.file_name()?.to_str()?;
    let stem = file_name.strip_suffix(".jsonl")?;
    if stem.len() < 36 {
        return None;
    }
    let tail_start = stem.len() - 36;
    // Guard against panicking when the cut would land mid-codepoint —
    // any multi-byte char straddling the tail boundary means the last
    // 36 bytes can't possibly be the ASCII-only UUID we're looking for.
    if !stem.is_char_boundary(tail_start) {
        return None;
    }
    let thread_id = &stem[tail_start..];
    if !is_uuid_form(thread_id) {
        return None;
    }
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let idx = components.iter().position(|c| *c == "team_sessions")?;
    let team_id = (*components.get(idx + 1)?).to_string();
    let project_hash = (*components.get(idx + 2)?).to_string();
    // The filename must sit directly under `<team_sessions>/<team>/<hash>/`,
    // not deeper — anything else is not the OpenCrab per-agent layout we
    // know how to attribute.
    if components.get(idx + 3).copied() != Some(file_name) || components.get(idx + 4).is_some() {
        return None;
    }
    Some(RolloutAttribution {
        thread_id: thread_id.to_string(),
        team_id,
        project_hash,
    })
}

fn is_uuid_form(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes[8] != b'-' || bytes[13] != b'-' || bytes[18] != b'-' || bytes[23] != b'-' {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| {
        if i == 8 || i == 13 || i == 18 || i == 23 {
            *b == b'-'
        } else {
            b.is_ascii_hexdigit()
        }
    })
}

/// One ingest pass. Walks `scan_root/*/*/rollout-*.jsonl`, pure-copies new
/// lines (`UNIQUE(thread_id, line_no) + INSERT OR IGNORE` gives idempotency),
/// and advances each thread's cursor. Errors on individual files are logged
/// + skipped — the pass keeps going.
///
/// Caller owns the connection; this never opens its own. The signature takes
/// `&Connection` (not `&mut`) so the bg loop can keep a long-lived handle and
/// reuse it across passes; per-file IMMEDIATE transactions are issued via
/// raw SQL.
pub fn ingest_once(
    conn: &Connection,
    scan_root: &std::path::Path,
    agent_id: &str,
) -> Result<IngestStats, BackendError> {
    let files = enumerate_rollout_files(scan_root)?;
    let mut stats = IngestStats {
        files_seen: files.len() as u64,
        ..Default::default()
    };
    let mut touched: std::collections::HashSet<String> = std::collections::HashSet::new();

    for path in &files {
        match ingest_one_file(conn, path, agent_id) {
            Ok(file_stats) => {
                stats.events_inserted += file_stats.events_inserted;
                if let Some(tid) = file_stats.thread_id {
                    touched.insert(tid);
                }
            }
            Err(err) => {
                eprintln!(
                    "[ingest] {}: {} — skipping this file, pass continues",
                    path.display(),
                    err
                );
            }
        }
    }
    stats.threads_touched = touched.len() as u64;
    Ok(stats)
}

#[derive(Debug, Default)]
struct FileIngestResult {
    thread_id: Option<String>,
    events_inserted: u64,
}

/// Walk `scan_root` two levels deep and collect every `rollout-*.jsonl`
/// file directly under `<team_id>/<project_hash>/`. Missing scan_root is
/// returned as an empty list (not an error) — that's the normal state for
/// a brand-new agent with no team rollouts yet. Sort by path for stable
/// test ordering.
fn enumerate_rollout_files(scan_root: &std::path::Path) -> Result<Vec<PathBuf>, BackendError> {
    let mut out: Vec<PathBuf> = Vec::new();
    if !scan_root.exists() {
        return Ok(out);
    }
    for team_entry in std::fs::read_dir(scan_root)? {
        let team_entry = team_entry?;
        if !team_entry.file_type()?.is_dir() {
            continue;
        }
        for hash_entry in std::fs::read_dir(team_entry.path())? {
            let hash_entry = hash_entry?;
            if !hash_entry.file_type()?.is_dir() {
                continue;
            }
            for file_entry in std::fs::read_dir(hash_entry.path())? {
                let file_entry = file_entry?;
                if !file_entry.file_type()?.is_file() {
                    continue;
                }
                let path = file_entry.path();
                let matches = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
                    .unwrap_or(false);
                if matches {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

fn ingest_one_file(
    conn: &Connection,
    path: &std::path::Path,
    agent_id: &str,
) -> Result<FileIngestResult, BackendError> {
    let Some(attribution) = parse_rollout_attribution(path) else {
        eprintln!(
            "[ingest] {}: unparseable attribution (not <team_sessions>/<team>/<hash>/rollout-…-<uuid>.jsonl), skipping",
            path.display()
        );
        return Ok(FileIngestResult::default());
    };

    let now = current_time_ms();
    let source_path = path.to_string_lossy().to_string();

    // Per-file IMMEDIATE transaction: takes the WAL write lock for the
    // duration of the upsert + line inserts + cursor bump. `Connection::
    // transaction_with_behavior` requires `&mut Connection`; since
    // `ingest_once` takes `&Connection`, we drive the lifecycle with raw
    // SQL and a manual RAII guard for the rollback path.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = ingest_one_file_in_tx(conn, path, agent_id, &attribution, &source_path, now);
    match &result {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
        }
        Err(_) => {
            // Best-effort: a ROLLBACK after a failed BEGIN IMMEDIATE is
            // safe; the connection returns to autocommit either way.
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
    result
}

fn ingest_one_file_in_tx(
    conn: &Connection,
    path: &std::path::Path,
    agent_id: &str,
    attribution: &RolloutAttribution,
    source_path: &str,
    now: i64,
) -> Result<FileIngestResult, BackendError> {
    // Upsert raw_thread. We deliberately do NOT touch source / parent_thread_id
    // / cwd here — those fields are payload-derived (S3). On re-encounter of
    // an existing thread the IGNORE keeps every column unchanged; only the
    // cursor + last_ingest_ts move (via the UPDATE at the end).
    conn.execute(
        "INSERT OR IGNORE INTO raw_thread \
         (thread_id, agent_id, team_id, project_hash, source_path, \
          first_seen_ts, last_ingest_ts, last_offset, last_line_no) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 0, 0)",
        params![
            &attribution.thread_id,
            agent_id,
            &attribution.team_id,
            &attribution.project_hash,
            source_path,
            now,
        ],
    )?;

    let (last_offset, last_line_no): (i64, i64) = conn.query_row(
        "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
        params![&attribution.thread_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len() as i64;

    // Shrink-detect: if the file is shorter than where we left off, the
    // log either rotated or was truncated. We DELETE every raw_event row
    // for this thread (inside the same IMMEDIATE tx) and re-scan from
    // offset 0, so the table reflects the file's current contents
    // exactly — no orphan rows beyond the new EOF.
    //
    // We also reset `last_distilled_line_no = 0` here: the post-shrink
    // file may have different *content* at line numbers the distiller
    // already consumed, so anything the distiller previously summarised
    // is now invalid. Resetting forces re-distillation on the next pass;
    // missing the reset would silently leave the distiller's view of
    // the thread out of sync with reality.
    let (seek_offset, base_line_no): (u64, i64) = if file_size >= last_offset {
        (last_offset as u64, last_line_no)
    } else {
        conn.execute(
            "DELETE FROM raw_event WHERE thread_id = ?1",
            params![&attribution.thread_id],
        )?;
        conn.execute(
            "UPDATE raw_thread SET last_distilled_line_no = 0 WHERE thread_id = ?1",
            params![&attribution.thread_id],
        )?;
        (0, 0)
    };

    use std::io::{BufRead, Seek, SeekFrom};
    let mut reader = std::io::BufReader::new(file);
    reader.seek(SeekFrom::Start(seek_offset))?;

    // Only complete lines (terminated by `\n`) get committed. If the
    // final read_line returns a partial line (no trailing `\n` — the
    // writer is mid-append, or the file just doesn't end in a newline),
    // we break *without* inserting it and *without* advancing the cursor.
    // The next pass picks the line up once it has been finished, so
    // `line_no` always equals the file's 1-based count of *complete*
    // lines and no half-line ever gets split across two raw_event rows.
    let mut events_inserted: u64 = 0;
    let mut committed_line_no = base_line_no;
    let mut committed_offset: i64 = seek_offset as i64;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        if !buf.ends_with('\n') {
            // Torn line at EOF — leave it uncommitted.
            break;
        }
        // Strip the trailing line terminator (`\n` or `\r\n`); inner
        // whitespace is preserved verbatim ("不 trim、不解析" per S2-ingest).
        let payload: &str = buf
            .strip_suffix('\n')
            .map(|s| s.strip_suffix('\r').unwrap_or(s))
            .unwrap_or(&buf);
        let next_line_no = committed_line_no + 1;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO raw_event(thread_id, line_no, payload, ingested_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![&attribution.thread_id, next_line_no, payload, now],
        )?;
        events_inserted += inserted as u64;
        committed_line_no = next_line_no;
        committed_offset += n as i64;
    }

    // S3-schema: `last_growth_ts` only advances when this pass actually
    // wrote new raw_event rows (incl. the shrink-then-re-insert path).
    // `last_ingest_ts` keeps bumping every pass — the two timestamps
    // intentionally have different semantics so the distiller can poll
    // "anything new since last time?" against `last_growth_ts` without
    // false positives from no-op passes.
    if events_inserted > 0 {
        conn.execute(
            "UPDATE raw_thread SET last_offset = ?1, last_line_no = ?2, \
                                    last_ingest_ts = ?3, last_growth_ts = ?3 \
             WHERE thread_id = ?4",
            params![committed_offset, committed_line_no, now, &attribution.thread_id],
        )?;
    } else {
        conn.execute(
            "UPDATE raw_thread SET last_offset = ?1, last_line_no = ?2, last_ingest_ts = ?3 \
             WHERE thread_id = ?4",
            params![committed_offset, committed_line_no, now, &attribution.thread_id],
        )?;
    }

    Ok(FileIngestResult {
        thread_id: Some(attribution.thread_id.clone()),
        events_inserted,
    })
}

/// Background ingest loop. Polls `scan_root` every [`INGEST_POLL_INTERVAL`]
/// and runs one `ingest_once` per tick on a blocking thread.
///
/// Pass errors are logged and the loop continues — this task **never
/// panics** out of the process. Each pass opens its own short-lived
/// connection via [`open`] (so the schema is fresh-migrated + WAL +
/// busy_timeout are all set), independent of the MCP server's connection
/// pool. WAL gives us safe concurrent reads, the per-file IMMEDIATE
/// transaction + busy_timeout serialize concurrent writes.
pub async fn run_ingest_loop(memory_db: PathBuf, scan_root: PathBuf, agent_id: String) {
    loop {
        let memory_db_pass = memory_db.clone();
        let scan_root_pass = scan_root.clone();
        let agent_id_pass = agent_id.clone();
        let join_result =
            tokio::task::spawn_blocking(move || -> Result<IngestStats, BackendError> {
                let conn = open(&memory_db_pass)?;
                ingest_once(&conn, &scan_root_pass, &agent_id_pass)
            })
            .await;
        match join_result {
            Ok(Ok(stats)) => {
                if stats.events_inserted > 0 {
                    eprintln!(
                        "[ingest] agent={agent_id} files_seen={} threads_touched={} events_inserted={}",
                        stats.files_seen, stats.threads_touched, stats.events_inserted,
                    );
                }
            }
            Ok(Err(err)) => {
                eprintln!("[ingest] agent={agent_id}: pass failed: {err}");
            }
            Err(join_err) => {
                eprintln!(
                    "[ingest] agent={agent_id}: blocking task join error: {join_err}"
                );
            }
        }
        tokio::time::sleep(INGEST_POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Phase 6 Step 3a — transcript rendering + compaction-segment splitting
// ---------------------------------------------------------------------------
//
// Pure functions that take raw_event JSONL lines (in `(line_no, payload)` form,
// already ordered by line_no) and produce a transcript string ready for an
// LLM distiller, segmented at top-level `"type":"compacted"` boundaries.
//
// CRITICAL: we navigate the JSON via `serde_json::Value` + string keys only.
// **No `codex_protocol` / `ResponseItem` / `RolloutItem` type import here.**
// The raw_event table is codex-cli-agnostic (§五 raw 表抽象边界); locking
// these structs in would couple OpenCrab's distiller to a moving upstream.

/// Classification of one rollout JSONL line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedLine {
    /// Goes into the transcript.
    Kept(RenderedItem),
    /// Top-level `type == "compacted"` — a segment boundary. The `summary`
    /// is the marker line's `payload.message` (empty string if missing).
    Boundary { summary: String },
    /// Filtered out by the memory policy, an unknown shape, or invalid JSON.
    Dropped,
}

/// One transcript-worthy item, in a structured form before rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedItem {
    Message {
        /// Lowercased role string ("user", "assistant", ...). "developer"
        /// is never produced here — that variant lands in `Dropped`.
        role: String,
        text: String,
    },
    ToolCall {
        name: String,
        arguments: String,
        call_id: String,
    },
    ToolResult {
        call_id: Option<String>,
        output: String,
    },
    /// Any of the policy-kept-but-shape-varied tool variants: local_shell_call,
    /// tool_search_call/output, custom_tool_call/output, web_search_call. If
    /// best-effort text extraction (`text`/`input`/`query`/`execution`/`content`)
    /// yielded a non-empty string, it's `Some`; otherwise `None` (rendered as
    /// a `[tool: <kind>]` placeholder).
    ToolMisc {
        kind: String,
        text: Option<String>,
    },
}

/// One contiguous range of `(line_no, payload)` pairs bounded by either a
/// top-level `"compacted"` marker or the start/end of the input. `transcript`
/// already has its lines joined with `\n` and is ready to feed an LLM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start_line_no: i64,
    pub end_line_no: i64,
    /// `Some(summary)` iff this segment is preceded by a compaction marker
    /// (the marker's `payload.message`). `None` for the first segment of a
    /// thread that hasn't been compacted at the start.
    pub prior_summary: Option<String>,
    pub transcript: String,
}

/// Classify one raw_event payload string. Never panics — any error path
/// (invalid JSON, missing field, unknown shape) returns `Dropped`.
pub fn parse_line(payload: &str) -> ParsedLine {
    let v: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return ParsedLine::Dropped,
    };
    let top_type = match v.get("type").and_then(|x| x.as_str()) {
        Some(t) => t,
        None => return ParsedLine::Dropped,
    };
    match top_type {
        // Top-level RolloutItem::Compacted — codex's segment marker.
        // Distinct from the *inner* `response_item/compaction` (which is
        // an encrypted-internal marker and gets Dropped below).
        "compacted" => {
            let summary = v
                .get("payload")
                .and_then(|p| p.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            ParsedLine::Boundary { summary }
        }
        "response_item" => parse_response_item(v.get("payload")),
        // session_meta / turn_context / event_msg / unknown → not transcript.
        _ => ParsedLine::Dropped,
    }
}

fn parse_response_item(payload: Option<&Value>) -> ParsedLine {
    let p = match payload {
        Some(p) => p,
        None => return ParsedLine::Dropped,
    };
    let inner = match p.get("type").and_then(|x| x.as_str()) {
        Some(t) => t,
        None => return ParsedLine::Dropped,
    };
    match inner {
        "message" => parse_message(p),
        "function_call" => parse_function_call(p),
        "function_call_output" => parse_function_call_output(p),
        // Policy-kept tool variants whose shapes vary — best-effort text
        // extraction; placeholder if none yields anything useful.
        "local_shell_call"
        | "tool_search_call"
        | "tool_search_output"
        | "custom_tool_call"
        | "custom_tool_call_output"
        | "web_search_call" => parse_tool_misc(p, inner),
        // Dropped per `should_persist_response_item_for_memories`
        // (codex-cli rollout/src/policy.rs:46).
        "reasoning" | "compaction" | "context_compaction" | "image_generation_call" => {
            ParsedLine::Dropped
        }
        _ => ParsedLine::Dropped,
    }
}

fn parse_message(p: &Value) -> ParsedLine {
    let role = match p.get("role").and_then(|x| x.as_str()) {
        Some(r) => r,
        None => return ParsedLine::Dropped,
    };
    if role == "developer" {
        return ParsedLine::Dropped;
    }
    let mut buf = String::new();
    if let Some(arr) = p.get("content").and_then(|v| v.as_array()) {
        for item in arr {
            let kind = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if matches!(kind, "input_text" | "output_text") {
                if let Some(text) = item.get("text").and_then(|x| x.as_str()) {
                    buf.push_str(text);
                }
            }
        }
    }
    if buf.trim().is_empty() {
        return ParsedLine::Dropped;
    }
    ParsedLine::Kept(RenderedItem::Message {
        role: role.to_lowercase(),
        text: buf,
    })
}

fn parse_function_call(p: &Value) -> ParsedLine {
    let name = p
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let arguments = p
        .get("arguments")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let call_id = p
        .get("call_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() && arguments.is_empty() {
        return ParsedLine::Dropped;
    }
    ParsedLine::Kept(RenderedItem::ToolCall {
        name,
        arguments,
        call_id,
    })
}

fn parse_function_call_output(p: &Value) -> ParsedLine {
    let call_id = p
        .get("call_id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    // `output` may be either a bare string (legacy) or an object with
    // `content` (string) or `content_items` (array of `{type, text}`).
    let text = match p.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(out) => {
            if let Some(content) = out.get("content").and_then(|v| v.as_str()) {
                content.to_string()
            } else if let Some(arr) = out.get("content_items").and_then(|v| v.as_array()) {
                arr.iter()
                    .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                    .collect::<Vec<&str>>()
                    .join("")
            } else {
                String::new()
            }
        }
        None => String::new(),
    };
    ParsedLine::Kept(RenderedItem::ToolResult {
        call_id,
        output: text,
    })
}

fn parse_tool_misc(p: &Value, kind: &str) -> ParsedLine {
    // Best-effort: try the few keys that variants in this group actually
    // use to carry textual content. Anything missing → Some(None) →
    // placeholder render.
    let text = ["text", "input", "query", "execution", "content"]
        .iter()
        .find_map(|key| p.get(key).and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    ParsedLine::Kept(RenderedItem::ToolMisc {
        kind: kind.to_string(),
        text,
    })
}

/// Render one Kept item to a transcript line. Returns `None` when the
/// rendered text would be entirely whitespace (per the "空文本的 Kept 跳过"
/// rule).
fn render_item(item: &RenderedItem) -> Option<String> {
    match item {
        RenderedItem::Message { role, text } => {
            if text.trim().is_empty() {
                None
            } else {
                Some(format!("{}: {}", role.to_uppercase(), text))
            }
        }
        RenderedItem::ToolCall {
            name, arguments, ..
        } => {
            if name.trim().is_empty() && arguments.trim().is_empty() {
                None
            } else {
                let n = if name.is_empty() { "<unknown>" } else { name };
                Some(format!("TOOL CALL {}: {}", n, arguments))
            }
        }
        RenderedItem::ToolResult { output, .. } => {
            if output.trim().is_empty() {
                None
            } else {
                Some(format!("TOOL RESULT: {}", output))
            }
        }
        RenderedItem::ToolMisc { kind, text } => match text {
            Some(t) if !t.trim().is_empty() => {
                Some(format!("TOOL {}: {}", kind.to_uppercase(), t))
            }
            _ => Some(format!("[tool: {}]", kind)),
        },
    }
}

/// Segment a thread's raw_event lines on top-level `"type":"compacted"`
/// boundaries. Each segment has a closed `[start_line_no, end_line_no]`
/// range (line numbers are 1-based per the S2 ingester's contract) and a
/// transcript built by rendering every Kept line, joined by `\n`. Dropped
/// lines occupy line_no slots in the range but contribute no transcript.
pub fn segment_thread(lines: &[(i64, &str)]) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let mut start_line: Option<i64> = None;
    let mut pending_prior: Option<String> = None;

    for &(line_no, payload) in lines {
        if start_line.is_none() {
            start_line = Some(line_no);
        }
        match parse_line(payload) {
            ParsedLine::Kept(item) => {
                if let Some(s) = render_item(&item) {
                    buf.push(s);
                }
            }
            ParsedLine::Boundary { summary } => {
                // Close the current segment. `end_line_no` is the boundary
                // line itself — per spec: "Boundary 计入前段 end".
                segments.push(Segment {
                    start_line_no: start_line.expect("start_line set above"),
                    end_line_no: line_no,
                    prior_summary: pending_prior.take(),
                    transcript: buf.join("\n"),
                });
                buf.clear();
                start_line = None;
                pending_prior = Some(summary);
            }
            ParsedLine::Dropped => {
                // Stays inside the current segment's line range; just no
                // transcript contribution. Don't touch buf or start_line.
            }
        }
    }

    // Trailing segment: only if there's at least one line after the last
    // boundary. The "末行即 compacted → 无末尾空段" rule falls out of this
    // naturally because the boundary's iteration resets `start_line = None`.
    if let Some(start) = start_line {
        let end = lines.last().map(|(n, _)| *n).unwrap_or(start);
        segments.push(Segment {
            start_line_no: start,
            end_line_no: end,
            prior_summary: pending_prior,
            transcript: buf.join("\n"),
        });
    }

    segments
}

// ---------------------------------------------------------------------------
// Phase 6 Step 3b — distillation extractor (LLM client + prompt + trait)
// ---------------------------------------------------------------------------

/// One distilled knowledge point — the unit the LLM returns and what
/// eventually lands in the `log` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KnowledgePoint {
    pub summary: String,
    pub detail: Option<String>,
    /// One of "decision" | "failure" | "pattern" | "fact" (normalised; any
    /// other value the LLM emits is coerced to "fact").
    pub kind: String,
}

/// Errors from one `Extractor::extract` call. `is_retryable()` separates
/// transient network blips from permanent failures so the orchestrator can
/// decide whether to back off and try again or surface the error.
#[derive(Debug)]
pub enum ExtractError {
    /// Configuration is missing or invalid (env vars unset, malformed URL,
    /// etc.). Operator intervention required — do not retry.
    Config(String),
    /// Network failure or 5xx / 408 / 429 from the upstream — almost always
    /// transient. **Retryable.**
    HttpTransient(String),
    /// 4xx from the upstream other than 408 / 429 (auth, bad request, …).
    /// The exact same request will fail again — not retryable.
    HttpClient(String),
    /// We got a 2xx but couldn't make sense of the body. Same input would
    /// likely re-produce the same garbage, but the orchestrator may
    /// surface it for inspection.
    Parse(String),
}

impl ExtractError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, ExtractError::HttpTransient(_))
    }
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::Config(m) => write!(f, "distill config error: {m}"),
            ExtractError::HttpTransient(m) => write!(f, "distill http transient: {m}"),
            ExtractError::HttpClient(m) => write!(f, "distill http client error: {m}"),
            ExtractError::Parse(m) => write!(f, "distill parse error: {m}"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Errors from one `Embedder::embed` call. Same retry semantics as
/// `ExtractError`: only `HttpTransient` is retryable.
#[derive(Debug, Clone)]
pub enum EmbedError {
    /// Configuration is missing or invalid (env / file). Not retryable.
    Config(String),
    /// Network failure or 5xx / 408 / 429 — almost always transient. **Retryable.**
    HttpTransient(String),
    /// 4xx other than 408/429 — same input will fail again. Not retryable.
    HttpClient(String),
    /// 2xx but the body was malformed, or a returned embedding had the
    /// wrong dimensionality. Not retryable from our side.
    Parse(String),
}

impl EmbedError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, EmbedError::HttpTransient(_))
    }
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Config(m) => write!(f, "embed config error: {m}"),
            EmbedError::HttpTransient(m) => write!(f, "embed http transient: {m}"),
            EmbedError::HttpClient(m) => write!(f, "embed http client error: {m}"),
            EmbedError::Parse(m) => write!(f, "embed parse error: {m}"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Embedding contract. Returns one vector per input text, in the same
/// order. Vectors must all be the same length and match `dimensions()`.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    fn dimensions(&self) -> usize;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Distillation contract. Tests mock with simple `impl Extractor`s; the
/// orchestrator owns one trait object so it can flip extractors per pass
/// (e.g., a "dry-run" no-op extractor for diagnostics).
#[async_trait::async_trait]
pub trait Extractor: Send + Sync {
    async fn extract(
        &self,
        transcript: &str,
        prior_summary: Option<&str>,
    ) -> Result<Vec<KnowledgePoint>, ExtractError>;
}

/// OpenAI-compatible `/v1/chat/completions` POST. All knobs come from
/// env so the orchestrator can flip provider without recompiling.
pub struct HttpExtractor {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl HttpExtractor {
    /// Build from `OPENCRAB_DISTILLER_{BASE_URL,MODEL,API_KEY}`. Returns
    /// `None` if any is missing or empty — the orchestrator treats this
    /// as "distillation disabled" rather than an error (clean local
    /// development).
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("OPENCRAB_DISTILLER_BASE_URL").ok()?;
        let model = std::env::var("OPENCRAB_DISTILLER_MODEL").ok()?;
        let api_key = std::env::var("OPENCRAB_DISTILLER_API_KEY").ok()?;
        if base_url.trim().is_empty() || model.trim().is_empty() || api_key.trim().is_empty() {
            return None;
        }
        Some(Self {
            client: reqwest::Client::new(),
            base_url,
            model,
            api_key,
        })
    }

    /// Layered credential resolution.
    ///
    /// 1. If all three `OPENCRAB_DISTILLER_*` env vars are present
    ///    (non-empty), use them.
    /// 2. Otherwise, try `<user_root>/distiller.json` — a JSON file with
    ///    `{ "base_url", "model", "api_key" }`. The path is the same
    ///    `~/.opencrab/` root the rest of the bin uses, so this stays
    ///    outside the repo working tree by construction.
    /// 3. Otherwise, `None` (distillation disabled).
    ///
    /// We try env first so an operator can override the file for a
    /// single run without rewriting it. The file fallback exists because
    /// the bin is spawned by codex-cli; shell env vars are not
    /// guaranteed to propagate through that spawn chain.
    pub fn load() -> Option<Self> {
        if let Some(ext) = Self::from_env() {
            return Some(ext);
        }
        Self::from_file()
    }

    fn from_file() -> Option<Self> {
        let root = crate::paths::user_root()?;
        Self::from_file_at(&root.join("distiller.json"))
    }

    fn from_file_at(path: &std::path::Path) -> Option<Self> {
        let body = std::fs::read_to_string(path).ok()?;
        let json: serde_json::Value = serde_json::from_str(&body).ok()?;
        let base_url = json
            .get("base_url")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let model = json
            .get("model")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let api_key = json
            .get("api_key")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        if base_url.is_empty() || model.is_empty() || api_key.is_empty() {
            return None;
        }
        Some(Self {
            client: reqwest::Client::new(),
            base_url,
            model,
            api_key,
        })
    }

    /// Final URL for the POST. Pure; split out so tests can assert the
    /// `/v1` segment is **not** double-injected. `base_url` is expected to
    /// already include the provider's API version segment (e.g.
    /// `https://api.openai.com/v1`, `https://dashscope.aliyuncs.com/compatible-mode/v1`).
    fn chat_completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }

    /// Pure: assemble the chat/completions request body. Split out for
    /// test-time inspection without sending.
    fn build_request_body(
        &self,
        transcript: &str,
        prior_summary: Option<&str>,
    ) -> serde_json::Value {
        let mut user_block = String::new();
        if let Some(prior) = prior_summary {
            user_block.push_str("[PRIOR CONTEXT SUMMARY]\n");
            user_block.push_str(prior);
            user_block.push_str("\n\n");
        }
        user_block.push_str("[TRANSCRIPT]\n");
        user_block.push_str(transcript);
        serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": EXTRACTION_PROMPT},
                {"role": "user", "content": user_block}
            ],
            // Low but non-zero — we want enough determinism that re-runs
            // on the same segment give similar output, but not so locked
            // that the model can't paraphrase a clearer summary.
            "temperature": 0.2,
        })
    }
}

#[async_trait::async_trait]
impl Extractor for HttpExtractor {
    async fn extract(
        &self,
        transcript: &str,
        prior_summary: Option<&str>,
    ) -> Result<Vec<KnowledgePoint>, ExtractError> {
        let url = self.chat_completions_url();
        let body = self.build_request_body(transcript, prior_summary);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ExtractError::HttpTransient(format!("send {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            // 5xx + 408 (timeout) + 429 (rate limit) → transient. Everything
            // else 4xx is "the request itself is wrong"; retrying won't help.
            let is_transient = status.is_server_error()
                || status.as_u16() == 408
                || status.as_u16() == 429;
            return if is_transient {
                Err(ExtractError::HttpTransient(format!(
                    "HTTP {status}: {body_text}"
                )))
            } else {
                Err(ExtractError::HttpClient(format!(
                    "HTTP {status}: {body_text}"
                )))
            };
        }

        let resp_body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ExtractError::Parse(format!("decode response body: {e}")))?;

        let content = resp_body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| {
                ExtractError::Parse(format!(
                    "missing choices[0].message.content in response: {resp_body}"
                ))
            })?;

        parse_knowledge_points(content).map_err(ExtractError::Parse)
    }
}

/// Tolerant parser for the LLM's reply.
///
/// Defends against three common deviations from "pure JSON array":
/// 1. wrapped in a ```json … ``` markdown fence (or unlabeled ``` … ```);
/// 2. surrounded by prose ("Here is the JSON: […]. Hope this helps!");
/// 3. invalid `kind` values / missing optional fields.
///
/// Returns `Err(String)` only for shapes we can't recover from (no '['
/// anywhere; outer is not a JSON array; entirely non-JSON between the
/// brackets). An empty array is `Ok(vec![])`. Per-element corruption
/// (empty / missing `summary`) results in that element being SILENTLY
/// SKIPPED — the orchestrator prefers ingesting a partial good batch
/// over discarding a whole pass.
pub fn parse_knowledge_points(s: &str) -> Result<Vec<KnowledgePoint>, String> {
    // Qwen3-thinking-style models emit `<think>...</think>` before the
    // actual answer. The think text often contains `[`/`]` which would
    // mislead `extract_json_array_slice`, so strip paired blocks first.
    let unthought = strip_think_blocks(s);
    let unfenced = strip_code_fence(&unthought);
    let sliced = extract_json_array_slice(unfenced)?;
    let value: serde_json::Value = serde_json::from_str(sliced)
        .map_err(|e| format!("invalid JSON: {e} (after fence-strip + bracket-slice)"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| "expected top-level JSON array".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let summary = item
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if summary.is_empty() {
            continue;
        }
        let detail = item.get("detail").and_then(|v| match v {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) if s.is_empty() => None,
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        });
        let kind_raw = item.get("kind").and_then(|v| v.as_str()).unwrap_or("fact");
        let kind = normalize_kind(kind_raw);
        out.push(KnowledgePoint {
            summary,
            detail,
            kind,
        });
    }
    Ok(out)
}

/// Remove every paired `<think>…</think>` block from `s`, non-greedy.
/// Each `<think>` is matched to its first following `</think>`. An
/// unclosed `<think>` (no terminator after it) is left untouched —
/// the downstream parser will fail naturally on the residual gibberish,
/// which is the right outcome because there can't be a valid JSON array
/// after an open think tag anyway.
fn strip_think_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        match rest.find("<think>") {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(start) => {
                let after_open = &rest[start + "<think>".len()..];
                match after_open.find("</think>") {
                    Some(end) => {
                        // Paired: drop `<think>…</think>` entirely.
                        out.push_str(&rest[..start]);
                        rest = &after_open[end + "</think>".len()..];
                    }
                    None => {
                        // Unclosed: keep the rest verbatim and bail.
                        out.push_str(rest);
                        return out;
                    }
                }
            }
        }
    }
}

fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    // ```json\n…\n```
    if let Some(rest) = t.strip_prefix("```json") {
        let rest = rest.trim_start_matches(|c: char| c == '\n' || c == '\r');
        if let Some(without_close) = rest.trim_end().strip_suffix("```") {
            return without_close;
        }
    }
    // ```\n…\n```
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches(|c: char| c == '\n' || c == '\r');
        if let Some(without_close) = rest.trim_end().strip_suffix("```") {
            return without_close;
        }
    }
    t
}

fn extract_json_array_slice(s: &str) -> Result<&str, String> {
    let start = s
        .find('[')
        .ok_or_else(|| "no '[' found in extractor reply".to_string())?;
    let end = s
        .rfind(']')
        .ok_or_else(|| "no ']' found in extractor reply".to_string())?;
    if end < start {
        return Err("array brackets out of order in extractor reply".to_string());
    }
    Ok(&s[start..=end])
}

fn normalize_kind(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    match lower.as_str() {
        "decision" | "failure" | "pattern" | "fact" => lower,
        _ => "fact".to_string(),
    }
}

/// The system prompt for the distiller. Adapted from Mem0's
/// `ADDITIVE_EXTRACTION_PROMPT` (research/mem0/mem0/configs/prompts.py:468)
/// — same skeleton (Role + categories + integrity rules + output format +
/// one few-shot), reframed from consumer-personalization to agent-work-
/// memory (see S3-R Q5.3).
///
/// **All adaptations are visible here**; keep them at this single site so
/// future prompt iteration shows up as a diff on this constant.
pub const EXTRACTION_PROMPT: &str = r#"# ROLE

You are a Memory Extractor for an AI software-engineering agent's work log. The agent has just spent a thread debugging, designing, and editing code — sometimes its own, sometimes another agent's. Your job is to extract DURABLE, REUSABLE knowledge from that thread: what should the next instance of this agent (or another agent picking up the same work) carry forward into future sessions?

# KINDS OF KNOWLEDGE (four)

- decision — a choice the agent or user made and the reason behind it. "Chose X over Y because Z."
- failure — something that broke, including the trigger condition and the fix. "X breaks under Y; resolved by Z."
- pattern — a reusable idiom inside THIS codebase / system. "The way to do K here is Z."
- fact — a non-obvious concrete fact about the code or system. "File P contains Q." "Service R requires S."

# INTEGRITY RULES (non-negotiable)

- No Echo. Don't restate the user's task assignment or the agent's question as a memory. The task itself is not a durable lesson. Performing an action is not itself a knowledge point: "I ran X", "I renamed the files", "the build succeeded", "the command exited 0" are NOT memories — unless the action came with a decision (and its reason), a surprise, a failure, or a durable fact about the system. A routine task that simply completed as expected leaves nothing to carry forward; output nothing for it.
- No Meta. Don't extract observations about the conversation ("the user asked", "the agent decided to..."). Extract the SUBSTANCE of what was learned, not the dialogue around it.
- Evidence-bound. Only extract claims directly supported by the [TRANSCRIPT]. Do not invent context, hypothesize causes, or paraphrase beyond what the messages and tool calls actually establish.
- Specific over generic. "TTL-based cache eviction with a 60s default in src/cache.rs" > "Cache invalidation is hard."
- Skip the trivial. Routine status pings, mid-decisions later revised, one-shot typos and their immediate corrections: not memories. A short, sharp list beats a long, padded one.

# PRIOR CONTEXT

If a [PRIOR CONTEXT SUMMARY] block is given, it summarises earlier work in the same thread that has since been compacted away by the model. Use it ONLY as background — to disambiguate references in the [TRANSCRIPT]. DO NOT extract knowledge points from it. Only extract from [TRANSCRIPT].

# OUTPUT FORMAT

Return a pure JSON array. Each element has exactly three fields:
- "summary": a single-sentence headline of the knowledge point.
- "detail": supporting context (exact error strings, file paths, code symbols, the workaround steps) as a string, OR null if the summary stands alone.
- "kind": one of "decision", "failure", "pattern", "fact".

Output ONLY the JSON array. No prose preamble, no closing remarks, no markdown code fences. If the transcript contains nothing durable, output [].

Soft cap: a typical segment yields 0–5 points. Favour quality over count — a short, sharp list beats a long, padded one. Never pad to hit a number.

# EXAMPLES

## Example 1 — failure

When the user block is:

[TRANSCRIPT]
USER: Run cargo test and see what happens.
ASSISTANT: I'll run the tests.
TOOL CALL bash: {"cmd":"cargo test"}
TOOL RESULT: thread 'tests::backend::s1_c1' panicked at 'SqliteFailure: duplicate column name: origin' at backend.rs:240
ASSISTANT: The v1 ALTER TABLE ran twice — that's not idempotent. I'll wrap the step in PRAGMA user_version gating and re-run.

Your output should be exactly:

[{"summary":"ALTER TABLE ADD COLUMN is not idempotent — rerunning the migration crashes with 'duplicate column name'","detail":"Gate schema changes on PRAGMA user_version; apply a step only when user_version < target and set user_version as the last statement inside the same IMMEDIATE transaction.","kind":"failure"}]

## Example 2 — decision

When the user block is:

[TRANSCRIPT]
USER: Should the ingester use spawn_blocking or switch the whole runtime to multi_thread?
ASSISTANT: We're on a current_thread tokio runtime to keep the binary small. Switching to multi_thread would touch the rest of the bin. spawn_blocking is the right escape hatch for the SQLite + file I/O pass — it offloads to the blocking thread pool without changing the runtime flavor.
ASSISTANT: I'll keep the runtime current_thread and wrap each ingest pass in spawn_blocking.

Your output should be exactly:

[{"summary":"Ingester runs in spawn_blocking on the current_thread runtime instead of switching to multi_thread","detail":"The memory-MCP process uses a current_thread tokio runtime; wrapping the blocking file+SQLite ingest pass in spawn_blocking keeps the MCP server responsive without changing the runtime flavor.","kind":"decision"}]

## Example 3 — nothing durable

When the user block is:

[TRANSCRIPT]
USER: thanks, that worked!
ASSISTANT: Glad it helped!

Your output should be exactly:

[]

## Example 4 — routine successful command, nothing durable

When the user block is:

[TRANSCRIPT]
USER: Get the dev environment running so I can build locally.
ASSISTANT: I'll install deps and build.
TOOL CALL exec_command: {"cmd":"make setup && make build"}
TOOL RESULT: dependencies installed
Build succeeded in 12.3s
ASSISTANT: Environment is ready — dependencies installed and the local build passes.

Your output should be exactly (the build merely succeeded — no decision, surprise, failure, or new fact):

[]

## Example 5 — mechanical task completed, nothing learned

When the user block is:

[TRANSCRIPT]
USER: Rename every markdown file under docs/ so the spaces become hyphens.
ASSISTANT: Sure, I'll rename them.
TOOL CALL exec_command: {"cmd":"rename 's/ /-/g' docs/*.md"}
TOOL RESULT: renamed 3 files
ASSISTANT: Done — renamed 3 files, all succeeded.

Your output should be exactly (a mechanical edit that just completed is an echo of the task, not a lesson):

[]
"#;

// ---------------------------------------------------------------------------
// Phase 6 Step 4-embed — embedding client + embed_pending_once
// ---------------------------------------------------------------------------

/// Default embedding dimensionality when the config file / env vars
/// don't say otherwise. Must match the `log_vec` table's declared
/// dimension (v6 migration: `float[1024]`).
pub const DEFAULT_EMBED_DIMENSIONS: usize = 1024;

/// SiliconFlow's documented batch cap. We never embed more than this
/// many texts per HTTP request.
pub const EMBED_BATCH_SIZE: usize = 32;

/// OpenAI-compatible `/embeddings` HTTP client. Shares `base_url` +
/// `api_key` with the distiller's `HttpExtractor`; `model` and
/// `dimensions` are embedding-specific.
pub struct HttpEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dimensions: usize,
}

impl HttpEmbedder {
    /// Env path: shared `OPENCRAB_DISTILLER_BASE_URL` + `OPENCRAB_DISTILLER_API_KEY`
    /// + embed-specific `OPENCRAB_EMBED_MODEL` + optional `OPENCRAB_EMBED_DIMENSIONS`
    /// (defaults to [`DEFAULT_EMBED_DIMENSIONS`]). Returns `None` if any
    /// required var is missing or empty.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("OPENCRAB_DISTILLER_BASE_URL").ok()?;
        let api_key = std::env::var("OPENCRAB_DISTILLER_API_KEY").ok()?;
        let model = std::env::var("OPENCRAB_EMBED_MODEL").ok()?;
        let dimensions = std::env::var("OPENCRAB_EMBED_DIMENSIONS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_EMBED_DIMENSIONS);
        if base_url.trim().is_empty()
            || api_key.trim().is_empty()
            || model.trim().is_empty()
        {
            return None;
        }
        Some(Self {
            client: reqwest::Client::new(),
            base_url,
            api_key,
            model,
            dimensions,
        })
    }

    /// Env first; otherwise read `<user_root>/distiller.json` and pick
    /// up `embed_model` (required) + `embed_dimensions` (default 1024).
    /// `base_url` + `api_key` are shared with the distiller config
    /// already in that same file.
    pub fn load() -> Option<Self> {
        if let Some(emb) = Self::from_env() {
            return Some(emb);
        }
        Self::from_file()
    }

    fn from_file() -> Option<Self> {
        let root = crate::paths::user_root()?;
        Self::from_file_at(&root.join("distiller.json"))
    }

    fn from_file_at(path: &std::path::Path) -> Option<Self> {
        let body = std::fs::read_to_string(path).ok()?;
        let json: serde_json::Value = serde_json::from_str(&body).ok()?;
        let base_url = json
            .get("base_url")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let api_key = json
            .get("api_key")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let model = json
            .get("embed_model")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let dimensions = json
            .get("embed_dimensions")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_EMBED_DIMENSIONS);
        if base_url.is_empty() || api_key.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            client: reqwest::Client::new(),
            base_url,
            api_key,
            model,
            dimensions,
        })
    }

    /// Final POST URL. Like the distiller, `base_url` must already
    /// include the provider's `/v1` segment — we only append `/embeddings`.
    fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }

    fn build_request_body(&self, texts: &[String]) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "input": texts,
            "dimensions": self.dimensions,
        })
    }
}

#[async_trait::async_trait]
impl Embedder for HttpEmbedder {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let url = self.embeddings_url();
        let body = self.build_request_body(texts);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbedError::HttpTransient(format!("send {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            let is_transient = status.is_server_error()
                || status.as_u16() == 408
                || status.as_u16() == 429;
            return if is_transient {
                Err(EmbedError::HttpTransient(format!(
                    "HTTP {status}: {body_text}"
                )))
            } else {
                Err(EmbedError::HttpClient(format!(
                    "HTTP {status}: {body_text}"
                )))
            };
        }

        let resp_body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EmbedError::Parse(format!("decode embed response body: {e}")))?;

        parse_embeddings_response(&resp_body, texts.len(), self.dimensions)
    }
}

/// Pure: pull the per-input vectors out of a `/v1/embeddings` reply,
/// re-order them by `data[i].index`, and verify each vector has
/// length == `expected_dim`. Split out so unit tests can hammer it
/// without going through HTTP.
fn parse_embeddings_response(
    body: &serde_json::Value,
    expected_count: usize,
    expected_dim: usize,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let arr = body
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| EmbedError::Parse(format!("missing data[] in: {body}")))?;
    if arr.len() != expected_count {
        return Err(EmbedError::Parse(format!(
            "data length {} != expected_count {}",
            arr.len(),
            expected_count
        )));
    }

    // Pull (index, embedding) out of each element; the API doesn't
    // guarantee `data[]` is sorted by index, so we sort ourselves.
    let mut pairs: Vec<(usize, Vec<f32>)> = Vec::with_capacity(arr.len());
    for item in arr {
        let idx = item
            .get("index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| EmbedError::Parse(format!("missing index in: {item}")))?
            as usize;
        let emb_arr = item
            .get("embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| EmbedError::Parse(format!("missing embedding in: {item}")))?;
        if emb_arr.len() != expected_dim {
            return Err(EmbedError::Parse(format!(
                "vector length {} != expected_dim {} (index={idx})",
                emb_arr.len(),
                expected_dim
            )));
        }
        let v: Vec<f32> = emb_arr
            .iter()
            .map(|n| {
                n.as_f64()
                    .map(|x| x as f32)
                    .ok_or_else(|| {
                        EmbedError::Parse(format!("non-numeric embedding element: {n}"))
                    })
            })
            .collect::<Result<Vec<f32>, EmbedError>>()?;
        pairs.push((idx, v));
    }
    pairs.sort_by_key(|(i, _)| *i);
    // After sort + length match, each index 0..N must appear exactly once.
    for (i, (idx, _)) in pairs.iter().enumerate() {
        if *idx != i {
            return Err(EmbedError::Parse(format!(
                "non-contiguous indices: position {i} has data.index = {idx}"
            )));
        }
    }
    Ok(pairs.into_iter().map(|(_, v)| v).collect())
}

/// Serialise a `[f32]` to the JSON-array form vec0's MATCH expects.
/// Pure helper; also used by tests.
fn vec_to_match_json(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}

/// Embedding-input text for one log row. The S4-R recon recommended
/// `summary + "\n\n" + detail`: the headline plus body together gives
/// the embedder maximum signal, and rows with `detail IS NULL` still
/// produce a usable text (just `summary` + empty trailer).
fn build_embed_text(summary: &str, detail: Option<&str>) -> String {
    format!("{}\n\n{}", summary, detail.unwrap_or(""))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmbedStats {
    pub rows_pending_seen: u64,
    pub rows_embedded: u64,
    pub batches: u64,
    pub errors: u64,
}

/// One embedding pass.
///
/// Loops over batches of ≤ [`EMBED_BATCH_SIZE`] `log` rows that lack a
/// matching `log_vec` row, embeds each batch through `embedder`, and
/// writes the result into `log_vec(rowid = log.id)` inside one IMMEDIATE
/// transaction per batch. On the first embedder error this pass stops
/// (rows stay pending for the next pass); no partial writes.
///
/// We track an in-pass `min_id` watermark so empty-content rows (which
/// can't happen via `log_progress` / distiller writes today but might
/// arrive via direct INSERT in some future path) don't cause an
/// infinite loop — every batch advances the watermark even if all its
/// rows got skipped.
pub async fn embed_pending_once(
    conn: &Connection,
    embedder: &dyn Embedder,
) -> Result<EmbedStats, BackendError> {
    let mut stats = EmbedStats::default();
    let mut min_id: i64 = 0;

    loop {
        let batch = fetch_pending_batch_after(conn, min_id, EMBED_BATCH_SIZE)?;
        if batch.is_empty() {
            return Ok(stats);
        }
        let new_max = batch.last().expect("non-empty").0;
        stats.rows_pending_seen += batch.len() as u64;

        // Partition: rows with usable text vs rows we silently skip.
        let mut ids: Vec<i64> = Vec::with_capacity(batch.len());
        let mut texts: Vec<String> = Vec::with_capacity(batch.len());
        for (id, summary, detail) in &batch {
            let text = build_embed_text(summary, detail.as_deref());
            if text.trim().is_empty() {
                continue;
            }
            ids.push(*id);
            texts.push(text);
        }

        if !texts.is_empty() {
            let vectors = match embedder.embed(&texts).await {
                Ok(v) => v,
                Err(err) => {
                    eprintln!(
                        "[embed] batch (rows {}..={}) failed: {err} — stopping pass, rows stay pending",
                        ids.first().copied().unwrap_or(0),
                        ids.last().copied().unwrap_or(0)
                    );
                    stats.errors += 1;
                    return Ok(stats);
                }
            };
            if vectors.len() != texts.len() {
                eprintln!(
                    "[embed] embedder returned {} vectors for {} texts — stopping pass",
                    vectors.len(),
                    texts.len()
                );
                stats.errors += 1;
                return Ok(stats);
            }
            write_embedding_batch(conn, &ids, &vectors)?;
            stats.rows_embedded += vectors.len() as u64;
            stats.batches += 1;
        }

        min_id = new_max;
    }
}

fn fetch_pending_batch_after(
    conn: &Connection,
    after_id: i64,
    limit: usize,
) -> Result<Vec<(i64, String, Option<String>)>, BackendError> {
    let mut stmt = conn.prepare(
        "SELECT id, summary, detail FROM log \
         WHERE id NOT IN (SELECT rowid FROM log_vec) AND id > ?1 \
         ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after_id, limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn write_embedding_batch(
    conn: &Connection,
    ids: &[i64],
    vectors: &[Vec<f32>],
) -> Result<(), BackendError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let inner = (|| -> Result<(), BackendError> {
        for (id, v) in ids.iter().zip(vectors.iter()) {
            let json = vec_to_match_json(v);
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, json],
            )?;
        }
        Ok(())
    })();
    match inner {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 6 Step 3-distill — orchestration loop (distill_once)
// ---------------------------------------------------------------------------

/// A thread becomes a distillation candidate when its last growth event is
/// older than this — long enough that the agent has plausibly stopped
/// adding to the thread and we can compress a finished segment safely.
pub const DISTILL_IDLE_MS: i64 = 120_000;

/// Hard safety valve: if the un-distilled tail of a thread grows beyond
/// this many lines, distill even if the thread isn't "idle". Prevents one
/// hot thread from accumulating an unbounded backlog.
pub const DISTILL_MAX_PENDING: i64 = 400;

/// Per-segment transcript char budget fed to the LLM. Older content
/// (head of segment) is dropped first; the truncation point is marked
/// with `[...earlier content truncated...]`.
pub const DISTILL_MAX_TRANSCRIPT_CHARS: usize = 48_000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DistillStats {
    pub threads_seen: u64,
    pub threads_triggered: u64,
    pub segments_processed: u64,
    pub points_written: u64,
    pub transient_errors: u64,
    pub client_errors: u64,
    pub parse_skips: u64,
}

#[derive(Debug, Clone)]
struct DistillCandidate {
    thread_id: String,
    project_hash: Option<String>,
    last_line_no: i64,
    last_distilled_line_no: i64,
    last_growth_ts: Option<i64>,
}

/// One distillation pass. `now_ms` is injected so tests can fake "the
/// thread has been idle for N ms" without sleeping. The pass takes
/// `&Connection` (long-lived from the caller) and `&dyn Extractor` so
/// real-LLM and fake extractors share one code path.
pub async fn distill_once(
    conn: &Connection,
    extractor: &dyn Extractor,
    now_ms: i64,
) -> Result<DistillStats, BackendError> {
    let mut stats = DistillStats::default();
    let candidates = read_distill_candidates(conn)?;
    stats.threads_seen = candidates.len() as u64;

    for cand in candidates {
        let idle = match cand.last_growth_ts {
            Some(growth) => now_ms - growth > DISTILL_IDLE_MS,
            None => false,
        };
        let pending = cand.last_line_no - cand.last_distilled_line_no;
        let fire = idle || pending > DISTILL_MAX_PENDING;
        if !fire {
            continue;
        }
        stats.threads_triggered += 1;

        let rows = read_distill_lines(
            conn,
            &cand.thread_id,
            cand.last_distilled_line_no,
            cand.last_line_no,
        )?;
        if rows.is_empty() {
            // Cursor + last_line_no disagree with raw_event reality; just
            // skip — next pass after an ingest reconciles.
            continue;
        }
        let lines_ref: Vec<(i64, &str)> = rows.iter().map(|(n, p)| (*n, p.as_str())).collect();
        let segments = segment_thread(&lines_ref);

        for segment in segments {
            stats.segments_processed += 1;

            let transcript_str = truncate_transcript(&segment.transcript);
            if transcript_str.trim().is_empty() {
                // All-Dropped segment: no LLM call, just slide the cursor
                // past so we never look at these lines again.
                advance_distill_cursor(conn, &cand.thread_id, segment.end_line_no)?;
                continue;
            }

            let result = extractor
                .extract(&transcript_str, segment.prior_summary.as_deref())
                .await;
            match result {
                Ok(kps) => {
                    let n = kps.len() as u64;
                    write_distilled_segment(conn, &cand, &kps, segment.end_line_no, now_ms)?;
                    stats.points_written += n;
                }
                Err(ExtractError::HttpTransient(msg)) => {
                    eprintln!(
                        "[distill] thread={} transient: {msg} — leaving cursor at {} for next pass",
                        cand.thread_id, cand.last_distilled_line_no
                    );
                    stats.transient_errors += 1;
                    break; // don't advance cursor; next pass retries this segment
                }
                Err(ExtractError::HttpClient(msg)) => {
                    eprintln!(
                        "[distill] thread={} client error (won't retry on same input): {msg}",
                        cand.thread_id
                    );
                    stats.client_errors += 1;
                    break; // same as transient: cursor stays put for now
                }
                Err(ExtractError::Parse(msg)) => {
                    eprintln!(
                        "[distill] thread={} parse error on segment [{}-{}], skipping past: {msg}",
                        cand.thread_id, segment.start_line_no, segment.end_line_no
                    );
                    // ADVANCE the cursor past this segment so we don't
                    // loop on garbage. The next segment in the same
                    // thread still gets processed.
                    advance_distill_cursor(conn, &cand.thread_id, segment.end_line_no)?;
                    stats.parse_skips += 1;
                    continue;
                }
                Err(other @ ExtractError::Config(_)) => {
                    // Treat like client: stop this thread for the pass.
                    eprintln!(
                        "[distill] thread={} fatal config error: {other}",
                        cand.thread_id
                    );
                    stats.client_errors += 1;
                    break;
                }
            }
        }
    }

    Ok(stats)
}

fn read_distill_candidates(conn: &Connection) -> Result<Vec<DistillCandidate>, BackendError> {
    let mut stmt = conn.prepare(
        "SELECT thread_id, project_hash, last_line_no, last_distilled_line_no, last_growth_ts \
         FROM raw_thread \
         WHERE last_distilled_line_no < last_line_no \
         ORDER BY thread_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(DistillCandidate {
            thread_id: r.get(0)?,
            project_hash: r.get(1)?,
            last_line_no: r.get(2)?,
            last_distilled_line_no: r.get(3)?,
            last_growth_ts: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn read_distill_lines(
    conn: &Connection,
    thread_id: &str,
    after_line_no: i64,
    up_to_line_no: i64,
) -> Result<Vec<(i64, String)>, BackendError> {
    let mut stmt = conn.prepare(
        "SELECT line_no, payload FROM raw_event \
         WHERE thread_id = ?1 AND line_no > ?2 AND line_no <= ?3 \
         ORDER BY line_no",
    )?;
    let rows = stmt.query_map(
        params![thread_id, after_line_no, up_to_line_no],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Keep the **tail** of the transcript when it overflows the budget —
/// the LLM cares more about recent context than ancient setup. The
/// dropped head is replaced with a single marker line so the model
/// knows it's working with a trimmed segment.
fn truncate_transcript(t: &str) -> String {
    let total = t.chars().count();
    if total <= DISTILL_MAX_TRANSCRIPT_CHARS {
        return t.to_string();
    }
    let marker = "[...earlier content truncated...]\n";
    let marker_chars = marker.chars().count();
    let budget = DISTILL_MAX_TRANSCRIPT_CHARS.saturating_sub(marker_chars);
    let skip = total - budget;
    let tail: String = t.chars().skip(skip).collect();
    let mut out = String::with_capacity(marker.len() + tail.len());
    out.push_str(marker);
    out.push_str(&tail);
    out
}

/// IMMEDIATE-tx UPDATE of `raw_thread.last_distilled_line_no` only.
/// Used for empty-transcript segments and parse-error skip-past.
fn advance_distill_cursor(
    conn: &Connection,
    thread_id: &str,
    new_line_no: i64,
) -> Result<(), BackendError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = conn.execute(
        "UPDATE raw_thread SET last_distilled_line_no = ?1 WHERE thread_id = ?2",
        params![new_line_no, thread_id],
    );
    match result {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(BackendError::Sqlite(e))
        }
    }
}

/// IMMEDIATE-tx: insert every KP as a `log` row with `origin='distill'`,
/// then advance the cursor. All-or-nothing — if any insert fails the
/// cursor stays put and the next pass retries the segment.
fn write_distilled_segment(
    conn: &Connection,
    cand: &DistillCandidate,
    kps: &[KnowledgePoint],
    end_line_no: i64,
    now_ms: i64,
) -> Result<(), BackendError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let inner = (|| -> Result<(), BackendError> {
        for kp in kps {
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin, project_hash, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    now_ms,
                    &kp.summary,
                    &kp.detail,
                    "distill",
                    &cand.project_hash,
                    &kp.kind,
                ],
            )?;
        }
        conn.execute(
            "UPDATE raw_thread SET last_distilled_line_no = ?1 WHERE thread_id = ?2",
            params![end_line_no, &cand.thread_id],
        )?;
        Ok(())
    })();
    match inner {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// How often the production distill loop wakes up to scan for candidate
/// threads. Less frequent than the ingester poll (15 s) because the
/// distiller only does useful work after `DISTILL_IDLE_MS` of silence —
/// a 2-minute idle threshold paired with a 30-second poll is plenty.
pub const DISTILL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Production distill loop. Owns one long-lived [`Connection`] (WAL +
/// busy_timeout via [`open`]) and runs forever, polling every
/// [`DISTILL_POLL_INTERVAL`]. Errors are logged and the loop continues —
/// **this function never panics out of the loop**; the only early exit
/// path is the initial `open()` failure (when there's nothing the loop
/// could do).
///
/// Lives on a dedicated OS thread with its own current_thread tokio
/// runtime (set up by [`super::main`]). That decouples the LLM call's
/// async work from the MCP server's runtime — they don't compete on the
/// same executor, and the long-running blocking SQL paths inside
/// [`distill_once`] stay off the server's hot loop.
pub async fn run_distill_loop(
    memory_db: PathBuf,
    extractor: HttpExtractor,
    embedder: Option<HttpEmbedder>,
) {
    // One connection for the whole lifetime of the loop. Stays on this
    // thread; the rusqlite::Connection isn't Sync, so we never share it.
    let conn = match open(&memory_db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[distill] open failed, loop not starting: {e}");
            return;
        }
    };
    loop {
        match distill_once(&conn, &extractor, current_time_ms()).await {
            Ok(stats) => {
                // Quiet by default; chatter only when something interesting
                // happened so a happy-path log doesn't spam every 30 s.
                if stats.points_written > 0
                    || stats.transient_errors > 0
                    || stats.client_errors > 0
                    || stats.parse_skips > 0
                {
                    eprintln!("[distill] {stats:?}");
                }
            }
            Err(e) => eprintln!("[distill] pass failed: {e}"),
        }

        // Embedding is independent of distillation: even if the
        // distiller had a bad pass, the embedder still processes its
        // own pending queue. Failures are logged + the loop continues.
        if let Some(emb) = &embedder {
            match embed_pending_once(&conn, emb).await {
                Ok(stats) => {
                    if stats.rows_embedded > 0 || stats.errors > 0 {
                        eprintln!("[embed] {stats:?}");
                    }
                }
                Err(e) => eprintln!("[embed] pass failed: {e}"),
            }
        }

        tokio::time::sleep(DISTILL_POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Schema version this binary writes. The DB file records its current
/// schema in `PRAGMA user_version`; `migrate` advances it one step at a
/// time up to this value.
const SCHEMA_VERSION: i64 = 7;

/// Bring `conn`'s schema up to [`SCHEMA_VERSION`].
///
/// v0 baseline (every `IF NOT EXISTS`) runs unconditionally so a fresh DB
/// gets the initial objects; against an already-initialised DB it is a
/// no-op. After that we walk from `user_version + 1` up to
/// `SCHEMA_VERSION`, applying one version step per IMMEDIATE transaction.
/// A future binary opening a higher-versioned DB iterates over an empty
/// range and exits clean.
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
    for target in (current + 1)..=SCHEMA_VERSION {
        // Each step's `ALTER TABLE ADD COLUMN` (and any other non-idempotent
        // DDL) would crash the next open() on a `duplicate column name`-style
        // error if it ran twice. Wrap the whole step in one IMMEDIATE
        // transaction with the `user_version` bump as the last statement so
        // any mid-batch failure rolls back cleanly and the next open()
        // retries from the same starting version.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Re-read `user_version` under the write lock: guards the rare case
        // of two first-opens racing on the upgrade.
        let locked: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if locked < target {
            apply_migration_step(&tx, target)?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// One version step. Each branch ends with `PRAGMA user_version = N` so
/// the bump is the last statement in the batch — a mid-step failure rolls
/// back the version pin along with the DDL.
fn apply_migration_step(tx: &rusqlite::Transaction<'_>, target: i64) -> Result<(), BackendError> {
    match target {
        1 => tx.execute_batch(
            "ALTER TABLE log ADD COLUMN origin TEXT NOT NULL DEFAULT 'self';
             ALTER TABLE log ADD COLUMN project_hash TEXT;
             CREATE INDEX IF NOT EXISTS idx_log_ts ON log(ts);
             PRAGMA user_version = 1;",
        )?,
        2 => tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS raw_thread (
                 thread_id        TEXT PRIMARY KEY,
                 agent_id         TEXT,
                 team_id          TEXT,
                 project_hash     TEXT,
                 source           TEXT,
                 parent_thread_id TEXT,
                 cwd              TEXT,
                 source_path      TEXT NOT NULL,
                 first_seen_ts    INTEGER NOT NULL,
                 last_ingest_ts   INTEGER NOT NULL,
                 last_offset      INTEGER NOT NULL DEFAULT 0,
                 last_line_no     INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS raw_event (
                 id          INTEGER PRIMARY KEY,
                 thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
                 line_no     INTEGER NOT NULL,
                 payload     TEXT NOT NULL,
                 ingested_at INTEGER NOT NULL,
                 UNIQUE(thread_id, line_no)
             );
             CREATE INDEX IF NOT EXISTS idx_raw_event_thread ON raw_event(thread_id);
             PRAGMA user_version = 2;",
        )?,
        3 => tx.execute_batch(
            // S3-schema: distill cursor + growth timestamp.
            //
            // `last_distilled_line_no` (NOT NULL DEFAULT 0): how far the S3
            // distiller has consumed this thread. Sibling to `last_line_no`
            // (ingester's cursor); both move monotonically.
            //
            // `last_growth_ts` (nullable): timestamp of the last ingest pass
            // that actually inserted new raw_event rows. Distinct from
            // `last_ingest_ts`, which is bumped every pass (even no-op ones).
            // The distiller uses this to decide "anything new since I last
            // ran?" without having to re-read raw_event.
            "ALTER TABLE raw_thread ADD COLUMN last_distilled_line_no INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE raw_thread ADD COLUMN last_growth_ts INTEGER;
             PRAGMA user_version = 3;",
        )?,
        4 => tx.execute_batch(
            // S3-distill: classify distilled `log` rows by `kind`
            // (decision/failure/pattern/fact). Nullable on purpose:
            //   * existing user-written `log_progress` rows have no kind
            //     and we won't backfill;
            //   * `log_progress` signature stays unchanged — it still
            //     writes `kind = NULL`. Only the distiller writes a kind.
            // FTS triggers reference only `detail`, so adding `kind`
            // doesn't require an FTS rebuild.
            "ALTER TABLE log ADD COLUMN kind TEXT;
             PRAGMA user_version = 4;",
        )?,
        5 => tx.execute_batch(
            // S4-schema: per-row embeddings into a vec0 virtual table.
            // Dim 4096 is fixed at table-create time — picked from S4-R
            // (SiliconFlow Qwen3-Embedding-8B native dim). Rowid is
            // expected to equal `log.id` so queries can plain-JOIN.
            //
            // We never write to log_vec in this step — backfill +
            // per-row insert come in S4-impl. Empty virtual table is
            // a no-op for KNN (returns 0 rows) and for the existing
            // FTS-only search path.
            "CREATE VIRTUAL TABLE IF NOT EXISTS log_vec USING vec0(embedding float[4096]);
             PRAGMA user_version = 5;",
        )?,
        6 => tx.execute_batch(
            // v6: re-dimension log_vec from 4096 (Qwen3-Embedding-8B) to 1024
            // (BAAI/bge-m3). A vec0 table's dim is fixed at create time, so we
            // DROP + re-CREATE. DROP+CREATE of a vec0 virtual table runs inside
            // this IMMEDIATE migration tx exactly like the v5 CREATE did.
            // Lossless in practice: production has 0 memory.db files
            // (greenfield) and test DBs are temp; embed_pending_once re-embeds
            // every log row on the next pass (empty log_vec → all rows pending).
            "DROP TABLE IF EXISTS log_vec;
             CREATE VIRTUAL TABLE log_vec USING vec0(embedding float[1024]);
             PRAGMA user_version = 6;",
        )?,
        7 => tx.execute_batch(
            // v7: rebuild log_fts with the FTS5 `trigram` tokenizer so CJK
            // substrings become lexically matchable (the default unicode61
            // doesn't segment CJK → Chinese queries got 0 FTS hits and the
            // lexical half of hybrid search was dead for Chinese). Same
            // external-content config (content='log', content_rowid='id',
            // column `detail`); ONLY the tokenizer changes. The sync triggers
            // (log_ai/ad/au) reference log_fts by name and keep working after
            // the recreate — sync logic untouched. DROP+CREATE of an FTS5 table
            // runs inside this IMMEDIATE tx just like the v6 log_vec rebuild;
            // `'rebuild'` re-indexes every existing row from the content table.
            "DROP TABLE IF EXISTS log_fts;
             CREATE VIRTUAL TABLE log_fts USING fts5(
                 detail,
                 content='log',
                 content_rowid='id',
                 tokenize='trigram'
             );
             INSERT INTO log_fts(log_fts) VALUES('rebuild');
             PRAGMA user_version = 7;",
        )?,
        _ => unreachable!("no migration step defined for v{target} — add an arm"),
    }
    Ok(())
}

fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// FTS5 `trigram` tokenizer (set in the v7 migration) indexes/matches only
/// substrings of **>= 3 chars**. Terms shorter than this are unmatchable and
/// are dropped — those queries fall back to the vector path.
const FTS_TRIGRAM_MIN: usize = 3;

/// Non-overlapping width used to slice a long CJK run into bounded substring
/// terms. Deliberately NOT a per-character sliding 3-gram sweep: a 4-char
/// chunk like `会话状态` is specific, whereas OR-ing every sliding 3-gram
/// (`会话状`,`话状态`,`状态用`,…) would recall any row sharing a common 3-gram
/// and flood RRF with noise.
const FTS_CJK_CHUNK: usize = 4;

/// CJK Unified Ideographs (incl. Ext-A) — enough to detect Chinese runs in a
/// query; non-CJK alphanumerics (latin / digits) are matched whole.
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9FFF}')
}

fn push_fts_term(terms: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, raw: &str) {
    if !raw.is_empty() && seen.insert(raw.to_string()) {
        // Quote as an FTS5 string literal so query operators inside the term
        // are content, not syntax; the only in-literal escape is `"` → `""`.
        terms.push(format!("\"{}\"", raw.replace('"', "\"\"")));
    }
}

/// Turn a free-text query into an FTS5 `trigram` MATCH expression.
///
/// The index is trigram (substring match, >= 3 chars — see [`FTS_TRIGRAM_MIN`]),
/// so:
///   * non-CJK runs (English words >= 3 chars) are kept whole and substring-
///     match (e.g. `test` matches `testing`);
///   * CJK runs are sliced into NON-overlapping [`FTS_CJK_CHUNK`]-char blocks
///     (bounded segmentation, not a sliding 3-gram sweep) so each block is a
///     specific substring term;
///   * runs/blocks shorter than [`FTS_TRIGRAM_MIN`] are dropped (the vector
///     path is the backstop for 1–2 char CJK queries like `缓存`).
///
/// Blocks are split on any non-alphanumeric char (punctuation / whitespace; CJK
/// ideographs are alphanumeric, so a CJK run stays one block), then `"`-escaped,
/// quoted, de-duplicated, and OR-joined for recall (bm25 surfaces the best). A
/// query that yields no matchable term returns `None` (caller: "no FTS results").
fn build_fts_match(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for block in query.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = block.chars().collect();
        if chars.len() < FTS_TRIGRAM_MIN {
            continue;
        }
        if chars.iter().any(|&c| is_cjk(c)) {
            let mut i = 0;
            while i < chars.len() {
                let end = (i + FTS_CJK_CHUNK).min(chars.len());
                if end - i >= FTS_TRIGRAM_MIN {
                    let piece: String = chars[i..end].iter().collect();
                    push_fts_term(&mut terms, &mut seen, &piece);
                }
                i += FTS_CJK_CHUNK;
            }
        } else {
            push_fts_term(&mut terms, &mut seen, block);
        }
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
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

// V2-corpus end-to-end runner harness (Layer A mechanical / B distill / C
// search). Kept in its own file; declared here as a child of `backend` so it
// can reach crate-private items (`search_with_timeout`, `current_time_ms`,
// `parse_line`, …) via `use super::*`.
#[cfg(test)]
#[path = "corpus_tests.rs"]
mod corpus_tests;

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

        let hits = block_on(search(&db, None, "OAuth", 6)).unwrap();
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

        let hits = block_on(search(&db, None, "alpha beta", 6)).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, id_two, "two-term match must rank first");
        assert_eq!(hits[1].id, id_one);
    }

    #[test]
    fn search_returns_empty_for_query_with_no_searchable_terms() {
        let (_tmp, db) = db_path();
        log_progress(&db, "real entry", Some("Real content here.")).unwrap();
        assert_eq!(block_on(search(&db, None, "  !!!  ??? ", 6)).unwrap(), Vec::new());
    }

    #[test]
    fn search_returns_empty_on_fresh_db() {
        let (_tmp, db) = db_path();
        assert_eq!(block_on(search(&db, None, "anything", 6)).unwrap(), Vec::new());
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
        let hits = block_on(search(&db, None, "keyword", 3)).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn search_skips_rows_with_null_detail() {
        let (_tmp, db) = db_path();
        // summary-only rows are intentionally not full-text searchable.
        log_progress(&db, "keyword in summary only", None).unwrap();
        assert_eq!(block_on(search(&db, None, "keyword", 6)).unwrap(), Vec::new());
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

    /// Helper for "log table's full column set at the **current** schema
    /// version". Name is historical — at S1 this was the v1 shape (6
    /// columns). S3-distill added a v4 step that appends `kind`, so this
    /// now returns 7 columns. All call sites still want "the set of cols
    /// fresh-opened code produces", so they keep working unchanged.
    fn expected_log_columns() -> std::collections::BTreeSet<String> {
        [
            "id",
            "ts",
            "summary",
            "detail",
            "origin",
            "project_hash",
            // S3-distill (v4) addition:
            "kind",
        ]
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

    // S1.A.1 — fresh DB has v1 products (origin/project_hash + idx_log_ts).
    // user_version lands on `SCHEMA_VERSION` (post-S2 = 2) — the v1 step
    // still runs as part of the full v0→v1→…→latest walk.
    #[test]
    fn s1_a1_fresh_db_lands_on_current_schema() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert_eq!(log_columns(&conn), expected_log_columns());
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

    // S1.B.1 — open() three times is idempotent; user_version stays at
    // SCHEMA_VERSION across every reopen.
    #[test]
    fn s1_b1_open_thrice_is_idempotent_and_user_version_stays_at_target() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        }
    }

    // S1.B.2 — re-open a persisted DB preserves log rows and the version pin.
    #[test]
    fn s1_b2_reopen_existing_db_preserves_rows_and_version() {
        let (_tmp, db) = db_path();
        let id_a = log_progress(&db, "kept summary", Some("kept body")).unwrap();
        let id_b = log_progress(&db, "second kept", None).unwrap();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
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
    // origin defaults to 'self' and project_hash to NULL. Endpoint is
    // SCHEMA_VERSION (the full v0→v1→latest walk runs), but the v1
    // products (columns + idx_log_ts) are what this test verifies.
    #[test]
    fn s1_c1_upgrades_v0_db_to_v1_preserving_old_rows() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert_eq!(log_columns(&conn), expected_log_columns());
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

        let hits = block_on(search(&db, None, "apricot", 6)).unwrap();
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

        let hits = block_on(search(&db, None, "apricot", 6)).unwrap();
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

    // -----------------------------------------------------------------
    // Phase 6 Step 2-schema — raw_thread + raw_event tables (v2)
    // -----------------------------------------------------------------

    /// Frozen copy of the v1 final form (v0 + provenance columns + ts index).
    /// Inlined here so the v1→v2 upgrade test reflects a real v1-shaped DB
    /// independently of whatever `migrate` does now.
    const V1_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT
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
    END;
    CREATE INDEX idx_log_ts ON log(ts);";

    fn expected_raw_thread_columns() -> std::collections::BTreeSet<String> {
        [
            "thread_id",
            "agent_id",
            "team_id",
            "project_hash",
            "source",
            "parent_thread_id",
            "cwd",
            "source_path",
            "first_seen_ts",
            "last_ingest_ts",
            "last_offset",
            "last_line_no",
            // S3-schema additions:
            "last_distilled_line_no",
            "last_growth_ts",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }

    fn expected_raw_event_columns() -> std::collections::BTreeSet<String> {
        ["id", "thread_id", "line_no", "payload", "ingested_at"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn table_columns(conn: &Connection, table: &str) -> std::collections::BTreeSet<String> {
        let sql = format!("PRAGMA table_info({table})");
        let mut stmt = conn.prepare(&sql).unwrap();
        let names = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
        names.map(|n| n.unwrap()).collect()
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct V1Row {
        id: i64,
        ts: i64,
        summary: String,
        detail: Option<String>,
        origin: String,
        project_hash: Option<String>,
    }

    /// Lay down a v1-shape DB at `db_path` (per `V1_DDL_FROZEN`) and seed
    /// three rows that mix default and non-default provenance values, so
    /// the v1→v2 upgrade test can verify every field survives byte-for-byte.
    fn build_v1_db(db_path: &std::path::Path) -> Vec<V1Row> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db_path).unwrap();
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(V1_DDL_FROZEN).unwrap();
        conn.execute_batch("PRAGMA user_version = 1;").unwrap();

        let seeds: [(i64, &str, Option<&str>, &str, Option<&str>); 3] = [
            (2001, "v1 self default", None, "self", None),
            (
                2002,
                "v1 inbox flagged",
                Some("Sourced from inbox tag."),
                "inbox",
                Some("abc123def456"),
            ),
            (
                2003,
                "v1 self with detail",
                Some("Plain self entry."),
                "self",
                None,
            ),
        ];
        let mut rows = Vec::new();
        for (ts, summary, detail, origin, project_hash) in seeds {
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin, project_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, summary, detail, origin, project_hash],
            )
            .unwrap();
            rows.push(V1Row {
                id: conn.last_insert_rowid(),
                ts,
                summary: summary.to_string(),
                detail: detail.map(|s| s.to_string()),
                origin: origin.to_string(),
                project_hash: project_hash.map(|s| s.to_string()),
            });
        }
        drop(conn);
        rows
    }

    // S2s.A.1 — fresh DB lands on full v2 schema; v1 products still present.
    #[test]
    fn s2s_a1_fresh_db_lands_on_v2_schema_with_raw_tables() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        // v2 products
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));
        // v1 products still survive a fresh open()
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
    }

    // S2s.A.2 — raw_* column sets match spec; (thread_id, line_no) UNIQUE
    // is enforced — a duplicate (t, 1) insert must fail with a constraint
    // violation.
    #[test]
    fn s2s_a2_raw_tables_have_expected_columns_and_unique_constraint() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(table_columns(&conn, "raw_thread"), expected_raw_thread_columns());
        assert_eq!(table_columns(&conn, "raw_event"), expected_raw_event_columns());

        // Seed a parent thread row so the UNIQUE check has a valid FK.
        conn.execute(
            "INSERT INTO raw_thread(thread_id, source_path, first_seen_ts, last_ingest_ts) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["thr-1", "/tmp/rollout.jsonl", 0_i64, 0_i64],
        )
        .unwrap();

        // First (thr-1, 1) wins; second must fail.
        conn.execute(
            "INSERT INTO raw_event(thread_id, line_no, payload, ingested_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["thr-1", 1_i64, "{}", 0_i64],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO raw_event(thread_id, line_no, payload, ingested_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params!["thr-1", 1_i64, "{}", 0_i64],
            )
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique") || msg.contains("constraint"),
            "expected UNIQUE violation, got: {err}"
        );
    }

    // S2s.B.1 — open() three times is idempotent on the v2 schema.
    #[test]
    fn s2s_b1_open_thrice_is_idempotent_at_v2() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(schema_object_exists(&conn, "table", "raw_thread"));
            assert!(schema_object_exists(&conn, "table", "raw_event"));
        }
    }

    // S2s.B.2 — seed 1 raw_thread + 2 raw_event on a v2 DB, close, reopen;
    // version + rows survive untouched.
    #[test]
    fn s2s_b2_reopen_v2_db_preserves_raw_rows_and_version() {
        let (_tmp, db) = db_path();
        {
            let conn = open(&db).unwrap();
            conn.execute(
                "INSERT INTO raw_thread(thread_id, agent_id, source_path, first_seen_ts, last_ingest_ts) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params!["thr-keep", "agent-a", "/tmp/keep.jsonl", 100_i64, 200_i64],
            )
            .unwrap();
            for line_no in 1..=2_i64 {
                conn.execute(
                    "INSERT INTO raw_event(thread_id, line_no, payload, ingested_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params!["thr-keep", line_no, format!("{{\"n\":{line_no}}}"), 200_i64],
                )
                .unwrap();
            }
        }

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        let (agent_id, source_path, first_seen_ts, last_ingest_ts): (
            Option<String>,
            String,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT agent_id, source_path, first_seen_ts, last_ingest_ts \
                 FROM raw_thread WHERE thread_id = ?1",
                params!["thr-keep"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("agent-a"));
        assert_eq!(source_path, "/tmp/keep.jsonl");
        assert_eq!(first_seen_ts, 100);
        assert_eq!(last_ingest_ts, 200);

        let mut stmt = conn
            .prepare(
                "SELECT line_no, payload FROM raw_event \
                 WHERE thread_id = ?1 ORDER BY line_no",
            )
            .unwrap();
        let events: Vec<(i64, String)> = stmt
            .query_map(params!["thr-keep"], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            events,
            vec![(1_i64, "{\"n\":1}".to_string()), (2_i64, "{\"n\":2}".to_string())]
        );
    }

    // S2s.C.1 — v1 DB upgrades to v2: every old log row survives byte-for-byte
    // (id/ts/summary/detail/origin/project_hash) and the v2 raw_* tables now
    // exist.
    #[test]
    fn s2s_c1_upgrades_v1_db_to_v2_preserving_log_rows_and_provenance() {
        let (_tmp, db) = db_path();
        let seeded = build_v1_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));

        let mut stmt = conn
            .prepare(
                "SELECT id, ts, summary, detail, origin, project_hash \
                 FROM log ORDER BY id",
            )
            .unwrap();
        let upgraded: Vec<V1Row> = stmt
            .query_map([], |r| {
                Ok(V1Row {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    summary: r.get(2)?,
                    detail: r.get(3)?,
                    origin: r.get(4)?,
                    project_hash: r.get(5)?,
                })
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(upgraded, seeded);
    }

    // S2s.C.2 — v0 DB walks the full v0→v1→v2 chain in one open(): every
    // old log row survives, origin defaults to 'self' / project_hash to
    // NULL (v1 effect), idx_log_ts exists (v1), raw_thread/raw_event exist
    // (v2).
    #[test]
    fn s2s_c2_upgrades_v0_db_through_v1_to_v2_in_one_open() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));

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

    // -----------------------------------------------------------------
    // Phase 6 Step 2-ingest — rollout → raw_thread/raw_event scanner
    // -----------------------------------------------------------------

    const SAMPLE_THREAD_UUID: &str = "019e6725-269e-7bc1-b118-a1e757ee324c";
    const SAMPLE_TEAM_ID: &str = "team_07e1d699-e0b4-453f-8d29-ca6281b36f30";
    const SAMPLE_PROJECT_HASH: &str = "b854e7722178";
    const SAMPLE_ISO_TS: &str = "2026-05-27T09-55-48";

    fn ingest_test_env() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let memory_db = tmp.path().join("memory.db");
        let scan_root = tmp.path().join("team_sessions");
        std::fs::create_dir_all(&scan_root).unwrap();
        (tmp, memory_db, scan_root)
    }

    fn write_rollout(
        scan_root: &std::path::Path,
        team_id: &str,
        project_hash: &str,
        thread_uuid: &str,
        iso_ts: &str,
        content: &str,
    ) -> PathBuf {
        let dir = scan_root.join(team_id).join(project_hash);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-{iso_ts}-{thread_uuid}.jsonl"));
        std::fs::write(&path, content).unwrap();
        path
    }

    fn count_rows(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn fetch_event_payloads(conn: &Connection, thread_id: &str) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT line_no, payload FROM raw_event \
                 WHERE thread_id = ?1 ORDER BY line_no",
            )
            .unwrap();
        stmt.query_map(params![thread_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    // S2i.A.1 — parse_rollout_attribution: good sample + bad stems.
    #[test]
    fn s2i_a1_parse_rollout_attribution_real_sample_and_bad_stem() {
        let good = std::path::PathBuf::from(format!(
            "/scan/team_sessions/{SAMPLE_TEAM_ID}/{SAMPLE_PROJECT_HASH}/\
             rollout-{SAMPLE_ISO_TS}-{SAMPLE_THREAD_UUID}.jsonl"
        ));
        let attr = parse_rollout_attribution(&good).expect("real sample must parse");
        assert_eq!(attr.thread_id, SAMPLE_THREAD_UUID);
        assert_eq!(attr.team_id, SAMPLE_TEAM_ID);
        assert_eq!(attr.project_hash, SAMPLE_PROJECT_HASH);

        // Bad: stem ends in a 36-char tail that is NOT UUID-shaped (no
        // dashes at positions 8/13/18/23).
        let bad_uuid_tail = "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX";
        assert_eq!(bad_uuid_tail.len(), 36);
        let bad_uuid = std::path::PathBuf::from(format!(
            "/scan/team_sessions/t/p/rollout-{SAMPLE_ISO_TS}-{bad_uuid_tail}.jsonl"
        ));
        assert_eq!(parse_rollout_attribution(&bad_uuid), None);

        // Bad: stem too short to even contain a 36-char tail.
        let too_short = std::path::PathBuf::from("/scan/team_sessions/t/p/rollout-foo.jsonl");
        assert_eq!(parse_rollout_attribution(&too_short), None);

        // Bad: no `team_sessions` in the path → no attribution.
        let no_team = std::path::PathBuf::from(format!(
            "/sessions/2026/05/21/rollout-{SAMPLE_ISO_TS}-{SAMPLE_THREAD_UUID}.jsonl"
        ));
        assert_eq!(parse_rollout_attribution(&no_team), None);

        // Bad: extension is not .jsonl.
        let not_jsonl = std::path::PathBuf::from(format!(
            "/scan/team_sessions/t/p/rollout-{SAMPLE_ISO_TS}-{SAMPLE_THREAD_UUID}.txt"
        ));
        assert_eq!(parse_rollout_attribution(&not_jsonl), None);
    }

    // S2i.A.2 — fresh DB + 1 file with N>=3 lines: 1 raw_thread with all
    // attribution fields correct, last_line_no=N, last_offset=filesize;
    // N raw_event rows with line_no 1..N and payloads byte-equal to source
    // lines.
    #[test]
    fn s2i_a2_first_pass_writes_thread_and_events_byte_for_byte() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_a2";
        let content = "line1\nline2\nline3\n"; // 18 bytes, 3 lines
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            content,
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();

        assert_eq!(stats.files_seen, 1);
        assert_eq!(stats.threads_touched, 1);
        assert_eq!(stats.events_inserted, 3);

        let (tid, aid, tmid, ph, sp, lln, loff): (
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT thread_id, agent_id, team_id, project_hash, source_path, \
                        last_line_no, last_offset \
                 FROM raw_thread",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(tid, SAMPLE_THREAD_UUID);
        assert_eq!(aid, agent_id);
        assert_eq!(tmid.as_deref(), Some(SAMPLE_TEAM_ID));
        assert_eq!(ph.as_deref(), Some(SAMPLE_PROJECT_HASH));
        assert_eq!(sp, rollout_path.to_string_lossy());
        assert_eq!(lln, 3);
        assert_eq!(loff, content.len() as i64);

        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![
                (1, "line1".to_string()),
                (2, "line2".to_string()),
                (3, "line3".to_string()),
            ]
        );
    }

    // S2i.B.1 — second ingest with no file change is a complete no-op
    // (events_inserted == 0, cursor frozen, raw_event count unchanged).
    #[test]
    fn s2i_b1_second_pass_unchanged_file_is_a_noop() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_b1";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc\n",
        );
        let conn = open(&memory_db).unwrap();
        let first = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(first.events_inserted, 3);

        let (off1, ln1): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(second.events_inserted, 0);
        assert_eq!(second.files_seen, 1);
        let (off2, ln2): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(off1, off2);
        assert_eq!(ln1, ln2);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_event"), 3);
    }

    // S2i.B.2 — append M lines, re-ingest: exactly M new raw_event rows
    // (line_no N+1..N+M), old payloads untouched, cursor advanced to new EOF.
    #[test]
    fn s2i_b2_append_then_ingest_picks_up_only_new_lines() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_b2";
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "first\nsecond\n",
        );
        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();

        // Append 2 more lines.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout_path)
            .unwrap();
        f.write_all(b"third\nfourth\n").unwrap();
        drop(f);

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(second.events_inserted, 2);
        assert_eq!(second.threads_touched, 1);

        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![
                (1, "first".to_string()),
                (2, "second".to_string()),
                (3, "third".to_string()),
                (4, "fourth".to_string()),
            ]
        );
        let (off, ln): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let new_size = std::fs::metadata(&rollout_path).unwrap().len() as i64;
        assert_eq!(off, new_size);
        assert_eq!(ln, 4);
    }

    // S2i.C.1 — two files with distinct team_id/project_hash/thread_uuid:
    // 2 raw_thread rows, each with its own event set, no cross-contamination.
    #[test]
    fn s2i_c1_two_distinct_threads_each_get_their_own_rows() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_c1";
        let thread_a = "11111111-2222-3333-4444-555555555555";
        let team_a = "team_aaaaaaaa-1111-1111-1111-111111111111";
        let hash_a = "aaaaaaaaaaaa";
        let thread_b = "66666666-7777-8888-9999-aaaaaaaaaaaa";
        let team_b = "team_bbbbbbbb-2222-2222-2222-222222222222";
        let hash_b = "bbbbbbbbbbbb";
        write_rollout(&scan_root, team_a, hash_a, thread_a, "2026-05-27T01-00-00", "A1\nA2\nA3\n");
        write_rollout(&scan_root, team_b, hash_b, thread_b, "2026-05-27T02-00-00", "B1\nB2\n");

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.threads_touched, 2);
        assert_eq!(stats.events_inserted, 5);

        let threads: Vec<(String, Option<String>, Option<String>)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT thread_id, team_id, project_hash FROM raw_thread ORDER BY thread_id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            threads,
            vec![
                (thread_a.to_string(), Some(team_a.to_string()), Some(hash_a.to_string())),
                (thread_b.to_string(), Some(team_b.to_string()), Some(hash_b.to_string())),
            ]
        );

        let a_payloads: Vec<String> = fetch_event_payloads(&conn, thread_a)
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(a_payloads, vec!["A1", "A2", "A3"]);
        let b_payloads: Vec<String> = fetch_event_payloads(&conn, thread_b)
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(b_payloads, vec!["B1", "B2"]);
    }

    // S2i.C.2 — a "sub-agent" rollout (same agent dir, same filename shape,
    // different thread uuid) is ingested like any other; `source` stays NULL
    // because we don't parse payload at this stage.
    #[test]
    fn s2i_c2_subagent_rollout_ingests_with_null_source_column() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_c2";
        // Parent thread
        let parent_uuid = SAMPLE_THREAD_UUID;
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            parent_uuid,
            "2026-05-27T09-55-48",
            "parent line 1\nparent line 2\n",
        );
        // Sub-agent thread (same team/project_hash dir, different uuid + ts)
        let sub_uuid = "abcdef01-2345-6789-abcd-ef0123456789";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            sub_uuid,
            "2026-05-27T09-56-00",
            "sub line 1\n",
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.threads_touched, 2);
        assert_eq!(stats.events_inserted, 3);

        // `source` is NULL for both — we never wrote it.
        let null_source_count: i64 = count_rows(
            &conn,
            "SELECT count(*) FROM raw_thread WHERE source IS NULL",
        );
        assert_eq!(null_source_count, 2);

        // Sub-agent's event made it in.
        assert_eq!(
            fetch_event_payloads(&conn, sub_uuid),
            vec![(1, "sub line 1".to_string())]
        );
    }

    // S2if.C.1 — shrink: truncate to < last_offset, re-ingest →
    // DELETE all raw_event rows for this thread (in the same IMMEDIATE
    // tx) then re-scan from offset 0. Post-shrink, raw_event matches
    // the file's current content exactly; no orphan rows survive.
    //
    // Replaces the pre-S2if-fix s2i_d1 test, which (correctly for the
    // earlier UNIQUE+IGNORE-only semantics) accepted orphans for line_nos
    // 4 and 5. S2if-fix tightened shrink semantics to "no orphans" — see
    // the shrink branch in `ingest_one_file_in_tx`.
    #[test]
    fn s2if_c1_shrink_purges_orphans_and_re_reads_from_zero() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_shrink";
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "x1\nx2\nx3\nx4\nx5\n", // 5 lines, 15 bytes
        );
        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_event"), 5);

        // Truncate to first 3 lines (9 bytes < 15 bytes).
        std::fs::write(&rollout_path, "x1\nx2\nx3\n").unwrap();

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        // Shrink path DELETEs all rows then re-inserts 1..3, so 3 inserts.
        assert_eq!(second.events_inserted, 3);

        // raw_event for this thread now matches the file exactly:
        // 3 rows, no orphans for line_nos 4 or 5.
        let payloads = fetch_event_payloads(&conn, SAMPLE_THREAD_UUID);
        assert_eq!(
            payloads,
            vec![
                (1, "x1".to_string()),
                (2, "x2".to_string()),
                (3, "x3".to_string()),
            ]
        );
        assert_eq!(
            count_rows(&conn, "SELECT count(*) FROM raw_event"),
            3,
            "orphan rows for line_no 4 and 5 must be deleted by the shrink path"
        );

        // Cursor lands on the shrunk EOF.
        let (off, ln): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(off, 9);
        assert_eq!(ln, 3);
    }

    // S2i.D.2 — bad filename matching the glob: skipped, ingest pass still
    // succeeds for sibling well-formed files.
    #[test]
    fn s2i_d2_bad_filename_is_skipped_without_failing_the_pass() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_d2";
        let team_dir = scan_root.join(SAMPLE_TEAM_ID).join(SAMPLE_PROJECT_HASH);
        std::fs::create_dir_all(&team_dir).unwrap();
        // Bad: 36-char tail of stem is not UUID-shaped.
        std::fs::write(
            team_dir.join("rollout-2026-05-27T09-55-48-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX.jsonl"),
            "bogus\nbogus\n",
        )
        .unwrap();
        // Good neighbour.
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "ok\n",
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(stats.files_seen, 2);
        // Only the well-formed file got attributed → 1 thread, 1 event.
        assert_eq!(stats.threads_touched, 1);
        assert_eq!(stats.events_inserted, 1);
        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![(1, "ok".to_string())]
        );
    }

    // S2i.D.3 — per-agent isolation: a well-formed rollout placed in a
    // *different* agent's directory tree (outside this agent's scan_root)
    // is invisible to this agent's ingester.
    #[test]
    fn s2i_d3_other_agents_rollouts_outside_scan_root_are_invisible() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_db = tmp.path().join("memory.db");
        // Our agent's scan_root.
        let my_scan_root = tmp.path().join("agents/agent_me/team_sessions");
        std::fs::create_dir_all(&my_scan_root).unwrap();
        // Another agent's identical layout, NOT under my scan_root.
        let other_scan_root = tmp.path().join("agents/agent_other/team_sessions");
        std::fs::create_dir_all(&other_scan_root).unwrap();
        write_rollout(
            &other_scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "other agent line\n",
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &my_scan_root, "agent_me").unwrap();
        assert_eq!(stats.files_seen, 0);
        assert_eq!(stats.threads_touched, 0);
        assert_eq!(stats.events_inserted, 0);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_thread"), 0);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_event"), 0);
    }

    // S2i.D.4 — legacy `sessions/YYYY/MM/DD/` layout is excluded by
    // construction (it's not under scan_root, and even if you point
    // scan_root at it, the path has no `team_sessions` segment so
    // parse_rollout_attribution returns None).
    #[test]
    fn s2i_d4_legacy_sessions_layout_is_not_ingested() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_db = tmp.path().join("memory.db");
        let my_scan_root = tmp.path().join("agents/agent_me/team_sessions");
        std::fs::create_dir_all(&my_scan_root).unwrap();
        // Legacy date-bucket layout, sitting outside any team_sessions tree.
        let legacy_dir = tmp.path().join("sessions/2026/05/21");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join(format!("rollout-{SAMPLE_ISO_TS}-{SAMPLE_THREAD_UUID}.jsonl")),
            "legacy content\n",
        )
        .unwrap();

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &my_scan_root, "agent_me").unwrap();
        assert_eq!(stats.files_seen, 0);
        assert_eq!(stats.events_inserted, 0);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_thread"), 0);
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 2-ingest-fix — torn line / UTF-8 panic / shrink orphan
    // -----------------------------------------------------------------

    // S2if.A.1 — torn line at EOF: the trailing line "c" (no `\n`) must
    // NOT be inserted and the cursor must NOT advance past the last
    // committed `\n`.
    #[test]
    fn s2if_a1_torn_line_at_eof_is_not_committed() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc", // 5 bytes; last line has no trailing \n
        );
        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, "agent_a1").unwrap();
        assert_eq!(stats.events_inserted, 2);

        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        let (off, ln): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // 4 bytes = "a\nb\n" — the position right after the last \n we saw.
        // "c" lives at bytes 4..5 and is NOT committed.
        assert_eq!(off, 4);
        assert_eq!(ln, 2);
    }

    // S2if.A.2 — the torn-line writer finishes: the previously-uncommitted
    // "c" gets completed to "cX\n" and a fresh "d\n" is appended. After
    // a second ingest the table has exactly four rows, AND `line_no=3`
    // holds the full "cX" — proving pass 1 did not split the half-line
    // across two raw_event rows.
    #[test]
    fn s2if_a2_completed_torn_line_ingests_as_one_row() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc",
        );
        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, "agent_a2").unwrap();

        // Writer completes the torn last line and adds one more.
        std::fs::write(&rollout_path, "a\nb\ncX\nd\n").unwrap();

        let second = ingest_once(&conn, &scan_root, "agent_a2").unwrap();
        assert_eq!(second.events_inserted, 2);

        let payloads = fetch_event_payloads(&conn, SAMPLE_THREAD_UUID);
        assert_eq!(
            payloads,
            vec![
                (1, "a".to_string()),
                (2, "b".to_string()),
                (3, "cX".to_string()),
                (4, "d".to_string()),
            ],
            "line 3 must be the FULL completed 'cX', not the half-line 'c'"
        );
    }

    // S2if.B.1 — parse_rollout_attribution must never panic on a stem
    // where the `len - 36` byte position falls inside a multi-byte UTF-8
    // codepoint. Construct a stem with a 4-byte emoji (🦀) straddling
    // that exact boundary and assert `None` is returned.
    #[test]
    fn s2if_b1_parse_rollout_attribution_does_not_panic_on_utf8_boundary() {
        let prefix = "rollout-2026-05-27T09-55-48-";
        assert_eq!(prefix.len(), 28);
        // Total stem byte length = 28 (prefix) + 4 (🦀) + 35 ('x') = 67.
        // tail_start = 67 - 36 = 31 — the FOURTH byte of 🦀, a UTF-8
        // continuation byte. `&stem[31..]` would panic without the
        // `is_char_boundary` guard.
        let suffix = "x".repeat(35);
        let bad_name = format!("{prefix}🦀{suffix}.jsonl");
        let stem_len = bad_name.len() - ".jsonl".len();
        assert_eq!(stem_len, 67);
        // Sanity: confirm the boundary check is the one that fires.
        let stem = &bad_name[..stem_len];
        assert!(!stem.is_char_boundary(stem_len - 36));

        let path = std::path::PathBuf::from(format!(
            "/scan/team_sessions/t/p/{bad_name}"
        ));
        assert_eq!(parse_rollout_attribution(&path), None);
    }

    // S2if.B.2 — placing the same UTF-8-boundary-bad file in scan_root
    // alongside a well-formed neighbour: ingest_once must not panic, the
    // bad file is skipped, the neighbour is ingested.
    #[test]
    fn s2if_b2_ingest_skips_utf8_named_file_and_keeps_going() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_b2";

        // UTF-8-boundary-bad file in its own team/hash directory.
        let bad_dir = scan_root.join("team_emoji").join("hash_emoji");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let prefix = "rollout-2026-05-27T09-55-48-";
        let suffix = "x".repeat(35);
        let bad_name = format!("{prefix}🦀{suffix}.jsonl");
        std::fs::write(bad_dir.join(&bad_name), "bogus content\n").unwrap();

        // Well-formed neighbour.
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "ok1\nok2\n",
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.threads_touched, 1);
        assert_eq!(stats.events_inserted, 2);

        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![(1, "ok1".to_string()), (2, "ok2".to_string())]
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3-schema — distill cursor + growth_ts (v3 migration)
    // -----------------------------------------------------------------

    /// Frozen copy of the v2 final form: log v1 (provenance cols + idx_log_ts)
    /// + raw_thread (old 12 cols, pre-S3) + raw_event + idx_raw_event_thread.
    /// Used by the v2→v3 upgrade test so it doesn't depend on a live migrate.
    const V2_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT
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
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id        TEXT PRIMARY KEY,
        agent_id         TEXT,
        team_id          TEXT,
        project_hash     TEXT,
        source           TEXT,
        parent_thread_id TEXT,
        cwd              TEXT,
        source_path      TEXT NOT NULL,
        first_seen_ts    INTEGER NOT NULL,
        last_ingest_ts   INTEGER NOT NULL,
        last_offset      INTEGER NOT NULL DEFAULT 0,
        last_line_no     INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);";

    /// Lay down a v2-shape DB at `db_path`. Caller seeds rows via direct SQL.
    fn build_v2_db(db_path: &std::path::Path) {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db_path).unwrap();
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(V2_DDL_FROZEN).unwrap();
        conn.execute_batch("PRAGMA user_version = 2;").unwrap();
    }

    // S3s.A.1 — fresh DB lands on v3 with the two new distill-cursor columns,
    // and every v1/v2 product is still present.
    #[test]
    fn s3s_a1_fresh_db_lands_on_v3_with_distill_cursor_columns() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        let cols = table_columns(&conn, "raw_thread");
        assert!(cols.contains("last_distilled_line_no"));
        assert!(cols.contains("last_growth_ts"));
        assert_eq!(cols.len(), 14, "raw_thread should be 14 columns at v3");

        // v2 products still present.
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));
        // v1 products still present.
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
    }

    // S3s.A.2 — column set equality + defaults check: a minimal insert
    // leaves `last_distilled_line_no = 0` and `last_growth_ts = NULL`.
    #[test]
    fn s3s_a2_minimal_insert_uses_v3_column_defaults() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        assert_eq!(table_columns(&conn, "raw_thread"), expected_raw_thread_columns());

        conn.execute(
            "INSERT INTO raw_thread(thread_id, source_path, first_seen_ts, last_ingest_ts) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["minimal-thr", "/tmp/r.jsonl", 0_i64, 0_i64],
        )
        .unwrap();

        let (last_distilled, last_growth): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_distilled_line_no, last_growth_ts FROM raw_thread \
                 WHERE thread_id = 'minimal-thr'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_distilled, 0);
        assert!(last_growth.is_none());
    }

    // S3s.B.1 — open() three times on a fresh DB: each open lands at v3
    // and migration arm 3 is idempotent (no duplicate-column crash).
    #[test]
    fn s3s_b1_open_thrice_is_idempotent_at_v3() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(schema_object_exists(&conn, "table", "raw_thread"));
        }
    }

    // S3s.B.2 — v2 DB upgrades to v3: a pre-existing raw_thread row with
    // only the old 12 columns populated survives the ALTERs; its v3
    // columns pick up the schema defaults (0 / NULL).
    #[test]
    fn s3s_b2_upgrades_v2_db_to_v3_preserving_raw_thread_rows() {
        let (_tmp, db) = db_path();
        build_v2_db(&db);

        // Seed an old-shape (12-col) raw_thread row before the upgrade.
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO raw_thread \
                 (thread_id, agent_id, team_id, project_hash, source, \
                  parent_thread_id, cwd, source_path, first_seen_ts, \
                  last_ingest_ts, last_offset, last_line_no) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    "thr-v2-old",
                    "agent-vintage",
                    "team-vintage",
                    "proj-hash",
                    "vscode",
                    None::<&str>,
                    "/tmp/cwd",
                    "/tmp/rollout.jsonl",
                    100_i64,
                    200_i64,
                    50_i64,
                    5_i64,
                ],
            )
            .unwrap();
        }

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        let (last_distilled, last_growth): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_distilled_line_no, last_growth_ts FROM raw_thread \
                 WHERE thread_id = 'thr-v2-old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_distilled, 0);
        assert!(last_growth.is_none());

        // 12 original columns byte-for-byte preserved.
        let (
            agent_id,
            team_id,
            project_hash,
            source,
            parent_thread_id,
            cwd,
            source_path,
            first_seen_ts,
            last_ingest_ts,
            last_offset,
            last_line_no,
        ): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            i64,
            i64,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT agent_id, team_id, project_hash, source, parent_thread_id, \
                        cwd, source_path, first_seen_ts, last_ingest_ts, \
                        last_offset, last_line_no \
                 FROM raw_thread WHERE thread_id = 'thr-v2-old'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(agent_id, "agent-vintage");
        assert_eq!(team_id.as_deref(), Some("team-vintage"));
        assert_eq!(project_hash.as_deref(), Some("proj-hash"));
        assert_eq!(source.as_deref(), Some("vscode"));
        assert!(parent_thread_id.is_none());
        assert_eq!(cwd.as_deref(), Some("/tmp/cwd"));
        assert_eq!(source_path, "/tmp/rollout.jsonl");
        assert_eq!(first_seen_ts, 100);
        assert_eq!(last_ingest_ts, 200);
        assert_eq!(last_offset, 50);
        assert_eq!(last_line_no, 5);
    }

    // S3s.B.3 — full v0 → v1 → v2 → v3 chain in one open(): every step's
    // products land, the seeded v0 log rows survive.
    #[test]
    fn s3s_b3_upgrades_v0_db_through_full_chain_to_v3() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        // v1 products
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
        // v2 products
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));
        // v3 products: raw_thread is the 14-column v3 shape
        assert_eq!(table_columns(&conn, "raw_thread"), expected_raw_thread_columns());

        // Old log rows preserved.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, seeded.len());
    }

    // ---- S4schema (v6) — log_vec re-dimension 4096 → 1024 (BAAI/bge-m3) ----

    // Cumulative v5 schema snapshot (log through v4 `kind`, raw_thread through
    // v3, raw_event, log_fts + triggers, log_vec float[4096]). Frozen so the
    // v5→v6 step can be exercised against the exact shape a real v5 DB had.
    const V5_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT,
        kind         TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(detail, content='log', content_rowid='id');
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id        TEXT PRIMARY KEY,
        agent_id         TEXT,
        team_id          TEXT,
        project_hash     TEXT,
        source           TEXT,
        parent_thread_id TEXT,
        cwd              TEXT,
        source_path      TEXT NOT NULL,
        first_seen_ts    INTEGER NOT NULL,
        last_ingest_ts   INTEGER NOT NULL,
        last_offset      INTEGER NOT NULL DEFAULT 0,
        last_line_no     INTEGER NOT NULL DEFAULT 0,
        last_distilled_line_no INTEGER NOT NULL DEFAULT 0,
        last_growth_ts   INTEGER
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);
    CREATE VIRTUAL TABLE log_vec USING vec0(embedding float[4096]);";

    fn log_vec_ddl(conn: &Connection) -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='log_vec'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    // S4schema.D.1 — v5→v6 drops+recreates log_vec at 1024; v0→…→v6 full chain
    // also lands log_vec at 1024. Confirms DROP+CREATE vec0 runs inside the
    // IMMEDIATE migration tx, and the rebuilt table is functional.
    #[test]
    fn s4schema_d1_v6_redimensions_log_vec_to_1024() {
        // Pins the s4 log_vec (1024-dim) schema at the *current* SCHEMA_VERSION
        // — not a literal version number (those rot on every bump; the real
        // invariant is read_user_version == SCHEMA_VERSION, asserted after
        // migrate below). v7 only touches log_fts, so log_vec stays 1024 here.
        // vec0 is registered process-globally inside backend::open(); this test
        // lays a frozen v5 schema (which has a vec0 table) via a RAW Connection
        // before any open(), so register the module explicitly to avoid a
        // test-ordering flake ("no such module: vec0").
        ensure_vec_extension();

        // (a) v5 → v6.
        {
            let (_tmp, db) = db_path();
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            {
                let conn = Connection::open(&db).unwrap();
                let _: String = conn
                    .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                    .unwrap();
                conn.execute_batch(V5_DDL_FROZEN).unwrap();
                conn.execute_batch("PRAGMA user_version = 5;").unwrap();
                assert!(log_vec_ddl(&conn).contains("float[4096]"), "v5 precondition");
                assert_eq!(read_user_version(&conn), 5); // laid-down v5 DB, pre-migration
            }
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            let ddl = log_vec_ddl(&conn);
            assert!(
                ddl.contains("float[1024]") && !ddl.contains("4096"),
                "post-v6 log_vec must be 1024: {ddl}"
            );
            // Functional: the rebuilt 1024-dim table accepts a row.
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (1, 's', 'd', 'self')",
                [],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            let v = vec![0.0_f32; 1024];
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_to_match_json(&v)],
            )
            .unwrap();
            // Pre-v5 products survived the in-place migration.
            assert!(schema_object_exists(&conn, "table", "raw_event"));
        }

        // (b) v0 → … → v6 full chain ends with log_vec at 1024.
        {
            let (_tmp, db) = db_path();
            build_v0_db(&db);
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(log_vec_ddl(&conn).contains("float[1024]"), "v0→v6 chain log_vec dim");
        }
    }

    // ---- S5schema (v7) — log_fts re-tokenized to FTS5 `trigram` (CJK lexical) ----

    // Cumulative v6 schema snapshot: v5 shape but log_vec at float[1024], and
    // log_fts still on the DEFAULT (unicode61) tokenizer (trigram arrives in v7).
    const V6_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT,
        kind         TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(detail, content='log', content_rowid='id');
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id        TEXT PRIMARY KEY,
        agent_id         TEXT,
        team_id          TEXT,
        project_hash     TEXT,
        source           TEXT,
        parent_thread_id TEXT,
        cwd              TEXT,
        source_path      TEXT NOT NULL,
        first_seen_ts    INTEGER NOT NULL,
        last_ingest_ts   INTEGER NOT NULL,
        last_offset      INTEGER NOT NULL DEFAULT 0,
        last_line_no     INTEGER NOT NULL DEFAULT 0,
        last_distilled_line_no INTEGER NOT NULL DEFAULT 0,
        last_growth_ts   INTEGER
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);
    CREATE VIRTUAL TABLE log_vec USING vec0(embedding float[1024]);";

    fn log_fts_ddl(conn: &Connection) -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='log_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    // T1 — v6→v7: log_fts re-tokenized to trigram, and pre-existing rows are
    // re-searchable (the 'rebuild' actually repopulated the new index).
    #[test]
    fn v6_to_v7_migrate() {
        let (_tmp, db) = db_path();
        ensure_vec_extension(); // V6_DDL_FROZEN has a vec0 table (raw open)
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
            conn.execute_batch(V6_DDL_FROZEN).unwrap();
            conn.execute_batch("PRAGMA user_version = 6;").unwrap();
            // Seed rows under the OLD unicode61 log_fts (one zh, one en).
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (1, 's1', ?1, 'self')",
                params!["我们的会话状态最终选 Postgres 存储方案"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (2, 's2', ?1, 'self')",
                params!["an english oauth note about tokens"],
            )
            .unwrap();
            assert!(!log_fts_ddl(&conn).contains("trigram"), "v6 precondition: unicode61");
            assert_eq!(read_user_version(&conn), 6);
        }
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert!(log_fts_ddl(&conn).contains("trigram"), "post-v7 log_fts must be trigram");
        // Rebuild evidence: the pre-existing English row is FTS-retrievable.
        let en = fts_ranked_ids(&conn, &build_fts_match("oauth").unwrap(), 50).unwrap();
        assert!(!en.is_empty(), "rebuild must re-index pre-existing rows; en hits = {en:?}");
        // And the Chinese row is now CJK-substring searchable (0 under unicode61).
        let zh = fts_ranked_ids(&conn, &build_fts_match("会话状态用什么存").unwrap(), 50).unwrap();
        eprintln!("[T1] post-v7 zh FTS hits = {zh:?} (Chinese row recalled via detail 子串)");
        assert!(!zh.is_empty(), "trigram + rebuild: Chinese detail must be FTS-recallable");
    }

    // T2 — v0 → … → v7 full chain ends at v7 with all core products + trigram + 1024.
    #[test]
    fn v0_to_v7_full_chain() {
        let (_tmp, db) = db_path();
        build_v0_db(&db);
        let conn = open(&db).unwrap();
        // read_user_version == SCHEMA_VERSION is the invariant (currently 7).
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "log_vec"));
        assert!(log_fts_ddl(&conn).contains("trigram"), "v0→v7 chain: log_fts trigram");
        assert!(log_vec_ddl(&conn).contains("float[1024]"), "v0→v7 chain: log_vec 1024");
    }

    // T3 — re-open a v7 DB: migrate is a no-op, log_fts stays trigram, version stable.
    #[test]
    fn v7_migrate_idempotent() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(log_fts_ddl(&conn).contains("trigram"), "log_fts stays trigram across reopens");
        }
    }

    // T4 — Design B gate: build_fts_match makes a CJK query lexically matchable
    // via DETAIL substrings (summary is NOT FTS-indexed), AND bounded
    // segmentation does not over-recall.
    #[test]
    fn fts_match_cjk_substring() {
        let (_tmp, db) = db_path();
        // NOTE: log_fts indexes only `detail` → the Chinese to be matched MUST
        // live in `detail` (summary中文 is invisible to FTS).
        let target = log_progress(
            &db,
            "会话存储选型",
            Some("我们的会话状态要持久化，最终选 Postgres 存储"),
        )
        .unwrap();
        // Precision noise #1 (semantic neighbor, no query chunk): 状态机, not 会话状态.
        let noise_word = log_progress(
            &db,
            "状态机",
            Some("状态机设计采用事件驱动模式"),
        )
        .unwrap();
        // Precision noise #2 (THE Design-B discriminator): contains the query
        // 3-gram "状态用" but NOT the bounded chunk "会话状态"/"用什么存". A brute
        // per-char 3-gram sweep WOULD recall this; bounded segmentation must NOT.
        let noise_trigram = log_progress(
            &db,
            "监控看板",
            Some("性能状态用量监控看板，与本主题无关"),
        )
        .unwrap();

        let conn = open(&db).unwrap();
        let expr = build_fts_match("会话状态用什么存").expect("CJK query → MATCH expr");
        eprintln!("[T4] build_fts_match(\"会话状态用什么存\") = {expr}");
        let hits = fts_ranked_ids(&conn, &expr, 50).unwrap();
        eprintln!(
            "[T4] FTS hits = {hits:?}  (target={target}, noise_word={noise_word}, noise_trigram={noise_trigram})"
        );
        assert!(hits.contains(&target), "RECALL: target row matched via detail 中文子串");
        assert!(!hits.contains(&noise_word), "PRECISION: 状态机设计 row must NOT match");
        assert!(
            !hits.contains(&noise_trigram),
            "PRECISION (Design-B gate): row sharing only the 3-gram 状态用 must NOT match — bounded segmentation, not a per-char 3-gram sweep"
        );
    }

    // T5 — English under trigram becomes SUBSTRING matching (behavior change
    // from unicode61 word-matching; recorded intentionally).
    #[test]
    fn fts_match_english_substring() {
        let (_tmp, db) = db_path();
        let a = log_progress(&db, "a", Some("we are testing the parser")).unwrap(); // testing ⊃ test
        let b = log_progress(&db, "b", Some("won the contest yesterday")).unwrap(); // contest ⊃ test
        let c = log_progress(&db, "c", Some("completely unrelated body")).unwrap(); // no "test"
        let conn = open(&db).unwrap();
        let expr = build_fts_match("test").expect("english query");
        eprintln!("[T5] build_fts_match(\"test\") = {expr}");
        let hits = fts_ranked_ids(&conn, &expr, 50).unwrap();
        eprintln!("[T5] FTS hits = {hits:?}  (testing={a}, contest={b}, unrelated={c})");
        assert!(hits.contains(&a) && hits.contains(&b), "trigram: 'test' substring-matches testing & contest");
        assert!(!hits.contains(&c), "unrelated row not matched");
    }

    // T6 — observe (not a pass/fail gate): trigram's >=3-char floor means a
    // 2-char CJK query is unmatchable → no FTS term → vector path is the backstop.
    #[test]
    fn fts_match_short_cjk_observe() {
        let two = build_fts_match("缓存");
        eprintln!("[T6] build_fts_match(\"缓存\") = {two:?}  (None ⇒ no FTS term; vector backstop)");
        assert!(two.is_none(), "2-char CJK query has no trigram-matchable term (>=3 floor)");
        let three = build_fts_match("缓存层");
        eprintln!("[T6] build_fts_match(\"缓存层\") = {three:?}  (3 chars ⇒ matchable)");
        assert!(three.is_some(), "3-char CJK query is trigram-matchable");
    }

    // S3s.C.1 — first ingest with new rows sets `last_growth_ts` to the
    // same `now` as `last_ingest_ts` (proving the inserts-branch UPDATE).
    #[test]
    fn s3s_c1_first_ingest_with_new_rows_sets_growth_ts() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_c1";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc\n",
        );

        let before = current_time_ms();
        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert!(stats.events_inserted > 0);

        let (last_ingest_ts, last_growth_ts): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_ingest_ts, last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert!(
            last_growth_ts.is_some(),
            "first ingest with new rows must set growth_ts"
        );
        let growth = last_growth_ts.unwrap();
        assert!(growth >= before, "growth_ts {growth} should be >= {before}");
        assert_eq!(
            Some(last_ingest_ts),
            last_growth_ts,
            "first-ingest pass writes both timestamps to the same `now`"
        );
    }

    // S3s.C.2 — KEY semantic test: no-op pass (0 new rows) advances
    // `last_ingest_ts` but leaves `last_growth_ts` unchanged. Pins down
    // that the two timestamps have different meanings.
    #[test]
    fn s3s_c2_noop_pass_advances_ingest_ts_but_not_growth_ts() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_c2";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "x\ny\n",
        );

        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();

        let (ingest1, growth1): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_ingest_ts, last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(growth1.is_some(), "first ingest must have set growth_ts");

        // 10 ms sleep so the second pass's `now` is strictly greater
        // (current_time_ms has 1 ms resolution).
        std::thread::sleep(std::time::Duration::from_millis(10));

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(second.events_inserted, 0, "file unchanged, no inserts expected");

        let (ingest2, growth2): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_ingest_ts, last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            ingest2 > ingest1,
            "last_ingest_ts must advance every pass: {ingest1} -> {ingest2}"
        );
        assert_eq!(
            growth2, growth1,
            "last_growth_ts must NOT advance when 0 new rows were inserted"
        );
    }

    // S3s.C.3 — appending then re-ingesting advances `last_growth_ts`
    // (proving the inserts-branch UPDATE actually fires on the resume
    // path, not only on the first-touch path).
    #[test]
    fn s3s_c3_append_then_ingest_advances_growth_ts() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_c3";
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "first\n",
        );

        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();
        let growth1: Option<i64> = conn
            .query_row(
                "SELECT last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| r.get(0),
            )
            .unwrap();
        assert!(growth1.is_some());

        std::thread::sleep(std::time::Duration::from_millis(10));

        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout_path)
            .unwrap();
        f.write_all(b"second\n").unwrap();
        drop(f);

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(second.events_inserted, 1);

        let growth2: Option<i64> = conn
            .query_row(
                "SELECT last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            growth2.unwrap() > growth1.unwrap(),
            "growth_ts must advance when new content arrives: {:?} -> {:?}",
            growth1,
            growth2
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3a — transcript render + segment_thread (pure)
    // -----------------------------------------------------------------

    use serde_json::json;

    /// Helper: turn a JSON value into a compact JSONL-style payload string.
    fn payload(v: serde_json::Value) -> String {
        v.to_string()
    }

    // ---- S3a.A.1 — classification ----

    #[test]
    fn s3a_a1_classification_user_message_is_kept() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello world"}]
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::Message { role, text }) => {
                assert_eq!(role, "user");
                assert_eq!(text, "hello world");
            }
            other => panic!("expected Kept(Message), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_assistant_message_is_kept() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Alice here."}]
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::Message { role, text }) => {
                assert_eq!(role, "assistant");
                assert_eq!(text, "Alice here.");
            }
            other => panic!("expected Kept(Message assistant), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_developer_message_is_dropped() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "system-side instructions"}]
            }
        }));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    #[test]
    fn s3a_a1_classification_reasoning_is_dropped() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "thinking..."}]
            }
        }));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    #[test]
    fn s3a_a1_classification_function_call_is_kept() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "do_thing",
                "arguments": "{\"x\":1}",
                "call_id": "call_abc"
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::ToolCall {
                name,
                arguments,
                call_id,
            }) => {
                assert_eq!(name, "do_thing");
                assert_eq!(arguments, "{\"x\":1}");
                assert_eq!(call_id, "call_abc");
            }
            other => panic!("expected Kept(ToolCall), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_function_call_output_is_kept() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_abc",
                "output": {"content": "ok"}
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::ToolResult { output, .. }) => {
                assert_eq!(output, "ok");
            }
            other => panic!("expected Kept(ToolResult), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_inner_compaction_is_dropped() {
        // Inner `response_item/compaction` is the encrypted-internal kind,
        // different from the top-level `compacted` boundary below.
        let line = payload(json!({
            "type": "response_item",
            "payload": {"type": "compaction", "encrypted_content": "…opaque…"}
        }));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    #[test]
    fn s3a_a1_classification_top_level_compacted_is_boundary() {
        let line = payload(json!({
            "type": "compacted",
            "payload": {"message": "<segment summary text>"}
        }));
        match parse_line(&line) {
            ParsedLine::Boundary { summary } => {
                assert_eq!(summary, "<segment summary text>");
            }
            other => panic!("expected Boundary, got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_top_level_compacted_missing_message_defaults_empty() {
        let line = payload(json!({"type": "compacted", "payload": {}}));
        assert_eq!(
            parse_line(&line),
            ParsedLine::Boundary {
                summary: String::new()
            }
        );
    }

    #[test]
    fn s3a_a1_classification_event_msg_turn_context_session_meta_dropped() {
        for top in ["event_msg", "turn_context", "session_meta"] {
            let line = payload(json!({"type": top, "payload": {"type": "x"}}));
            assert_eq!(
                parse_line(&line),
                ParsedLine::Dropped,
                "top type {top} must be Dropped"
            );
        }
    }

    #[test]
    fn s3a_a1_classification_unknown_top_type_dropped() {
        let line = payload(json!({"type": "something_new", "payload": {"type": "x"}}));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    // ---- S3a.A.2 — extraction ----

    #[test]
    fn s3a_a2_message_multi_content_blocks_concat_text_only() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "first "},
                    // Non-text block — must be skipped, not panic.
                    {"type": "input_image", "image_url": "data:..."},
                    {"type": "output_text", "text": "second."},
                    // Junk block with no `text` field.
                    {"type": "input_text"},
                    {"type": "input_text", "text": " end"}
                ]
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::Message { role, text }) => {
                assert_eq!(role, "user");
                assert_eq!(text, "first second. end");
            }
            other => panic!("expected Kept(Message), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a2_function_call_output_via_content_items() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "c-2",
                "output": {
                    "content_items": [
                        {"type": "text", "text": "part1 "},
                        {"type": "text", "text": "part2"}
                    ]
                }
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::ToolResult {
                call_id,
                output,
            }) => {
                assert_eq!(call_id.as_deref(), Some("c-2"));
                assert_eq!(output, "part1 part2");
            }
            other => panic!("expected Kept(ToolResult), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a2_function_call_arguments_passed_through_verbatim() {
        // Arguments is a raw JSON-encoded string per protocol — we don't
        // re-parse it; the distiller sees exactly what codex stored.
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "log_progress",
                "arguments": "{\"summary\":\"hi\",\"detail\":\"world\"}",
                "call_id": "call_xyz"
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::ToolCall {
                name, arguments, ..
            }) => {
                assert_eq!(name, "log_progress");
                assert_eq!(arguments, "{\"summary\":\"hi\",\"detail\":\"world\"}");
            }
            other => panic!("expected Kept(ToolCall), got {other:?}"),
        }
    }

    // ---- S3a.A.3 — robustness (no panics) ----

    #[test]
    fn s3a_a3_invalid_json_is_dropped() {
        assert_eq!(parse_line("{not json"), ParsedLine::Dropped);
        assert_eq!(parse_line(""), ParsedLine::Dropped);
        assert_eq!(parse_line("null"), ParsedLine::Dropped);
        // Valid JSON but no `type` field.
        assert_eq!(parse_line("{\"foo\":1}"), ParsedLine::Dropped);
        // `type` of wrong shape (number).
        assert_eq!(parse_line("{\"type\":42}"), ParsedLine::Dropped);
        // response_item with no payload.
        assert_eq!(parse_line("{\"type\":\"response_item\"}"), ParsedLine::Dropped);
        // response_item with empty payload.
        assert_eq!(
            parse_line("{\"type\":\"response_item\",\"payload\":{}}"),
            ParsedLine::Dropped
        );
        // message with no role.
        let no_role = payload(json!({
            "type": "response_item",
            "payload": {"type": "message", "content": []}
        }));
        assert_eq!(parse_line(&no_role), ParsedLine::Dropped);
    }

    // ---- S3a.B.1 — rendering with role labels ----

    #[test]
    fn s3a_b1_renders_kept_items_with_role_labels() {
        let lines = vec![
            (
                1_i64,
                payload(json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "do the thing"}]
                    }
                })),
            ),
            (
                2,
                payload(json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "on it"}]
                    }
                })),
            ),
            (
                3,
                payload(json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "do_thing",
                        "arguments": "{\"x\":1}",
                        "call_id": "c1"
                    }
                })),
            ),
            (
                4,
                payload(json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "c1",
                        "output": {"content": "done"}
                    }
                })),
            ),
        ];
        let lines_ref: Vec<(i64, &str)> = lines.iter().map(|(n, s)| (*n, s.as_str())).collect();
        let segs = segment_thread(&lines_ref);
        assert_eq!(segs.len(), 1);
        assert_eq!(
            segs[0].transcript,
            "USER: do the thing\n\
             ASSISTANT: on it\n\
             TOOL CALL do_thing: {\"x\":1}\n\
             TOOL RESULT: done"
        );
    }

    // ---- S3a.C — segmentation ----

    fn user_msg(line_no: i64, text: &str) -> (i64, String) {
        (
            line_no,
            payload(json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }
            })),
        )
    }

    fn assistant_msg(line_no: i64, text: &str) -> (i64, String) {
        (
            line_no,
            payload(json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]
                }
            })),
        )
    }

    fn compacted(line_no: i64, summary: &str) -> (i64, String) {
        (
            line_no,
            payload(json!({
                "type": "compacted",
                "payload": {"message": summary}
            })),
        )
    }

    fn reasoning(line_no: i64, text: &str) -> (i64, String) {
        (
            line_no,
            payload(json!({
                "type": "response_item",
                "payload": {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": text}]
                }
            })),
        )
    }

    fn event_msg(line_no: i64) -> (i64, String) {
        (
            line_no,
            payload(json!({"type": "event_msg", "payload": {"type": "token_count"}})),
        )
    }

    fn as_refs(v: &[(i64, String)]) -> Vec<(i64, &str)> {
        v.iter().map(|(n, s)| (*n, s.as_str())).collect()
    }

    // S3a.C.1 — no compacted markers: one segment, prior_summary=None,
    // start/end equal the first/last input line_no.
    #[test]
    fn s3a_c1_no_boundary_single_segment() {
        let lines = vec![
            user_msg(5, "u1"),
            assistant_msg(6, "a1"),
            user_msg(7, "u2"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start_line_no, 5);
        assert_eq!(segs[0].end_line_no, 7);
        assert!(segs[0].prior_summary.is_none());
        assert_eq!(segs[0].transcript, "USER: u1\nASSISTANT: a1\nUSER: u2");
    }

    // S3a.C.2 — one mid-stream boundary: 2 segments, seg2.prior_summary
    // == marker.message, marker is in neither transcript, seg1.end==C,
    // seg2.start==C+1.
    #[test]
    fn s3a_c2_single_boundary_in_middle_splits_to_two_segments() {
        let lines = vec![
            user_msg(1, "u1"),
            assistant_msg(2, "a1"),
            compacted(3, "rollup-of-1-and-2"),
            user_msg(4, "u2"),
            assistant_msg(5, "a2"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 2);

        assert_eq!(segs[0].start_line_no, 1);
        assert_eq!(segs[0].end_line_no, 3);
        assert!(segs[0].prior_summary.is_none());
        assert_eq!(segs[0].transcript, "USER: u1\nASSISTANT: a1");
        assert!(
            !segs[0].transcript.contains("rollup-of-1-and-2"),
            "marker must not leak into seg1 transcript"
        );

        assert_eq!(segs[1].start_line_no, 4);
        assert_eq!(segs[1].end_line_no, 5);
        assert_eq!(segs[1].prior_summary.as_deref(), Some("rollup-of-1-and-2"));
        assert_eq!(segs[1].transcript, "USER: u2\nASSISTANT: a2");
        assert!(
            !segs[1].transcript.contains("rollup-of-1-and-2"),
            "marker must not leak into seg2 transcript either"
        );
    }

    // S3a.C.3 — multiple boundaries: ranges are contiguous and non-overlapping;
    // each segment's prior_summary is its preceding marker's message.
    #[test]
    fn s3a_c3_multiple_boundaries_produce_contiguous_ranges() {
        let lines = vec![
            user_msg(1, "u1"),
            compacted(2, "sum-A"),
            assistant_msg(3, "a1"),
            compacted(4, "sum-B"),
            user_msg(5, "u2"),
            assistant_msg(6, "a2"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 3);

        assert_eq!((segs[0].start_line_no, segs[0].end_line_no), (1, 2));
        assert!(segs[0].prior_summary.is_none());
        assert_eq!(segs[0].transcript, "USER: u1");

        assert_eq!((segs[1].start_line_no, segs[1].end_line_no), (3, 4));
        assert_eq!(segs[1].prior_summary.as_deref(), Some("sum-A"));
        assert_eq!(segs[1].transcript, "ASSISTANT: a1");

        assert_eq!((segs[2].start_line_no, segs[2].end_line_no), (5, 6));
        assert_eq!(segs[2].prior_summary.as_deref(), Some("sum-B"));
        assert_eq!(segs[2].transcript, "USER: u2\nASSISTANT: a2");

        // Contiguity: end_i + 1 == start_{i+1}.
        for win in segs.windows(2) {
            assert_eq!(win[0].end_line_no + 1, win[1].start_line_no);
        }
    }

    // S3a.C.4 — trailing boundary: no empty tail segment; the marker's
    // line_no is the previous segment's end.
    #[test]
    fn s3a_c4_trailing_boundary_does_not_produce_empty_tail_segment() {
        let lines = vec![
            user_msg(1, "u1"),
            assistant_msg(2, "a1"),
            compacted(3, "rollup"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 1, "no trailing empty segment expected");
        assert_eq!(segs[0].start_line_no, 1);
        assert_eq!(segs[0].end_line_no, 3, "boundary's line_no closes the segment");
        assert_eq!(segs[0].transcript, "USER: u1\nASSISTANT: a1");
    }

    // S3a.C.5 — Dropped lines (reasoning / event_msg) sit inside the
    // segment's line range but contribute no transcript.
    #[test]
    fn s3a_c5_dropped_lines_keep_range_but_skip_transcript() {
        let lines = vec![
            user_msg(10, "u1"),
            reasoning(11, "mid-thought"),
            event_msg(12),
            assistant_msg(13, "a1"),
            compacted(14, "sum"),
            event_msg(15),
            user_msg(16, "u2"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 2);

        assert_eq!((segs[0].start_line_no, segs[0].end_line_no), (10, 14));
        assert_eq!(
            segs[0].transcript, "USER: u1\nASSISTANT: a1",
            "reasoning + event_msg must not appear in the transcript"
        );

        assert_eq!((segs[1].start_line_no, segs[1].end_line_no), (15, 16));
        assert_eq!(segs[1].prior_summary.as_deref(), Some("sum"));
        assert_eq!(segs[1].transcript, "USER: u2");
    }

    // S3a.E.1 — module-internal: confirm the parser uses serde_json::Value
    // navigation. No codex-cli ResponseItem / RolloutItem types are imported.
    // (Grep verifies; this test just exists as the documented contract.)
    #[test]
    fn s3a_e1_render_module_uses_no_codex_protocol_types() {
        // Sentinel: a deeply unfamiliar tool kind still classifies cleanly
        // (no enum-exhaustiveness coupling to a codex-cli upstream version).
        let line = payload(json!({
            "type": "response_item",
            "payload": {"type": "future_unknown_tool", "name": "x"}
        }));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3b — extractor parse + request body + env config
    // -----------------------------------------------------------------

    /// Build an HttpExtractor for tests without going through env. Lets us
    /// inspect `build_request_body` deterministically.
    fn test_http_extractor(base_url: &str, model: &str, api_key: &str) -> HttpExtractor {
        HttpExtractor {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
        }
    }

    // ---- S3be.A — tolerant JSON parse ----

    #[test]
    fn s3be_a1_clean_json_array_parses() {
        let s = r#"[
            {"summary":"x","detail":"d","kind":"decision"},
            {"summary":"y","detail":null,"kind":"fact"}
        ]"#;
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 2);
        assert_eq!(kps[0].summary, "x");
        assert_eq!(kps[0].detail.as_deref(), Some("d"));
        assert_eq!(kps[0].kind, "decision");
        assert_eq!(kps[1].summary, "y");
        assert!(kps[1].detail.is_none());
        assert_eq!(kps[1].kind, "fact");
    }

    #[test]
    fn s3be_a2_strips_json_code_fence() {
        let s = "```json\n[{\"summary\":\"in-a-fence\",\"detail\":null,\"kind\":\"pattern\"}]\n```";
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1);
        assert_eq!(kps[0].summary, "in-a-fence");
        assert_eq!(kps[0].kind, "pattern");

        // Unlabeled ``` fence also works.
        let unlabeled =
            "```\n[{\"summary\":\"plain-fence\",\"detail\":null,\"kind\":\"fact\"}]\n```";
        let kps2 = parse_knowledge_points(unlabeled).unwrap();
        assert_eq!(kps2.len(), 1);
        assert_eq!(kps2[0].summary, "plain-fence");
    }

    #[test]
    fn s3be_a3_extracts_array_from_prose_wrapper() {
        let s = r#"Here is the JSON you asked for: [{"summary":"sliced","detail":null,"kind":"fact"}]. Hope this helps!"#;
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1);
        assert_eq!(kps[0].summary, "sliced");
    }

    #[test]
    fn s3be_a4_defaults_and_kind_normalization() {
        // Missing kind → "fact"; missing detail → None; invalid kind → "fact";
        // valid kind in different case → normalised lowercase.
        let s = r#"[
            {"summary":"missing-kind","detail":"d1"},
            {"summary":"missing-detail","kind":"failure"},
            {"summary":"bad-kind","detail":null,"kind":"banana"},
            {"summary":"upper-kind","detail":null,"kind":"DECISION"}
        ]"#;
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 4);

        assert_eq!(kps[0].kind, "fact");
        assert_eq!(kps[0].detail.as_deref(), Some("d1"));

        assert_eq!(kps[1].kind, "failure");
        assert!(kps[1].detail.is_none());

        assert_eq!(
            kps[2].kind, "fact",
            "unknown kind values fall back to 'fact'"
        );

        assert_eq!(kps[3].kind, "decision", "kind is normalised to lowercase");
    }

    #[test]
    fn s3be_a5_empty_array_and_empty_summary_skip() {
        // "[]" → Ok(empty).
        assert_eq!(parse_knowledge_points("[]").unwrap(), Vec::new());
        // Element with empty / missing summary is skipped silently.
        let s = r#"[
            {"summary":"","detail":"ignored","kind":"failure"},
            {"summary":"   ","kind":"fact"},
            {"summary":"survivor","kind":"pattern"}
        ]"#;
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1, "empty/whitespace summaries are silently dropped");
        assert_eq!(kps[0].summary, "survivor");
        assert_eq!(kps[0].kind, "pattern");
    }

    #[test]
    fn s3be_a6_total_garbage_returns_err_no_panic() {
        assert!(parse_knowledge_points("absolutely no JSON here").is_err());
        assert!(parse_knowledge_points("").is_err());
        // Top-level non-array between brackets.
        assert!(
            parse_knowledge_points("{\"summary\":\"x\"}").is_err(),
            "no '[' → Err"
        );
        // Brackets but the body between them is gibberish.
        assert!(parse_knowledge_points("[this is not JSON]").is_err());
    }

    // ---- S3be.B — request body assembly (no network) ----

    #[test]
    fn s3be_b1_body_with_prior_summary_includes_prior_block() {
        let extractor = test_http_extractor("https://api.example/v1", "qwen-test", "sk-test");
        let body = extractor.build_request_body("USER: hi", Some("earlier we agreed X."));

        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("qwen-test"));
        let temperature = body.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!(
            (0.0..=0.5).contains(&temperature),
            "temperature stays low: got {temperature}"
        );

        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 2);

        assert_eq!(messages[0].get("role").and_then(|v| v.as_str()), Some("system"));
        assert_eq!(
            messages[0].get("content").and_then(|v| v.as_str()),
            Some(EXTRACTION_PROMPT),
            "system message must be EXTRACTION_PROMPT verbatim"
        );

        assert_eq!(messages[1].get("role").and_then(|v| v.as_str()), Some("user"));
        let user_content = messages[1].get("content").and_then(|v| v.as_str()).unwrap();
        assert!(user_content.contains("[PRIOR CONTEXT SUMMARY]"));
        assert!(user_content.contains("earlier we agreed X."));
        assert!(user_content.contains("[TRANSCRIPT]"));
        assert!(user_content.contains("USER: hi"));
        // Prior block must come before the transcript block.
        let prior_idx = user_content.find("[PRIOR CONTEXT SUMMARY]").unwrap();
        let transcript_idx = user_content.find("[TRANSCRIPT]").unwrap();
        assert!(
            prior_idx < transcript_idx,
            "prior must precede transcript"
        );
    }

    #[test]
    fn s3be_b1_body_without_prior_summary_omits_prior_block() {
        let extractor = test_http_extractor("https://api.example/v1", "model-x", "k");
        let body = extractor.build_request_body("USER: hi", None);

        let user_content = body.get("messages").and_then(|v| v.as_array()).unwrap()[1]
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(
            !user_content.contains("[PRIOR CONTEXT SUMMARY]"),
            "no PRIOR block when prior_summary is None: got {user_content:?}"
        );
        assert!(user_content.contains("[TRANSCRIPT]"));
        assert!(user_content.contains("USER: hi"));

        // model field propagates from the extractor.
        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("model-x"));
    }

    // ---- S3be.C — from_env ----

    // env is process-global; serialise these tests against EVERY other
    // env-mutating test in the same binary. `paths::ENV_LOCK` is the
    // shared mutex used by paths::tests for HOME / USERPROFILE mutation;
    // reusing it here means our HOME-touching tests below (S3cfg.A.*)
    // can't race with the path tests.
    fn with_env<F>(values: &[(&str, Option<&str>)], f: F)
    where
        F: FnOnce(),
    {
        let _g = crate::paths::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Snapshot + override + run + restore. set_var / remove_var are
        // unsafe in Rust 2024+; we hold ENV_LOCK so this is the only env
        // writer for the duration of `f`.
        let prior: Vec<(&str, Option<String>)> = values
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in values {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        f();
        for (k, prev) in prior {
            match prev {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    #[test]
    fn s3be_c1_from_env_all_set_returns_some() {
        with_env(
            &[
                ("OPENCRAB_DISTILLER_BASE_URL", Some("https://test.example/v1")),
                ("OPENCRAB_DISTILLER_MODEL", Some("qwen-test")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("sk-test")),
            ],
            || {
                let extractor = HttpExtractor::from_env().expect("all three vars set → Some");
                assert_eq!(extractor.base_url, "https://test.example/v1");
                assert_eq!(extractor.model, "qwen-test");
                assert_eq!(extractor.api_key, "sk-test");
            },
        );
    }

    #[test]
    fn s3be_c1_from_env_missing_any_returns_none() {
        for missing in [
            "OPENCRAB_DISTILLER_BASE_URL",
            "OPENCRAB_DISTILLER_MODEL",
            "OPENCRAB_DISTILLER_API_KEY",
        ] {
            let mut env: Vec<(&str, Option<&str>)> = vec![
                ("OPENCRAB_DISTILLER_BASE_URL", Some("https://x/v1")),
                ("OPENCRAB_DISTILLER_MODEL", Some("m")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("k")),
            ];
            for slot in env.iter_mut() {
                if slot.0 == missing {
                    slot.1 = None;
                }
            }
            with_env(&env, || {
                assert!(
                    HttpExtractor::from_env().is_none(),
                    "missing {missing} → None"
                );
            });
        }
    }

    #[test]
    fn s3be_c1_from_env_empty_string_treated_as_missing() {
        // Empty / whitespace-only values are treated the same as missing
        // (avoid surfacing "I see a base_url but it's blank, what now?").
        with_env(
            &[
                ("OPENCRAB_DISTILLER_BASE_URL", Some("   ")),
                ("OPENCRAB_DISTILLER_MODEL", Some("m")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("k")),
            ],
            || {
                assert!(HttpExtractor::from_env().is_none());
            },
        );
    }

    // ---- S3be.E.2 — #[ignore]'d live test (manual `cargo test -- --ignored`) ----

    /// Hits the real endpoint configured in `OPENCRAB_DISTILLER_*`. Not run
    /// in CI; run manually with:
    ///   `cargo test --bin opencrab-memory-mcp -- --ignored s3be_e2`
    /// after exporting the env vars. `OPENCRAB_DISTILLER_BASE_URL` must
    /// include the provider's API version segment (e.g.
    /// `https://api.openai.com/v1`, `https://dashscope.aliyuncs.com/compatible-mode/v1`);
    /// the extractor appends `/chat/completions` and nothing else.
    #[test]
    #[ignore]
    fn s3be_e2_live_extract_hits_real_endpoint() {
        let extractor = HttpExtractor::load()
            .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(extractor.extract(
            "USER: I keep crashing with 'duplicate column name: origin' when re-running cargo test.\n\
             ASSISTANT: ALTER TABLE ADD COLUMN is not idempotent — gate the step on PRAGMA user_version inside an IMMEDIATE transaction so a re-run is a no-op.",
            None,
        ));
        let kps = result.expect("live extract should succeed against the configured provider");
        eprintln!("[live] extracted {} knowledge point(s)", kps.len());
        for kp in &kps {
            eprintln!("[live]   kind={} summary={}", kp.kind, kp.summary);
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3b-distill — distill_once + URL/prompt fixes (S3d)
    // -----------------------------------------------------------------

    /// Block on a future on a temporary current_thread runtime — same flavor
    /// as the production bin so async behaviour matches.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    // ---- FakeExtractor — async trait impl, no network ----

    #[derive(Clone)]
    enum FakeResult {
        Ok(Vec<KnowledgePoint>),
        Transient(String),
        Client(String),
        Parse(String),
    }

    struct FakeExtractor {
        queue: std::sync::Mutex<std::collections::VecDeque<FakeResult>>,
        fallback: std::sync::Mutex<FakeResult>,
        calls: std::sync::Mutex<Vec<(String, Option<String>)>>,
    }

    impl FakeExtractor {
        fn always(result: FakeResult) -> Self {
            Self {
                queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
                fallback: std::sync::Mutex::new(result),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_sequence(seq: Vec<FakeResult>, fallback: FakeResult) -> Self {
            Self {
                queue: std::sync::Mutex::new(seq.into_iter().collect()),
                fallback: std::sync::Mutex::new(fallback),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn calls_snapshot(&self) -> Vec<(String, Option<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Extractor for FakeExtractor {
        async fn extract(
            &self,
            transcript: &str,
            prior_summary: Option<&str>,
        ) -> Result<Vec<KnowledgePoint>, ExtractError> {
            self.calls.lock().unwrap().push((
                transcript.to_string(),
                prior_summary.map(|s| s.to_string()),
            ));
            let next = self.queue.lock().unwrap().pop_front();
            let result = next.unwrap_or_else(|| self.fallback.lock().unwrap().clone());
            match result {
                FakeResult::Ok(kps) => Ok(kps),
                FakeResult::Transient(m) => Err(ExtractError::HttpTransient(m)),
                FakeResult::Client(m) => Err(ExtractError::HttpClient(m)),
                FakeResult::Parse(m) => Err(ExtractError::Parse(m)),
            }
        }
    }

    // ---- Seeding helpers (build raw_thread + raw_event rows) ----

    fn user_msg_str(text: &str) -> String {
        payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}]
            }
        }))
    }

    fn assistant_msg_str(text: &str) -> String {
        payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }
        }))
    }

    fn compacted_str(summary: &str) -> String {
        payload(json!({
            "type": "compacted",
            "payload": {"message": summary}
        }))
    }

    fn reasoning_str(text: &str) -> String {
        payload(json!({
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": text}]
            }
        }))
    }

    fn event_msg_str() -> String {
        payload(json!({"type": "event_msg", "payload": {"type": "token_count"}}))
    }

    fn seed_distill_thread(
        conn: &Connection,
        thread_id: &str,
        last_line_no: i64,
        last_distilled_line_no: i64,
        last_growth_ts: Option<i64>,
        project_hash: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO raw_thread \
             (thread_id, agent_id, project_hash, source_path, \
              first_seen_ts, last_ingest_ts, last_offset, last_line_no, \
              last_distilled_line_no, last_growth_ts) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                thread_id,
                "agent-test",
                project_hash,
                "/tmp/r.jsonl",
                0_i64,
                0_i64,
                0_i64,
                last_line_no,
                last_distilled_line_no,
                last_growth_ts,
            ],
        )
        .unwrap();
    }

    fn seed_raw_event(conn: &Connection, thread_id: &str, line_no: i64, payload_str: &str) {
        conn.execute(
            "INSERT INTO raw_event(thread_id, line_no, payload, ingested_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![thread_id, line_no, payload_str, 0_i64],
        )
        .unwrap();
    }

    fn read_distilled_cursor(conn: &Connection, thread_id: &str) -> i64 {
        conn.query_row(
            "SELECT last_distilled_line_no FROM raw_thread WHERE thread_id = ?1",
            params![thread_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    // ==== A — trigger gate ====

    // S3d.A.1 — not idle AND pending < MAX → no trigger.
    #[test]
    fn s3d_a1_not_idle_low_pending_does_not_trigger() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        // last_growth_ts very recent → not idle.
        seed_distill_thread(&conn, "thr1", 3, 0, Some(now_ms - 1_000), None);
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("hello"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("hi"));
        seed_raw_event(&conn, "thr1", 3, &user_msg_str("ok"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.threads_seen, 1);
        assert_eq!(stats.threads_triggered, 0);
        assert_eq!(stats.points_written, 0);
        assert_eq!(fake.call_count(), 0, "must not call LLM when not triggered");
        assert_eq!(read_distilled_cursor(&conn, "thr1"), 0, "cursor must not move");
    }

    // S3d.A.2 — idle (now_ms - growth > DISTILL_IDLE_MS) → trigger.
    #[test]
    fn s3d_a2_idle_triggers_distill() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            3,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("hello"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("hi"));
        seed_raw_event(&conn, "thr1", 3, &user_msg_str("ok"));

        let kp = KnowledgePoint {
            summary: "x".into(),
            detail: None,
            kind: "fact".into(),
        };
        let fake = FakeExtractor::always(FakeResult::Ok(vec![kp]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.threads_triggered, 1);
        assert!(fake.call_count() >= 1, "idle thread should call LLM");
    }

    // S3d.A.3 — not idle BUT pending > MAX → trigger (safety valve).
    #[test]
    fn s3d_a3_pending_overflow_triggers_even_when_not_idle() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        let last_line = DISTILL_MAX_PENDING + 1;
        // last_growth_ts very recent → not idle, but pending > MAX.
        seed_distill_thread(
            &conn,
            "thr1",
            last_line,
            0,
            Some(now_ms - 1_000),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("flooding the buffer"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.threads_triggered, 1);
    }

    // ==== B — write path ====

    // S3d.B.1 — extractor returns 2 KPs → both land in `log` with
    // origin='distill', project_hash propagated, kind preserved; cursor
    // advances to segment end; detail is FTS-searchable.
    #[test]
    fn s3d_b1_writes_kps_with_distill_origin_kind_and_project_hash() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            3,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            Some("proj-abc123"),
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("how do I do X?"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("by Y because Z"));
        seed_raw_event(&conn, "thr1", 3, &assistant_msg_str("done"));

        let kps = vec![
            KnowledgePoint {
                summary: "Use Y for X".into(),
                detail: Some("Because Z applies in this repo's setup".into()),
                kind: "decision".into(),
            },
            KnowledgePoint {
                summary: "Cache invalidation note".into(),
                detail: Some("a fenestration sentinel for FTS".into()),
                kind: "fact".into(),
            },
        ];
        let fake = FakeExtractor::always(FakeResult::Ok(kps));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.points_written, 2);

        let mut stmt = conn
            .prepare("SELECT summary, detail, origin, project_hash, kind FROM log ORDER BY id")
            .unwrap();
        let rows: Vec<(String, Option<String>, String, Option<String>, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        for (_, _, origin, project_hash, kind) in &rows {
            assert_eq!(origin, "distill");
            assert_eq!(project_hash.as_deref(), Some("proj-abc123"));
            assert!(kind.is_some());
        }
        assert_eq!(rows[0].0, "Use Y for X");
        assert_eq!(rows[0].4.as_deref(), Some("decision"));
        assert_eq!(rows[1].0, "Cache invalidation note");
        assert_eq!(rows[1].4.as_deref(), Some("fact"));

        // FTS round-trip on a unique token in `detail`.
        let hits = block_on(search(&db, None, "fenestration", 6)).unwrap();
        assert!(!hits.is_empty(), "distilled `detail` must be FTS-searchable");

        assert_eq!(read_distilled_cursor(&conn, "thr1"), 3);
    }

    // ==== C — segments + prior_summary ====

    // S3d.C.1 — thread with a mid-stream compacted boundary: extractor
    // gets two calls; the second carries the marker's `message` as
    // `prior_summary`; cursor lands on the last line.
    #[test]
    fn s3d_c1_compacted_thread_passes_prior_summary_to_extractor() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            5,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("u1"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("a1"));
        seed_raw_event(&conn, "thr1", 3, &compacted_str("rollup-summary"));
        seed_raw_event(&conn, "thr1", 4, &user_msg_str("u2"));
        seed_raw_event(&conn, "thr1", 5, &assistant_msg_str("a2"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let _ = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        let calls = fake.calls_snapshot();
        assert_eq!(calls.len(), 2, "two segments → two extract calls");
        assert!(
            calls[0].1.is_none(),
            "first segment has no prior_summary (no boundary before it)"
        );
        assert_eq!(
            calls[1].1.as_deref(),
            Some("rollup-summary"),
            "second segment carries the marker's message as prior_summary"
        );
        assert_eq!(read_distilled_cursor(&conn, "thr1"), 5);
    }

    // S3d.C.2 — segment whose lines are all Dropped (reasoning + event_msg)
    // → LLM is NOT called, but the cursor still advances past the
    // segment so we don't look at these lines next pass.
    #[test]
    fn s3d_c2_empty_transcript_segment_skips_llm_but_advances_cursor() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            3,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &reasoning_str("internal thought"));
        seed_raw_event(&conn, "thr1", 2, &event_msg_str());
        seed_raw_event(&conn, "thr1", 3, &reasoning_str("more thinking"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(
            fake.call_count(),
            0,
            "all-Dropped segment must not invoke the LLM"
        );
        assert_eq!(stats.segments_processed, 1, "the segment still counts");
        assert_eq!(
            read_distilled_cursor(&conn, "thr1"),
            3,
            "cursor advances past the empty segment so we don't see it again"
        );
        let log_count: i64 = count_rows(&conn, "SELECT count(*) FROM log");
        assert_eq!(log_count, 0);
    }

    // ==== D — error strategy ====

    // S3d.D.1 — transient error: 0 log rows, cursor stays put (retry on
    // the next pass).
    #[test]
    fn s3d_d1_transient_error_does_not_advance_cursor() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            2,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("u"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("a"));

        let fake = FakeExtractor::always(FakeResult::Transient("503 backend".into()));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.transient_errors, 1);
        assert_eq!(stats.points_written, 0);
        assert_eq!(
            read_distilled_cursor(&conn, "thr1"),
            0,
            "cursor must stay put on transient — next pass retries"
        );
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM log"), 0);
    }

    // S3d.D.2 — client error: same handling as transient (cursor stays,
    // log empty), but counted separately for visibility.
    #[test]
    fn s3d_d2_client_error_does_not_advance_cursor() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            2,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("u"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("a"));

        let fake = FakeExtractor::always(FakeResult::Client("401 unauthorized".into()));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.client_errors, 1);
        assert_eq!(stats.points_written, 0);
        assert_eq!(read_distilled_cursor(&conn, "thr1"), 0);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM log"), 0);
    }

    // S3d.D.3 — parse error: 0 log rows for THAT segment, cursor
    // ADVANCES past it (so we don't loop on garbage); a subsequent
    // segment in the same thread is still processed normally.
    #[test]
    fn s3d_d3_parse_error_advances_cursor_past_segment_continues_to_next() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            5,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("u1"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("a1"));
        seed_raw_event(&conn, "thr1", 3, &compacted_str("rollup"));
        seed_raw_event(&conn, "thr1", 4, &user_msg_str("u2"));
        seed_raw_event(&conn, "thr1", 5, &assistant_msg_str("a2"));

        let kp = KnowledgePoint {
            summary: "from seg2".into(),
            detail: None,
            kind: "fact".into(),
        };
        let fake = FakeExtractor::with_sequence(
            vec![
                FakeResult::Parse("LLM emitted prose-only".into()),
                FakeResult::Ok(vec![kp]),
            ],
            FakeResult::Ok(vec![]),
        );
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.parse_skips, 1, "exactly one parse skip");
        assert_eq!(stats.points_written, 1, "second segment's KP still landed");

        assert_eq!(
            read_distilled_cursor(&conn, "thr1"),
            5,
            "cursor advanced past BOTH the parse-skipped segment AND the good one"
        );

        let log_count: i64 = count_rows(&conn, "SELECT count(*) FROM log");
        assert_eq!(log_count, 1);
        let (summary, kind): (String, Option<String>) = conn
            .query_row(
                "SELECT summary, kind FROM log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(summary, "from seg2");
        assert_eq!(kind.as_deref(), Some("fact"));
    }

    // ==== E — idempotency ====

    // S3d.E.1 — a fully-distilled thread (last_distilled == last_line) is
    // not even surfaced by the candidate query, so the LLM is never called.
    #[test]
    fn s3d_e1_fully_distilled_thread_does_not_re_trigger() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            5,
            5,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        for i in 1..=5_i64 {
            seed_raw_event(&conn, "thr1", i, &user_msg_str(&format!("u{i}")));
        }
        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.threads_seen, 0);
        assert_eq!(stats.threads_triggered, 0);
        assert_eq!(fake.call_count(), 0);
    }

    // ==== F — truncation ====

    // S3d.F.1 — a segment whose transcript exceeds DISTILL_MAX_TRANSCRIPT_CHARS
    // is delivered to the extractor with the head dropped and the marker
    // prepended; total chars never exceed the cap.
    #[test]
    fn s3d_f1_long_transcript_is_truncated_with_marker() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            2,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        let huge = "X".repeat(DISTILL_MAX_TRANSCRIPT_CHARS + 1_000);
        seed_raw_event(&conn, "thr1", 1, &user_msg_str(&huge));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("tail message"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let _ = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        let mut calls = fake.calls_snapshot();
        let last = calls.pop().expect("LLM should have been called once");
        let transcript = last.0;
        let char_count = transcript.chars().count();
        assert!(
            char_count <= DISTILL_MAX_TRANSCRIPT_CHARS,
            "got {char_count} chars, max {DISTILL_MAX_TRANSCRIPT_CHARS}"
        );
        assert!(
            transcript.starts_with("[...earlier content truncated...]"),
            "must start with truncation marker; head was: {:?}",
            transcript.chars().take(60).collect::<String>()
        );
        // Tail content survives.
        assert!(transcript.ends_with("tail message"));
    }

    // ==== G — migration v3→v4 + full v0→v4 + log_progress untouched ====

    // S3d.G.1 — v3 DB upgrades to v4: `log.kind` (nullable) gets added;
    // pre-existing log rows have NULL kind.
    #[test]
    fn s3d_g1_upgrades_v3_db_to_v4_adds_log_kind_nullable() {
        let (_tmp, db) = db_path();
        build_v2_db(&db);
        {
            // Hand-roll the v3 step on top of the v2-shape DB.
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "ALTER TABLE raw_thread ADD COLUMN last_distilled_line_no INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE raw_thread ADD COLUMN last_growth_ts INTEGER;
                 PRAGMA user_version = 3;",
            )
            .unwrap();
            // Pre-v4 log row: no kind column to set.
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (?1, ?2, ?3, ?4)",
                params![100_i64, "old summary", "old detail", "self"],
            )
            .unwrap();
        }

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        let cols = log_columns(&conn);
        assert!(cols.contains("kind"), "v4 must add the `kind` column to log");

        let (summary, detail, origin, kind): (String, Option<String>, String, Option<String>) =
            conn.query_row(
                "SELECT summary, detail, origin, kind FROM log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(summary, "old summary");
        assert_eq!(detail.as_deref(), Some("old detail"));
        assert_eq!(origin, "self");
        assert!(
            kind.is_none(),
            "pre-v4 row picks up kind = NULL on upgrade"
        );
    }

    // S3d.G.2 — full v0 → v1 → v2 → v3 → v4 chain in one open().
    #[test]
    fn s3d_g2_upgrades_v0_db_through_full_chain_to_v4() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        // log: v1 cols + v4 `kind`.
        assert!(log_columns(&conn).contains("kind"));
        // raw_thread: v2 + v3 cols.
        assert_eq!(
            table_columns(&conn, "raw_thread"),
            expected_raw_thread_columns()
        );
        // v0 log rows preserved.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, seeded.len());
    }

    // S3d.G.3 — log_progress (user-side) keeps `kind = NULL`. The
    // signature is unchanged; only the distiller writes a kind.
    #[test]
    fn s3d_g3_log_progress_writes_with_null_kind_unchanged() {
        let (_tmp, db) = db_path();
        let id = log_progress(&db, "user note", Some("by hand")).unwrap();
        let conn = open(&db).unwrap();
        let (origin, kind): (String, Option<String>) = conn
            .query_row(
                "SELECT origin, kind FROM log WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin, "self");
        assert!(
            kind.is_none(),
            "log_progress writes don't set kind (signature unchanged)"
        );
    }

    // ==== H — S3b-extract fix regressions ====

    // S3d.H.1 — chat_completions_url does NOT inject `/v1`; it appends
    // exactly `/chat/completions` (handling a trailing slash on base).
    #[test]
    fn s3d_h1_chat_completions_url_does_not_inject_v1() {
        let e1 = test_http_extractor("https://api.openai.com/v1", "m", "k");
        assert_eq!(
            e1.chat_completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        let e2 = test_http_extractor("https://api.openai.com/v1/", "m", "k");
        assert_eq!(
            e2.chat_completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        // Anti-regression: no double `/v1/v1` even if the base ended with /.
        assert!(!e1.chat_completions_url().contains("/v1/v1"));
        assert!(!e2.chat_completions_url().contains("/v1/v1"));
        // A non-v1-style base (e.g. dashscope) keeps its own version prefix.
        let e3 = test_http_extractor(
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "qwen-max",
            "sk-test",
        );
        assert_eq!(
            e3.chat_completions_url(),
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
        );
    }

    // S3d.H.2 — PROMPT has ≥2 few-shot examples plus an empty-output demo,
    // and the OUTPUT FORMAT section mentions the 0–5 soft cap.
    #[test]
    fn s3d_h2_prompt_has_multiple_examples_and_empty_result_demo() {
        // The failure example (existing).
        assert!(
            EXTRACTION_PROMPT.contains("\"kind\":\"failure\""),
            "Example 1 (failure) is missing"
        );
        // The decision example (S3-distill addition).
        assert!(
            EXTRACTION_PROMPT.contains("\"kind\":\"decision\""),
            "Example 2 (decision) is missing"
        );
        // The empty-result demo: contains the literal user/assistant
        // pair AND shows `[]` as the expected output.
        assert!(
            EXTRACTION_PROMPT.contains("thanks, that worked!"),
            "Example 3 (empty result) user line is missing"
        );
        assert!(
            EXTRACTION_PROMPT.contains("Glad it helped!"),
            "Example 3 (empty result) assistant line is missing"
        );
        // Soft cap line (the en-dash variant the spec uses).
        assert!(
            EXTRACTION_PROMPT.contains("0–5"),
            "Soft cap '0–5 points' is missing from OUTPUT FORMAT"
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3-wire — shrink-resets-distill + end-to-end ignored
    // -----------------------------------------------------------------

    // S3w.A.1 — when the ingester detects a shrunk file and DELETEs the
    // raw_event rows, it must also reset `last_distilled_line_no` to 0.
    // Otherwise the distiller would skip re-distilling the new file
    // content under the assumption "I've already consumed lines 1..N",
    // silently going out of sync.
    #[test]
    fn s3w_a1_shrink_resets_distill_cursor_and_re_reads_file() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_shrink_distill";
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc\nd\ne\n", // 5 lines, 10 bytes
        );
        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();

        // Simulate the distiller having consumed every line.
        conn.execute(
            "UPDATE raw_thread SET last_distilled_line_no = ?1 WHERE thread_id = ?2",
            params![5_i64, SAMPLE_THREAD_UUID],
        )
        .unwrap();

        // Truncate the file to fewer (and DIFFERENT) lines.
        std::fs::write(&rollout_path, "x\ny\n").unwrap();

        let _second = ingest_once(&conn, &scan_root, agent_id).unwrap();

        // raw_event now matches the file exactly (orphans gone, no dups).
        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![(1, "x".to_string()), (2, "y".to_string())]
        );

        // Ingester cursor + distill cursor:
        let (last_offset, last_line_no, last_distilled): (i64, i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no, last_distilled_line_no \
                 FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(last_offset, 4, "ingester cursor lands on the shrunk EOF");
        assert_eq!(last_line_no, 2);
        assert_eq!(
            last_distilled, 0,
            "shrink MUST reset last_distilled_line_no so the new content gets re-distilled"
        );
    }

    // S3w.C.1 — end-to-end ingest→distill against the real configured
    // provider. Not in CI; run manually with:
    //   `cargo test --bin opencrab-memory-mcp -- --ignored s3w_c1`
    // with `OPENCRAB_DISTILLER_{BASE_URL,MODEL,API_KEY}` exported.
    #[test]
    #[ignore]
    fn s3w_c1_e2e_ingest_then_distill_writes_distill_origin_log_row() {
        let extractor = HttpExtractor::load()
            .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");

        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_s3w_c1";

        // A small but rich transcript: a real failure-with-fix the LLM
        // can almost certainly extract.
        let transcript_lines = vec![
            user_msg_str("My cargo test keeps panicking — what should I check?"),
            assistant_msg_str("I'll look at the failure."),
            assistant_msg_str(
                "The panic is 'SqliteFailure: duplicate column name: origin' — \
                 ALTER TABLE ADD COLUMN ran twice. The fix is to wrap the step in \
                 PRAGMA user_version gating inside an IMMEDIATE transaction so a \
                 re-run is a no-op.",
            ),
        ];
        let content = transcript_lines.join("\n") + "\n";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            &content,
        );

        let conn = open(&memory_db).unwrap();
        let ingest_stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert!(ingest_stats.events_inserted >= 3);

        // Force the idle trigger by handing distill_once a now_ms far
        // ahead of the just-set last_growth_ts.
        let now_ms = current_time_ms() + DISTILL_IDLE_MS + 60_000;
        let stats = block_on(distill_once(&conn, &extractor, now_ms)).unwrap();

        assert_eq!(stats.threads_seen, 1);
        assert_eq!(stats.threads_triggered, 1);
        eprintln!("[live] distill stats: {stats:?}");

        let distill_rows: i64 = count_rows(
            &conn,
            "SELECT count(*) FROM log WHERE origin = 'distill'",
        );
        assert!(
            distill_rows >= 1,
            "the live extractor should have produced at least one distilled row"
        );
        let mut stmt = conn
            .prepare(
                "SELECT summary, detail, kind FROM log \
                 WHERE origin = 'distill' ORDER BY id",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap()
        {
            let (summary, detail, kind) = row.unwrap();
            eprintln!(
                "[live] distilled: kind={:?} summary={} detail={:?}",
                kind, summary, detail
            );
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3-cfg — env / file credential loading + think-strip
    // -----------------------------------------------------------------

    /// Write a `distiller.json` under a temp HOME's `.opencrab/` dir so
    /// the file fallback in `HttpExtractor::load()` finds it.
    fn write_distiller_json(tmp: &std::path::Path, base_url: &str, model: &str, api_key: &str) {
        let opencrab_dir = tmp.join(".opencrab");
        std::fs::create_dir_all(&opencrab_dir).unwrap();
        let body = serde_json::json!({
            "base_url": base_url,
            "model": model,
            "api_key": api_key,
        })
        .to_string();
        std::fs::write(opencrab_dir.join("distiller.json"), body).unwrap();
    }

    // S3cfg.A.1 — env vars win over a (different) on-disk file.
    #[test]
    fn s3cfg_a1_env_present_takes_priority_over_file() {
        let tmp = tempfile::tempdir().unwrap();
        // File holds values we MUST NOT see in the loaded extractor.
        write_distiller_json(
            tmp.path(),
            "https://this-must-not-win.example/v1",
            "wrong-model",
            "sk-wrong",
        );

        let home_str = tmp.path().to_str().unwrap();
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", Some("https://env.example/v1")),
                ("OPENCRAB_DISTILLER_MODEL", Some("env-model")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("sk-env")),
            ],
            || {
                let extractor = HttpExtractor::load().expect("env should produce Some");
                assert_eq!(extractor.base_url, "https://env.example/v1");
                assert_eq!(extractor.model, "env-model");
                assert_eq!(extractor.api_key, "sk-env");
            },
        );
    }

    // S3cfg.A.2 — env absent → file fallback wins.
    #[test]
    fn s3cfg_a2_env_absent_falls_back_to_distiller_json() {
        let tmp = tempfile::tempdir().unwrap();
        write_distiller_json(
            tmp.path(),
            "https://file-fallback.example/v1",
            "file-model",
            "sk-file",
        );

        let home_str = tmp.path().to_str().unwrap();
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", None),
                ("OPENCRAB_DISTILLER_MODEL", None),
                ("OPENCRAB_DISTILLER_API_KEY", None),
            ],
            || {
                let extractor =
                    HttpExtractor::load().expect("file fallback should produce Some");
                assert_eq!(extractor.base_url, "https://file-fallback.example/v1");
                assert_eq!(extractor.model, "file-model");
                assert_eq!(extractor.api_key, "sk-file");
            },
        );
    }

    // S3cfg.A.3 — neither env nor file → None.
    #[test]
    fn s3cfg_a3_neither_env_nor_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        // Don't create distiller.json — even the dir is absent.
        let home_str = tmp.path().to_str().unwrap();
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", None),
                ("OPENCRAB_DISTILLER_MODEL", None),
                ("OPENCRAB_DISTILLER_API_KEY", None),
            ],
            || {
                assert!(
                    HttpExtractor::load().is_none(),
                    "no env + no file → distillation disabled"
                );
            },
        );
    }

    // S3cfg.B.1 — `<think>…</think>` blocks are stripped before the JSON
    // array is sliced out. The think text contains `[` that would
    // otherwise hijack `extract_json_array_slice`.
    #[test]
    fn s3cfg_b1_strips_paired_think_blocks_before_parsing() {
        let s = "<think>let me output [stuff] and reason</think>\n\
                 [{\"summary\":\"s\",\"detail\":null,\"kind\":\"fact\"}]";
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1, "exactly one KP after think-strip");
        assert_eq!(kps[0].summary, "s");
        assert_eq!(kps[0].kind, "fact");
    }

    // S3cfg.B.1 (extra) — multiple paired blocks; all paired, all stripped.
    #[test]
    fn s3cfg_b1_strips_multiple_paired_think_blocks() {
        let s = "<think>first thinking with [</think>some text\n\
                 <think>second [thinking step</think>\n\
                 [{\"summary\":\"sharp\",\"kind\":\"decision\"}]";
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1);
        assert_eq!(kps[0].summary, "sharp");
        assert_eq!(kps[0].kind, "decision");
    }

    // S3cfg.B.2 — no think blocks → identical to pre-S3cfg parse behaviour.
    #[test]
    fn s3cfg_b2_no_think_blocks_unchanged_behavior() {
        let clean = r#"[{"summary":"plain","detail":"d","kind":"pattern"}]"#;
        let kps = parse_knowledge_points(clean).unwrap();
        assert_eq!(kps.len(), 1);
        assert_eq!(kps[0].summary, "plain");
        assert_eq!(kps[0].detail.as_deref(), Some("d"));
        assert_eq!(kps[0].kind, "pattern");

        // Fenced (S3be regression coverage): the think-strip pass should
        // be a no-op when there are no think tags, so the fence path
        // continues to work.
        let fenced = "```json\n[{\"summary\":\"in-fence\",\"kind\":\"fact\"}]\n```";
        let kps2 = parse_knowledge_points(fenced).unwrap();
        assert_eq!(kps2.len(), 1);
        assert_eq!(kps2[0].summary, "in-fence");
    }

    // S3cfg.B.1 (defensive) — unclosed `<think>` left untouched; the
    // parse fails naturally (no JSON afterwards). No panic.
    #[test]
    fn s3cfg_b1_unclosed_think_block_left_untouched_and_parse_errs() {
        let s = "<think>this never closes, and there is no array";
        assert!(parse_knowledge_points(s).is_err());
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3-val — one-shot quality look at a REAL rollout
    // -----------------------------------------------------------------

    /// Drive the full S2 ingest → S3 distill chain against a *real*
    /// agent's rollout files. Not a CI test — runs only with:
    ///   `cargo test --bin opencrab-memory-mcp -- --ignored \
    ///        s3val_real_rollout_distill --nocapture`
    /// (and the distiller must be configured via env or
    /// `~/.opencrab/distiller.json`).
    ///
    /// The agent below was selected because its `team_sessions/`
    /// holds rollouts with substantive content (a real DB-choice
    /// decision the PM was asked to record), not just kickoff stubs.
    /// Re-point at a different agent if this one is no longer present.
    #[test]
    #[ignore]
    fn s3val_real_rollout_distill() {
        let extractor = HttpExtractor::load()
            .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");

        let home = std::env::var("HOME").expect("HOME");
        let scan_root: std::path::PathBuf = std::path::PathBuf::from(home).join(
            ".opencrab/agents/agent_1e17677e-4a5b-40db-8113-3aeba71635d4/team_sessions",
        );
        assert!(
            scan_root.exists(),
            "no scan_root at {} — re-point at a real agent dir or skip",
            scan_root.display()
        );

        let tmp = tempfile::tempdir().unwrap();
        let memory_db = tmp.path().join("memory.db");

        let conn = open(&memory_db).unwrap();
        let ingest = ingest_once(&conn, &scan_root, "agent_s3val").unwrap();
        eprintln!("[s3val] ingest stats: {ingest:?}");

        // Thread-scale glimpse: how big is each thread?
        let mut stmt = conn
            .prepare(
                "SELECT thread_id, last_line_no FROM raw_thread ORDER BY thread_id",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
        {
            let (tid, lines) = row.unwrap();
            eprintln!("[s3val] thread {} = {} lines", tid, lines);
        }

        // Force the idle trigger so every thread distills this pass.
        let far_future = current_time_ms() + DISTILL_IDLE_MS + 60_000;
        let stats = block_on(distill_once(&conn, &extractor, far_future)).unwrap();
        eprintln!("[s3val] distill stats: {stats:?}");

        // Dump every distilled row verbatim.
        let mut stmt = conn
            .prepare(
                "SELECT id, summary, detail, kind FROM log \
                 WHERE origin = 'distill' ORDER BY id",
            )
            .unwrap();
        let rows: Vec<(i64, String, Option<String>, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        eprintln!("[s3val] total distilled rows = {}", rows.len());
        for (id, summary, detail, kind) in &rows {
            eprintln!("[s3val] -------- log id={} kind={:?}", id, kind);
            eprintln!("[s3val]   summary: {}", summary);
            eprintln!("[s3val]   detail:  {:?}", detail);
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 4-schema — sqlite-vec registration + log_vec (v5)
    // -----------------------------------------------------------------

    /// Frozen v4 final form: log v1 + provenance + idx_log_ts + `kind`,
    /// raw_thread v3 (14 cols), raw_event, idx_raw_event_thread.
    /// Used by the v4→v5 upgrade test so it doesn't depend on live
    /// migrate code.
    const V4_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT,
        kind         TEXT
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
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id              TEXT PRIMARY KEY,
        agent_id               TEXT,
        team_id                TEXT,
        project_hash           TEXT,
        source                 TEXT,
        parent_thread_id       TEXT,
        cwd                    TEXT,
        source_path            TEXT NOT NULL,
        first_seen_ts          INTEGER NOT NULL,
        last_ingest_ts         INTEGER NOT NULL,
        last_offset            INTEGER NOT NULL DEFAULT 0,
        last_line_no           INTEGER NOT NULL DEFAULT 0,
        last_distilled_line_no INTEGER NOT NULL DEFAULT 0,
        last_growth_ts         INTEGER
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);";

    /// Lay down a v4-shape DB and stamp `PRAGMA user_version = 4`.
    fn build_v4_db(db_path: &std::path::Path) {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db_path).unwrap();
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(V4_DDL_FROZEN).unwrap();
        conn.execute_batch("PRAGMA user_version = 4;").unwrap();
    }

    /// Serialise an `[f32; 1024]` to the JSON-array form that vec0's
    /// MATCH operator accepts. Test-only.
    fn vec_to_json(v: &[f32]) -> String {
        let mut s = String::with_capacity(v.len() * 6 + 2);
        s.push('[');
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("{x}"));
        }
        s.push(']');
        s
    }

    /// Build a 1024-dim vector that's mostly zeros except for one
    /// component set to 1.0 at `axis` — gives us unambiguous KNN
    /// ordering: querying near `axis=i` returns rowid `i+1` first.
    fn axis_unit_vec(axis: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 1024];
        v[axis] = 1.0;
        v
    }

    // S4s.A.1 — fresh DB lands on v5 with log_vec present, and vec0
    // is actually usable (vec_version() returns something).
    #[test]
    fn s4s_a1_fresh_db_lands_on_v5_with_log_vec_table() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        assert!(schema_object_exists(&conn, "table", "log_vec"));

        // vec_version() proves the extension actually registered.
        let vec_version: String = conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .unwrap();
        assert!(
            !vec_version.is_empty(),
            "vec_version() must return something, got {vec_version:?}"
        );
    }

    // S4s.A.2 — log_vec accepts a 1024-dim insert tied to a parent log row.
    #[test]
    fn s4s_a2_log_vec_accepts_1024_dim_insert() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        // Need a real log row so the rowid is meaningful for join tests.
        conn.execute(
            "INSERT INTO log(ts, summary, detail, origin) VALUES (?1, ?2, ?3, ?4)",
            params![1_i64, "subject", "body", "self"],
        )
        .unwrap();
        let log_id = conn.last_insert_rowid();

        let vec = axis_unit_vec(0);
        let json = vec_to_json(&vec);
        conn.execute(
            "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
            params![log_id, &json],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT count(*) FROM log_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // S4s.B.1 — three axis-unit vectors → KNN against a vector close
    // to one axis returns that rowid first; distances are non-decreasing.
    #[test]
    fn s4s_b1_knn_ranks_nearest_rowid_first() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        for (rowid, axis) in [(1_i64, 0_usize), (2, 1), (3, 2)] {
            let v = axis_unit_vec(axis);
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![rowid, vec_to_json(&v)],
            )
            .unwrap();
        }

        // Query vector very close to axis-0 (rowid 1).
        let mut q = vec![0.0_f32; 1024];
        q[0] = 0.95;
        q[1] = 0.05;
        let q_json = vec_to_json(&q);

        let mut stmt = conn
            .prepare(
                "SELECT rowid, distance FROM log_vec \
                 WHERE embedding MATCH ?1 ORDER BY distance LIMIT 3",
            )
            .unwrap();
        let rows: Vec<(i64, f64)> = stmt
            .query_map(params![&q_json], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(rows.len(), 3, "should get 3 KNN hits");
        assert_eq!(rows[0].0, 1, "rowid 1 (axis-0) ranks first");
        // Non-decreasing distance.
        for w in rows.windows(2) {
            assert!(w[0].1 <= w[1].1, "distances must be sorted: {rows:?}");
        }
    }

    // S4s.B.2 — JOIN log_vec back to log via rowid = log.id, surfacing
    // summary + distance in one query (the shape memory_search will use).
    #[test]
    fn s4s_b2_join_log_with_log_vec_returns_summary_and_distance() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        // Three log rows, three matching vectors.
        let labels = ["first", "second", "third"];
        for (axis, label) in labels.iter().enumerate() {
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![(axis as i64) + 1, *label, "x", "self"],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            let v = axis_unit_vec(axis);
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_to_json(&v)],
            )
            .unwrap();
        }

        // Query near axis-1 ("second") and confirm the JOIN shape.
        let mut q = vec![0.0_f32; 1024];
        q[1] = 1.0;
        let q_json = vec_to_json(&q);

        // Note the `k = ?2` constraint: vec0 needs LIMIT or `k = ?` to
        // appear directly on its scan. With a JOIN, an outer `LIMIT 3`
        // wouldn't reach vec0's xBestIndex, so we pass `k` instead.
        let mut stmt = conn
            .prepare(
                "SELECT log.id, log.summary, log_vec.distance \
                 FROM log_vec JOIN log ON log.id = log_vec.rowid \
                 WHERE log_vec.embedding MATCH ?1 AND log_vec.k = ?2 \
                 ORDER BY log_vec.distance",
            )
            .unwrap();
        let rows: Vec<(i64, String, f64)> = stmt
            .query_map(params![&q_json, 3_i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, "second", "nearest hit's log.summary == 'second'");
    }

    // S4s.C.1 — v4 DB upgrades to v5: existing log rows survive; log_vec
    // appears empty.
    #[test]
    fn s4s_c1_upgrades_v4_db_to_v5_preserving_log_rows() {
        let (_tmp, db) = db_path();
        build_v4_db(&db);
        // Seed a log row pre-v5 (no log_vec yet).
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![42_i64, "pre-v5", "body", "self", None::<&str>],
            )
            .unwrap();
        }

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert!(schema_object_exists(&conn, "table", "log_vec"));

        let (summary, kind): (String, Option<String>) = conn
            .query_row(
                "SELECT summary, kind FROM log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(summary, "pre-v5");
        assert!(kind.is_none());

        let count: i64 = conn
            .query_row("SELECT count(*) FROM log_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "fresh v5 log_vec starts empty");
    }

    // S4s.C.2 — full v0 → v1 → v2 → v3 → v4 → v5 chain in one open();
    // v0 log rows preserved; every per-version product present.
    #[test]
    fn s4s_c2_upgrades_v0_db_through_full_chain_to_v5() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        // v1 product
        assert!(log_columns(&conn).contains("kind"));
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
        // v2 product
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));
        // v3 product
        assert_eq!(
            table_columns(&conn, "raw_thread"),
            expected_raw_thread_columns()
        );
        // v4 product (kind on log)
        assert!(log_columns(&conn).contains("kind"));
        // v5 product
        assert!(schema_object_exists(&conn, "table", "log_vec"));

        let count: i64 = conn
            .query_row("SELECT count(*) FROM log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, seeded.len(), "v0 log rows preserved");
    }

    // S4s.C.3 — open() three times in a row is idempotent on v5;
    // `CREATE VIRTUAL TABLE IF NOT EXISTS` doesn't re-create log_vec.
    #[test]
    fn s4s_c3_open_thrice_is_idempotent_at_v5() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(schema_object_exists(&conn, "table", "log_vec"));
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 4-embed — Embedder + embed_pending_once
    // -----------------------------------------------------------------

    /// In-memory embedder for tests. No network. Records every call's
    /// `texts` slice + supports three behaviours: per-text axis-units
    /// (good for KNN assertions), a fixed constant vector for everyone,
    /// or always-error.
    struct FakeEmbedder {
        dimensions: usize,
        behavior: std::sync::Mutex<FakeEmbedBehavior>,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
    }

    #[derive(Clone)]
    enum FakeEmbedBehavior {
        /// `texts[i]` → unit vector along axis `i`. Distinguishable in KNN.
        AxisUnits,
        /// Every text → the same vector.
        Constant(Vec<f32>),
        /// Always return this error.
        Err(EmbedError),
    }

    impl FakeEmbedder {
        fn axis_units(dimensions: usize) -> Self {
            Self {
                dimensions,
                behavior: std::sync::Mutex::new(FakeEmbedBehavior::AxisUnits),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn constant(dimensions: usize, v: Vec<f32>) -> Self {
            assert_eq!(v.len(), dimensions);
            Self {
                dimensions,
                behavior: std::sync::Mutex::new(FakeEmbedBehavior::Constant(v)),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn errors_with(dimensions: usize, err: EmbedError) -> Self {
            Self {
                dimensions,
                behavior: std::sync::Mutex::new(FakeEmbedBehavior::Err(err)),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn batches(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Embedder for FakeEmbedder {
        fn dimensions(&self) -> usize {
            self.dimensions
        }

        async fn embed(
            &self,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.calls.lock().unwrap().push(texts.to_vec());
            let beh = self.behavior.lock().unwrap().clone();
            match beh {
                FakeEmbedBehavior::AxisUnits => {
                    let mut out = Vec::with_capacity(texts.len());
                    for i in 0..texts.len() {
                        let mut v = vec![0.0_f32; self.dimensions];
                        if i < self.dimensions {
                            v[i] = 1.0;
                        }
                        out.push(v);
                    }
                    Ok(out)
                }
                FakeEmbedBehavior::Constant(v) => {
                    Ok(texts.iter().map(|_| v.clone()).collect())
                }
                FakeEmbedBehavior::Err(e) => Err(e),
            }
        }
    }

    fn test_http_embedder(
        base_url: &str,
        api_key: &str,
        model: &str,
        dimensions: usize,
    ) -> HttpEmbedder {
        HttpEmbedder {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            dimensions,
        }
    }

    fn seed_log_row(conn: &Connection, summary: &str, detail: Option<&str>) -> i64 {
        conn.execute(
            "INSERT INTO log(ts, summary, detail, origin) VALUES (?1, ?2, ?3, ?4)",
            params![current_time_ms(), summary, detail, "self"],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn log_vec_rowids(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn.prepare("SELECT rowid FROM log_vec ORDER BY rowid").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    // ==== A — request build + response parse + load() ====

    // S4e.A.1 — request body has model + input array + dimensions.
    #[test]
    fn s4e_a1_build_request_body_shape() {
        let e = test_http_embedder("https://api.example/v1", "sk-x", "model-y", 1024);
        let texts = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let body = e.build_request_body(&texts);
        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("model-y"));
        assert_eq!(
            body.get("dimensions").and_then(|v| v.as_u64()),
            Some(1024)
        );
        let input = body.get("input").and_then(|v| v.as_array()).unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0].as_str(), Some("one"));
        assert_eq!(input[1].as_str(), Some("two"));
        assert_eq!(input[2].as_str(), Some("three"));

        // URL: never re-inject `/v1`.
        assert_eq!(
            e.embeddings_url(),
            "https://api.example/v1/embeddings",
            "embeddings_url must append /embeddings, not /v1/embeddings"
        );
    }

    // S4e.A.2 — out-of-order data[] is re-sorted by `index`.
    #[test]
    fn s4e_a2_parse_response_reorders_by_index() {
        let body = json!({
            "object": "list",
            "model": "m",
            "data": [
                {"index": 2, "object": "embedding", "embedding": [0.0, 0.0, 1.0]},
                {"index": 0, "object": "embedding", "embedding": [1.0, 0.0, 0.0]},
                {"index": 1, "object": "embedding", "embedding": [0.0, 1.0, 0.0]},
            ],
            "usage": {"prompt_tokens": 9, "total_tokens": 9}
        });
        let vectors = parse_embeddings_response(&body, 3, 3).unwrap();
        assert_eq!(vectors.len(), 3);
        // index=0 must be first regardless of where it appeared in `data`.
        assert_eq!(vectors[0], vec![1.0, 0.0, 0.0]);
        assert_eq!(vectors[1], vec![0.0, 1.0, 0.0]);
        assert_eq!(vectors[2], vec![0.0, 0.0, 1.0]);
    }

    // S4e.A.3 — wrong-dimension vectors → Parse error (we don't put bad
    // vectors into log_vec).
    #[test]
    fn s4e_a3_wrong_dimensionality_returns_parse_error() {
        let body = json!({
            "data": [
                {"index": 0, "embedding": [1.0, 2.0]},   // dim=2
                {"index": 1, "embedding": [3.0, 4.0]},
            ]
        });
        let err = parse_embeddings_response(&body, 2, 4).unwrap_err();
        match err {
            EmbedError::Parse(msg) => assert!(
                msg.contains("vector length 2") && msg.contains("expected_dim 4"),
                "expected dim mismatch message, got: {msg}"
            ),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    // S4e.A.4 — load(): env beats file; file fallback; missing
    // embed_model → None.
    #[test]
    fn s4e_a4_load_env_first_then_file_else_none() {
        let tmp = tempfile::tempdir().unwrap();
        let opencrab_dir = tmp.path().join(".opencrab");
        std::fs::create_dir_all(&opencrab_dir).unwrap();
        std::fs::write(
            opencrab_dir.join("distiller.json"),
            json!({
                "base_url": "https://file.example/v1",
                "api_key": "sk-file",
                "model": "chat-file",
                "embed_model": "embed-file",
                "embed_dimensions": 1024
            })
            .to_string(),
        )
        .unwrap();
        let home_str = tmp.path().to_str().unwrap();

        // (a) env wins
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", Some("https://env.example/v1")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("sk-env")),
                ("OPENCRAB_EMBED_MODEL", Some("env-embed-model")),
                ("OPENCRAB_EMBED_DIMENSIONS", None),
            ],
            || {
                let e = HttpEmbedder::load().expect("env wins → Some");
                assert_eq!(e.base_url, "https://env.example/v1");
                assert_eq!(e.api_key, "sk-env");
                assert_eq!(e.model, "env-embed-model");
                assert_eq!(e.dimensions, 1024, "default when env dim absent");
            },
        );

        // (b) env unset → file fallback
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", None),
                ("OPENCRAB_DISTILLER_API_KEY", None),
                ("OPENCRAB_EMBED_MODEL", None),
                ("OPENCRAB_EMBED_DIMENSIONS", None),
            ],
            || {
                let e = HttpEmbedder::load().expect("file fallback → Some");
                assert_eq!(e.base_url, "https://file.example/v1");
                assert_eq!(e.api_key, "sk-file");
                assert_eq!(e.model, "embed-file");
                assert_eq!(e.dimensions, 1024);
            },
        );

        // (c) file lacks embed_model → None even though base_url/api_key are set
        std::fs::write(
            opencrab_dir.join("distiller.json"),
            json!({
                "base_url": "https://file.example/v1",
                "api_key": "sk-file",
                "model": "chat-only"
            })
            .to_string(),
        )
        .unwrap();
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", None),
                ("OPENCRAB_DISTILLER_API_KEY", None),
                ("OPENCRAB_EMBED_MODEL", None),
                ("OPENCRAB_EMBED_DIMENSIONS", None),
            ],
            || {
                assert!(
                    HttpEmbedder::load().is_none(),
                    "missing embed_model → None"
                );
            },
        );
    }

    // ==== B — embed_pending_once orchestration ====

    // S4e.B.1 — greenfield (0 log rows) is a complete no-op.
    #[test]
    fn s4e_b1_greenfield_is_noop() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let fake = FakeEmbedder::axis_units(1024);
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.rows_pending_seen, 0);
        assert_eq!(stats.rows_embedded, 0);
        assert_eq!(stats.batches, 0);
        assert_eq!(fake.call_count(), 0);
        assert!(log_vec_rowids(&conn).is_empty());
    }

    // S4e.B.2 — 3 pending rows → fake returns 3 vectors → 3 log_vec
    // rows (rowid == log.id) + KNN over the result orders correctly.
    #[test]
    fn s4e_b2_embeds_pending_rows_and_writes_to_log_vec() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id1 = seed_log_row(&conn, "alpha topic", Some("alpha body"));
        let id2 = seed_log_row(&conn, "beta topic", Some("beta body"));
        let id3 = seed_log_row(&conn, "gamma topic", Some("gamma body"));

        let fake = FakeEmbedder::axis_units(1024);
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.rows_pending_seen, 3);
        assert_eq!(stats.rows_embedded, 3);
        assert_eq!(stats.batches, 1);
        assert_eq!(fake.call_count(), 1);

        assert_eq!(log_vec_rowids(&conn), vec![id1, id2, id3]);

        // KNN: pick the axis-1 unit (== second row's text) → row 2 first.
        let mut q = vec![0.0_f32; 1024];
        q[1] = 1.0;
        let q_json = vec_to_match_json(&q);
        let mut stmt = conn
            .prepare(
                "SELECT log.id FROM log_vec JOIN log ON log.id = log_vec.rowid \
                 WHERE log_vec.embedding MATCH ?1 AND log_vec.k = ?2 \
                 ORDER BY log_vec.distance",
            )
            .unwrap();
        let ids: Vec<i64> = stmt
            .query_map(params![&q_json, 3_i64], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ids.first().copied(), Some(id2));
    }

    // S4e.B.3 — idempotent: a second pass with no new rows is a no-op.
    #[test]
    fn s4e_b3_second_pass_with_no_new_rows_is_noop() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        seed_log_row(&conn, "a", Some("b"));
        let fake = FakeEmbedder::axis_units(1024);
        let _ = block_on(embed_pending_once(&conn, &fake)).unwrap();

        let stats2 = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats2.rows_pending_seen, 0);
        assert_eq!(stats2.rows_embedded, 0);
        assert_eq!(stats2.batches, 0);
        // call_count = 1 from the first pass, not bumped by the second.
        assert_eq!(fake.call_count(), 1);
    }

    // S4e.B.4 — partial backfill: rows that already have a log_vec
    // entry are skipped; only the missing one is sent to the embedder.
    #[test]
    fn s4e_b4_skips_rows_that_already_have_an_embedding() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id1 = seed_log_row(&conn, "already embedded 1", Some("b1"));
        let id2 = seed_log_row(&conn, "already embedded 2", Some("b2"));
        let id3 = seed_log_row(&conn, "still pending", Some("b3"));

        // Pre-seed log_vec for ids 1 and 2.
        for id in [id1, id2] {
            let mut v = vec![0.0_f32; 1024];
            v[0] = 1.0;
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_to_match_json(&v)],
            )
            .unwrap();
        }

        let fake = FakeEmbedder::axis_units(1024);
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.rows_pending_seen, 1, "only id3 is pending");
        assert_eq!(stats.rows_embedded, 1);

        let calls = fake.batches();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
        // The only text sent should be id3's text.
        assert!(
            calls[0][0].starts_with("still pending"),
            "embed sent the pending row's text, got: {:?}",
            calls[0][0]
        );

        let rowids = log_vec_rowids(&conn);
        assert!(rowids.contains(&id3));
        assert_eq!(rowids.len(), 3);
    }

    // S4e.B.5 — text shape: `summary + "\n\n" + detail`, with NULL detail
    // producing `summary + "\n\n" + ""`. Empty-content rows skipped.
    #[test]
    fn s4e_b5_embed_text_shape_matches_spec() {
        // Direct build_embed_text check.
        assert_eq!(build_embed_text("hi", Some("body")), "hi\n\nbody");
        assert_eq!(build_embed_text("hi", None), "hi\n\n");
        assert_eq!(build_embed_text("", None), "\n\n");

        // Via embed_pending_once: detail = NULL row produces "summary\n\n".
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        seed_log_row(&conn, "headline", None);
        let fake = FakeEmbedder::axis_units(1024);
        let _ = block_on(embed_pending_once(&conn, &fake)).unwrap();
        let calls = fake.batches();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
        assert_eq!(calls[0][0], "headline\n\n");
    }

    // S4e.B.6 — more than EMBED_BATCH_SIZE pending rows: multiple
    // batches, each ≤ batch size, all rows embedded.
    #[test]
    fn s4e_b6_chunks_more_than_batch_size_rows_into_multiple_batches() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let total = (EMBED_BATCH_SIZE * 2 + 5) as i64;
        for i in 1..=total {
            seed_log_row(&conn, &format!("row {i}"), Some(&format!("body {i}")));
        }

        let fake = FakeEmbedder::constant(1024, vec![0.0_f32; 1024]);
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.rows_embedded, total as u64);
        let batches = fake.batches();
        assert_eq!(batches.len(), 3, "ceil({} / {})", total, EMBED_BATCH_SIZE);
        for b in &batches {
            assert!(
                b.len() <= EMBED_BATCH_SIZE,
                "batch size {} exceeds cap {}",
                b.len(),
                EMBED_BATCH_SIZE
            );
        }
        assert_eq!(log_vec_rowids(&conn).len(), total as usize);
    }

    // S4e.B.7 — embedder error → no log_vec inserts, all rows stay pending.
    #[test]
    fn s4e_b7_embedder_error_leaves_rows_pending() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        for i in 1..=5 {
            seed_log_row(&conn, &format!("row {i}"), Some("body"));
        }

        let fake =
            FakeEmbedder::errors_with(1024, EmbedError::HttpTransient("503".into()));
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.rows_embedded, 0);
        assert!(log_vec_rowids(&conn).is_empty(), "no half-written batch");

        // Pending count via SELECT: still 5.
        let pending: i64 = conn
            .query_row(
                "SELECT count(*) FROM log WHERE id NOT IN (SELECT rowid FROM log_vec)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 5);
    }

    // ==== D — #[ignore] live: real SiliconFlow /v1/embeddings ====

    /// Manual: `cargo test --bin opencrab-memory-mcp -- --ignored \
    ///   s4e_d1_live_embed_against_siliconflow --nocapture`
    #[test]
    #[ignore]
    fn s4e_d1_live_embed_against_siliconflow() {
        let embedder = HttpEmbedder::load()
            .expect("OPENCRAB_EMBED_MODEL env OR distiller.json embed_model required");

        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        // Two log rows with semantically different content + a third
        // closer to row A. KNN-against-A's embedding should rank
        // (A, A-related) ahead of (B).
        let id_migration = seed_log_row(
            &conn,
            "cargo test failed: duplicate column name origin",
            Some("ALTER TABLE ADD COLUMN ran twice; PRAGMA user_version gating fixes it"),
        );
        let id_lunch = seed_log_row(
            &conn,
            "lunch menu discussion",
            Some("we agreed on dumplings for the team lunch"),
        );
        let id_other_migration = seed_log_row(
            &conn,
            "SQLite ALTER TABLE migration",
            Some("idempotent schema upgrade via user_version PRAGMA"),
        );

        let stats = block_on(embed_pending_once(&conn, &embedder)).unwrap();
        eprintln!("[live-embed] stats: {stats:?}");
        eprintln!("[live-embed] dim: {}", embedder.dimensions());
        assert_eq!(stats.rows_embedded, 3);

        // Sanity: each log_vec row must hold a 1024-d vector.
        // sqlite-vec stores vectors as opaque; we infer from the
        // distance column behaving sensibly.
        let q_text = "ALTER TABLE migration not idempotent";
        let q_vec = block_on(embedder.embed(&[q_text.to_string()]))
            .expect("live embed of query");
        eprintln!("[live-embed] query vector length: {}", q_vec[0].len());
        assert_eq!(
            q_vec[0].len(),
            1024,
            "embedding dimension must match log_vec's float[1024]"
        );

        let q_json = vec_to_match_json(&q_vec[0]);
        let mut stmt = conn
            .prepare(
                "SELECT log.id, log.summary, log_vec.distance \
                 FROM log_vec JOIN log ON log.id = log_vec.rowid \
                 WHERE log_vec.embedding MATCH ?1 AND log_vec.k = ?2 \
                 ORDER BY log_vec.distance",
            )
            .unwrap();
        let rows: Vec<(i64, String, f64)> = stmt
            .query_map(params![&q_json, 3_i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        eprintln!("[live-embed] KNN rows (id, summary, distance):");
        for (id, summary, dist) in &rows {
            eprintln!("  id={id} dist={dist:.4} summary={summary}");
        }
        assert_eq!(rows.len(), 3);

        // The two migration-related rows should outrank the lunch row.
        let lunch_pos = rows
            .iter()
            .position(|(id, _, _)| *id == id_lunch)
            .expect("lunch row in results");
        let migration_positions: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, (id, _, _))| *id == id_migration || *id == id_other_migration)
            .map(|(i, _)| i)
            .collect();
        assert!(
            migration_positions.iter().all(|&pos| pos < lunch_pos),
            "both migration rows must rank above the lunch row; got order: {:?}",
            rows.iter().map(|r| r.0).collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 4-search — hybrid (FTS bm25 ⊕ vector KNN, RRF-merged)
    // -----------------------------------------------------------------

    /// FakeEmbedder variant with a configurable artificial delay before
    /// returning the result. Used to exercise the search-time embed timeout.
    struct SlowFakeEmbedder {
        dimensions: usize,
        delay: std::time::Duration,
        inner: FakeEmbedBehavior,
    }

    impl SlowFakeEmbedder {
        fn new(dimensions: usize, delay: std::time::Duration, inner: FakeEmbedBehavior) -> Self {
            Self { dimensions, delay, inner }
        }
    }

    #[async_trait::async_trait]
    impl Embedder for SlowFakeEmbedder {
        fn dimensions(&self) -> usize {
            self.dimensions
        }

        async fn embed(
            &self,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            tokio::time::sleep(self.delay).await;
            let beh = self.inner.clone();
            match beh {
                FakeEmbedBehavior::AxisUnits => {
                    let mut out = Vec::with_capacity(texts.len());
                    for i in 0..texts.len() {
                        let mut v = vec![0.0_f32; self.dimensions];
                        if i < self.dimensions {
                            v[i] = 1.0;
                        }
                        out.push(v);
                    }
                    Ok(out)
                }
                FakeEmbedBehavior::Constant(v) => {
                    Ok(texts.iter().map(|_| v.clone()).collect())
                }
                FakeEmbedBehavior::Err(e) => Err(e),
            }
        }
    }

    /// Build a 1024-d vector with values set at specific axes — used to
    /// craft scenarios where FTS and vector orderings differ.
    fn vec_with_axes(values: &[(usize, f32)]) -> Vec<f32> {
        let mut v = vec![0.0_f32; 1024];
        for &(i, x) in values {
            v[i] = x;
        }
        v
    }

    fn insert_log_vec(conn: &Connection, rowid: i64, v: &[f32]) {
        conn.execute(
            "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
            params![rowid, vec_to_match_json(v)],
        )
        .unwrap();
    }

    // ==== A — hybrid sort + RRF math ====

    // S4w.A.1 — FTS and vector lists disagree; RRF merges them into the
    // expected order (an entry appearing in BOTH lists outranks an
    // entry that's strong in only one).
    //
    // Construction:
    //   - "keyword" appears twice in A's detail → FTS rank 1 (best)
    //   - "keyword" appears once in C's detail → FTS rank 2
    //   - B has no "keyword" → not in FTS
    //   Vectors:
    //   - A: (0.5, 0.5, …) — partly aligned with query axis-1
    //   - B: (0, 1.0, …) — exactly query axis-1, KNN rank 1
    //   - C: (1.0, 0, …) — far from query, KNN rank 3
    //   Embedder returns axis-1 unit for the query.
    //
    // RRF (K=60, rank starts at 1):
    //   A: 1/(60+1) + 1/(60+2) ≈ 0.03253
    //   C: 1/(60+2) + 1/(60+3) ≈ 0.03200
    //   B: 0          + 1/(60+1) ≈ 0.01639
    // Expected: [A, C, B].
    #[test]
    fn s4w_a1_rrf_merges_fts_and_vector_orderings_correctly() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        let id_a = seed_log_row(&conn, "alpha", Some("alpha keyword keyword body"));
        let id_b = seed_log_row(&conn, "beta", Some("nothing matching here"));
        let id_c = seed_log_row(&conn, "gamma", Some("keyword tail"));

        insert_log_vec(&conn, id_a, &vec_with_axes(&[(0, 0.5), (1, 0.5)]));
        insert_log_vec(&conn, id_b, &vec_with_axes(&[(1, 1.0)]));
        insert_log_vec(&conn, id_c, &vec_with_axes(&[(0, 1.0)]));

        // Embed-the-query returns axis-1 unit (close to id_b).
        let fake = FakeEmbedder::constant(1024, vec_with_axes(&[(1, 1.0)]));
        drop(conn);

        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(
            ids,
            vec![id_a, id_c, id_b],
            "RRF must rank id_a (top of both lists indirectly) > id_c (both lists) > id_b (vector only)"
        );
    }

    // S4w.A.2 — RRF math on hand-picked lists: verify scores by hand.
    #[test]
    fn s4w_a2_rrf_merge_math_is_correct_on_known_inputs() {
        // Two lists, three docs, disagreement.
        let fts = vec![1_i64, 2, 3];
        let vec_list = vec![3_i64, 1, 2];
        // Hand-computed RRF (K=60, rank 1-based):
        //   id 1: 1/61 + 1/62 = 0.032526
        //   id 2: 1/62 + 1/63 = 0.031883
        //   id 3: 1/63 + 1/61 = 0.032265
        // Order (desc): 1, 3, 2.
        let merged = rrf_merge(&[&fts, &vec_list], 10);
        assert_eq!(merged, vec![1_i64, 3, 2]);

        // Ties on score → broken by id ASC.
        let merged2 = rrf_merge(&[&vec![1_i64, 2], &vec![2_i64, 1]], 10);
        // id 1: 1/61 + 1/62 = 0.032526
        // id 2: 1/62 + 1/61 = 0.032526 (same)
        // tie → smaller id wins
        assert_eq!(merged2, vec![1_i64, 2]);
    }

    // ==== B — graceful degradation paths ====

    // S4w.B.1 — embedder=None → pure FTS path; behavior equals the
    // pre-S4 FTS-only search.
    #[test]
    fn s4w_b1_no_embedder_returns_pure_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("alpha beta keyword"));
        let _id_b = seed_log_row(&conn, "b", Some("nothing relevant"));
        drop(conn);

        let hits = block_on(search(&db, None, "keyword", 10)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id_a);
    }

    // S4w.B.2 — embedder returns Err → vector path dropped, pure-FTS
    // result returned without error.
    #[test]
    fn s4w_b2_embedder_error_falls_back_to_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("keyword in detail"));
        let id_b = seed_log_row(&conn, "b", Some("not relevant"));
        // Both have log_vec entries so a working vector path would
        // re-shuffle the ranking — but the embedder errors, so it shouldn't.
        insert_log_vec(&conn, id_a, &vec_with_axes(&[(0, 1.0)]));
        insert_log_vec(&conn, id_b, &vec_with_axes(&[(1, 1.0)]));
        drop(conn);

        let fake = FakeEmbedder::errors_with(
            1024,
            EmbedError::HttpTransient("503 backend".into()),
        );
        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        assert_eq!(hits.len(), 1, "FTS hits only — id_b has no 'keyword'");
        assert_eq!(hits[0].id, id_a);
    }

    // S4w.B.3 — `log` rows exist but `log_vec` is empty (greenfield):
    // vector path returns 0 ids → result equals pure FTS.
    #[test]
    fn s4w_b3_empty_log_vec_returns_pure_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("keyword present"));
        let _id_b = seed_log_row(&conn, "b", Some("other content"));
        // No log_vec inserts → greenfield.
        drop(conn);

        let fake = FakeEmbedder::constant(1024, vec_with_axes(&[(1, 1.0)]));
        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id_a);
    }

    // S4w.B.4 — embedder claims dim=1024 but returns a 100-length vector:
    // search drops the vector path and falls back to FTS.
    #[test]
    fn s4w_b4_query_embedding_dim_mismatch_falls_back_to_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("keyword body"));
        let _id_b = seed_log_row(&conn, "b", Some("unrelated"));
        insert_log_vec(&conn, id_a, &vec_with_axes(&[(0, 1.0)]));
        drop(conn);

        // Direct construction lets us bypass FakeEmbedder::constant's
        // dim-assertion and emit a vector that lies about its size.
        let fake = FakeEmbedder {
            dimensions: 1024,
            behavior: std::sync::Mutex::new(FakeEmbedBehavior::Constant(vec![0.0_f32; 100])),
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        assert_eq!(hits.len(), 1, "vector path dropped, FTS only");
        assert_eq!(hits[0].id, id_a);
    }

    // S4w.B.5 — embed takes longer than QUERY_EMBED_TIMEOUT: the
    // timeout fires, vector path drops, FTS-only result returned.
    // Uses an injected (tiny) timeout via the test-only
    // `search_with_timeout` so the test stays sub-second.
    #[test]
    fn s4w_b5_embed_timeout_falls_back_to_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("keyword here"));
        drop(conn);

        let slow = SlowFakeEmbedder::new(
            1024,
            std::time::Duration::from_millis(500),
            FakeEmbedBehavior::Constant(vec![0.0_f32; 1024]),
        );
        let hits = block_on(search_with_timeout(
            &db,
            Some(&slow),
            "keyword",
            10,
            std::time::Duration::from_millis(50), // budget shorter than the embedder's 500ms delay
        ))
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id_a);
    }

    // ==== C — MemoryHit shape contract ====

    // S4w.C.1 — MemoryHit fields are unchanged (id, ts, summary, snippet);
    // snippet is the truncated `detail` body — proving the MCP-facing
    // shape of memory_search hasn't drifted.
    #[test]
    fn s4w_c1_memory_hit_shape_unchanged() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id = seed_log_row(
            &conn,
            "headline",
            Some("body containing the keyword sentinel for FTS"),
        );
        drop(conn);

        let hits = block_on(search(&db, None, "sentinel", 10)).unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.id, id);
        assert!(h.ts > 0);
        assert_eq!(h.summary, "headline");
        assert_eq!(h.snippet, "body containing the keyword sentinel for FTS");

        // memory_get is unchanged; spot-check it still returns the
        // full row by id.
        let entry = get(&db, id).unwrap();
        assert!(entry.found);
        assert_eq!(entry.summary, "headline");
        assert_eq!(
            entry.detail.as_deref(),
            Some("body containing the keyword sentinel for FTS")
        );
    }
}

