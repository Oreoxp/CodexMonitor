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

// `Path` is used by this module (`open`); `PathBuf` is consumed only by the test
// submodules through their `use super::*`. Keep both, silence the non-test lint.
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

/// Default number of search hits when the caller does not pass `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 6;
/// Hard cap on `limit` so one call cannot pull the whole archive.
const MAX_SEARCH_LIMIT: usize = 25;

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

mod search;
#[allow(unused_imports)]
pub(crate) use search::*;

mod ingest;
#[allow(unused_imports)]
pub(crate) use ingest::*;
mod render;
#[allow(unused_imports)]
pub(crate) use render::*;

mod distiller;
#[allow(unused_imports)]
pub(crate) use distiller::*;

// ===========================================================================
// Consolidation (v8 / S5) — the LLM judge + candidate finding + dry-run.
//
// S5's thesis: the RELATION KIND is decided by the judge, NOT the distance.
// Vector distance only GATES candidate eligibility (KNN < T); the five-way
// classification is the judge's job because the bge-m3 distance bands overlap
// (a false-neighbour pair measured 0.79 — closer than a true contradiction at
// 0.80), so distance can't separate the relations.
//
// This module judges candidate pairs, appends an audit trail, and — in APPLY
// mode (`dry_run=false`) — mutates live state: `superseded_by` for
// dedup/supersede, a `log_contradiction` edge for contradictions. Search-side
// filtering of superseded rows lives in `search` (the FTS + KNN halves), not
// here. `dry_run=true` preserves the original preview behaviour (audit only,
// zero mutation).
//
// The background loop now drives this in production (`run_consolidation_step` ←
// `run_distill_loop`, kill-switched by `OPENCRAB_CONSOLIDATION_ENABLED`, fired
// once `count_new_live >= consolidation_min_new_kps`). The module keeps
// `#[allow(dead_code)]` only for the items still reached solely from tests —
// the dry-run inspection surface (`dump_audit_tsv`) and the marker reader —
// which are exercised by Layer D (`corpus_layer_d_consolidate` /
// `corpus_layer_d_apply`) + the apply / judge / trigger unit tests. Reuses the
// distiller's wire layer (`HttpExtractor::post_chat`) — same model + creds, new
// prompt, no new HTTP code.
// ===========================================================================
#[allow(dead_code)]
mod consolidate;
#[allow(unused_imports)]
pub(crate) use consolidate::*;

mod embedder;
#[allow(unused_imports)]
pub(crate) use embedder::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

mod schema;
#[allow(unused_imports)]
pub(crate) use schema::*;

fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// V2-corpus end-to-end runner harness (Layer A mechanical / B distill / C
// search). Kept in its own file; declared here as a child of `backend` so it
// can reach crate-private items (`search_with_timeout`, `current_time_ms`,
// `parse_line`, …) via `use super::*`.
#[cfg(test)]
#[path = "corpus_tests.rs"]
mod corpus_tests;

#[cfg(test)]
#[path = "backend/tests.rs"]
mod tests;
