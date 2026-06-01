use super::{open, current_time_ms, BackendError};
use std::path::PathBuf;
use rusqlite::{params, Connection};

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
pub(crate) struct FileIngestResult {
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

pub(crate) fn ingest_one_file_in_tx(
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

