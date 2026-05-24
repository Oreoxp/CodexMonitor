// Phase 5 Step 2 — `opencrab-memory-mcp`: a per-agent, read-only stdio MCP
// server over one agent's daily-memory archive.
//
// One instance is spawned per agent by the OpenCrab Tauri host, registered
// in that agent's `thread/start` `config` (see
// `codex_spawn::build_memory_mcp_server_entry`). The agent id and the
// agent's `project-memory/` directory arrive as CLI args, so the process is
// scoped to exactly one agent's memory — per-agent isolation by construction.

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
            eprintln!("usage: opencrab-memory-mcp --agent-id <id> --memory-dir <path>");
            return ExitCode::from(2);
        }
    };
    match server::run_stdio(args.agent_id, args.memory_dir).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[opencrab-memory-mcp] server error: {err}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    agent_id: String,
    memory_dir: PathBuf,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut agent_id: Option<String> = None;
    let mut memory_dir: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--agent-id" => {
                agent_id = Some(args.next().ok_or("--agent-id needs a value")?);
            }
            "--memory-dir" => {
                memory_dir =
                    Some(PathBuf::from(args.next().ok_or("--memory-dir needs a value")?));
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    Ok(Args {
        agent_id: agent_id.ok_or("missing required --agent-id")?,
        memory_dir: memory_dir.ok_or("missing required --memory-dir")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_both_required_args() {
        let parsed = parse(&["--agent-id", "alice", "--memory-dir", "/m/dir"]).unwrap();
        assert_eq!(parsed.agent_id, "alice");
        assert_eq!(parsed.memory_dir, PathBuf::from("/m/dir"));
    }

    #[test]
    fn rejects_missing_or_unknown_args() {
        assert!(parse(&["--agent-id", "alice"]).is_err());
        assert!(parse(&["--memory-dir", "/m/dir"]).is_err());
        assert!(parse(&["--bogus", "x"]).is_err());
    }
}
