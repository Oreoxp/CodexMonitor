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
    match server::run_stdio(args.agent_id, memory_db).await {
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
