// Phase 6 Step 1 — `opencrab-memory-mcp`: a per-agent stdio MCP server over
// one agent's SQLite log store.
//
// One instance is spawned per agent by the OpenCrab Tauri host (see
// `codex_spawn::build_memory_mcp_server_entry`). The agent id is the only
// thing the caller hands us — the memory.db path is derived here from
// `~/.opencrab/agents/<agent_id>/memory.db` so this process is the file's
// sole owner by construction.

// `paths.rs` is the SSOT for every `.opencrab` path; `#[path]`-include it
// (same pattern as `src/bin/codex_monitor_daemon.rs`) instead of routing
// through the lib so the bin keeps its existing `pub(crate)` boundary.
// `dead_code` is allowed because we only call a small subset of the
// resolvers — the rest are dead in this compilation unit by design.
#[allow(dead_code)]
#[path = "../../paths.rs"]
mod paths;

mod backend;
mod server;

use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Register vec0 BEFORE any sqlite3_open in this process. Idempotent
    // via OnceLock so the bin can be re-entered safely; harmless for
    // any non-vec connections that follow.
    backend::ensure_vec_extension();
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("[opencrab-memory-mcp] {err}");
            eprintln!("usage: opencrab-memory-mcp --agent-id <id>");
            return ExitCode::from(2);
        }
    };
    let memory_db = match resolve_memory_db(&args.agent_id) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("[opencrab-memory-mcp] {err}");
            return ExitCode::FAILURE;
        }
    };
    // Phase 6 Step 2-ingest — spawn the rollout → raw_thread/raw_event
    // ingest loop. It polls `<user_root>/agents/<id>/team_sessions/` every
    // 15 s, runs one `backend::ingest_once` per tick on a blocking thread,
    // and never panics out of the loop — see [`backend::run_ingest_loop`].
    let scan_root = match resolve_scan_root(&args.agent_id) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("[opencrab-memory-mcp] scan_root unresolved: {err}");
            return ExitCode::FAILURE;
        }
    };
    let ingest_handle = tokio::spawn(backend::run_ingest_loop(
        memory_db.clone(),
        scan_root,
        args.agent_id.clone(),
    ));

    // Phase 6 Step 3-wire — spawn the distillation loop iff the operator
    // configured a provider via `OPENCRAB_DISTILLER_{BASE_URL,MODEL,API_KEY}`.
    // Lives on a dedicated OS thread with its own current_thread runtime:
    //   * decouples the LLM POST's async work from the MCP server's
    //     runtime so they don't compete on the same executor;
    //   * keeps the SQLite paths inside `distill_once` off the server's
    //     hot loop, regardless of how long an extract round-trip takes.
    // The thread is detached — when `main` returns the process exits and
    // tears the thread down. Any in-flight IMMEDIATE transaction rolls
    // back via WAL recovery on the next open; no graceful-shutdown
    // signal is required.
    match backend::HttpExtractor::load() {
        Some(extractor) => {
            let embedder = backend::HttpEmbedder::load();
            match &embedder {
                Some(_) => eprintln!("[embed] enabled"),
                None => eprintln!(
                    "[embed] disabled — set OPENCRAB_EMBED_MODEL (env) or \
                     add `embed_model` (+ optional `embed_dimensions`) to \
                     `<user_root>/distiller.json` to enable"
                ),
            }
            let db = memory_db.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build distill runtime");
                rt.block_on(backend::run_distill_loop(db, extractor, embedder));
            });
            eprintln!("[distill] enabled");
        }
        None => eprintln!(
            "[distill] disabled — set OPENCRAB_DISTILLER_BASE_URL/MODEL/API_KEY \
             or write `<user_root>/distiller.json` with {{base_url, model, api_key}}"
        ),
    }

    // A separate embedder instance for the server's `memory_search`
    // hybrid path. Each is independent — loading from env/file is
    // cheap, and decoupling means the server's embedder can be live
    // even if the distill loop's was disabled (or vice versa). When
    // either is `None`, that side degrades silently.
    let server_embedder = backend::HttpEmbedder::load();
    let result = server::run_stdio(args.agent_id, memory_db, server_embedder).await;
    // Server exited (stdin closed) — stop the ingester. `abort` is enough
    // because the loop holds no buffered writes between passes: any
    // mid-pass IMMEDIATE transaction is owned inside the spawn_blocking
    // call, which finishes (commit or rollback) before we'd ever see a
    // chance to cancel.
    ingest_handle.abort();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[opencrab-memory-mcp] server error: {err}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    agent_id: String,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut agent_id: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--agent-id" => {
                agent_id = Some(args.next().ok_or("--agent-id needs a value")?);
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    Ok(Args {
        agent_id: agent_id.ok_or("missing required --agent-id")?,
    })
}

fn resolve_memory_db(agent_id: &str) -> Result<PathBuf, String> {
    let root = paths::user_root().ok_or_else(|| {
        "cannot resolve user-layer root (no $HOME / $USERPROFILE / passwd entry)".to_string()
    })?;
    Ok(paths::user_agent_memory_db(&root, agent_id))
}

/// `<user_root>/agents/<agent_id>/team_sessions/` — the per-agent rollout
/// directory the S2 ingester scans. See S2-R recon (the codex-cli fork
/// writes team rollouts here via `ThreadStartParams.session_dir`).
fn resolve_scan_root(agent_id: &str) -> Result<PathBuf, String> {
    let root = paths::user_root().ok_or_else(|| {
        "cannot resolve user-layer root (no $HOME / $USERPROFILE / passwd entry)".to_string()
    })?;
    Ok(paths::user_agent_dir(&root, agent_id).join("team_sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_agent_id_only() {
        let parsed = parse(&["--agent-id", "alice"]).unwrap();
        assert_eq!(parsed.agent_id, "alice");
    }

    #[test]
    fn rejects_missing_or_unknown_args() {
        // `--memory-dir` was retired in P6 Step 3 — it is now an unknown arg.
        assert!(parse(&["--memory-dir", "/m/dir"]).is_err());
        assert!(parse(&["--bogus", "x"]).is_err());
        assert!(parse(&["--agent-id"]).is_err());
    }
}
