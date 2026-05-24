// P6 Step 18 — opencrab-team-mcp: stub MCP server that hosts the
// `send_message` and `propose_plan` tool schemas.
//
// **Stub on purpose.** The tools' bodies are no-op acks; the real side
// effects (route the message via team_router / persist a plan row + open
// approval modal) are owned by the Tauri host, which observes the agent's
// tool calls via codex's `item/completed` notifications carrying the full
// `arguments` JSON.
//
// This is the minimal viable spike target for P6's "structured-tool
// protocol" evaluation. If the spike validates the model-side behavior,
// this binary graduates to production unchanged on the schema side; the
// host-side observer that wires call args to team_router replaces the
// text-tag parsers (`plan_parser.rs` + `parse_send_message_tags`).
//
// CLI: takes no arguments. The server is workspace-agnostic — agents
// identify themselves via the per-thread Codex context, not via args. If
// future iterations need agent scoping (e.g., to gate which `to` values
// are allowed by ACL at the MCP layer), `--agent-id` follows the
// opencrab-memory-mcp pattern.

mod server;

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Step 18 debug — startup sentinel. The eval harness checks for this
    // file to confirm codex actually spawned us (codex captures MCP
    // stderr to its own logs, so plain eprintln isn't always visible from
    // the eval bin's perspective). Best-effort write; failure to write
    // does not block startup.
    let sentinel = std::env::temp_dir().join(format!(
        "opencrab-team-mcp-startup-{}.log",
        std::process::id()
    ));
    let _ = std::fs::write(
        &sentinel,
        format!(
            "started pid={} at {:?}\nargs: {:?}\n",
            std::process::id(),
            std::time::SystemTime::now(),
            std::env::args().collect::<Vec<_>>()
        ),
    );
    eprintln!(
        "[opencrab-team-mcp] startup sentinel: {}",
        sentinel.display()
    );
    match server::run_stdio().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[opencrab-team-mcp] server error: {err}");
            ExitCode::FAILURE
        }
    }
}
