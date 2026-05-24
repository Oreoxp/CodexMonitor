// P6 Step 9 — opencrab-eval: a behavioural eval harness for OpenCrab Team
// Mode prompts.
//
// What it does (the "form (b)" path from step 8 §E.6):
//   per-fixture tempdir + spawned codex-app-server child + `compose.mts`
//   bridge → real `composeDeveloperInstructions` + `finalizeSystemPromptFor
//   Codex` bytes → ws thread/start → kickoff turn → fixture user turn →
//   collect final text → assertion + raw transcript printed.
//
// Architecture:
//   - This bin is part of the `codex-monitor` Cargo package. We pull the
//     production seed code (`bootstrap`, `team_config::types`, `paths`)
//     into the bin via `#[path]`-include — same pattern the
//     `codex_monitor_daemon` bin uses for code it has to share. Keeps the
//     eval bin self-contained without touching the lib's public surface.
//   - Per-fixture isolation: distinct `$HOME` / `$CODEX_HOME` → distinct
//     codex-app-server child → distinct ephemeral port.
//   - Default skip: if either `$OPENCRAB_EVAL_BEARER` (DashScope token)
//     or `$OPENCRAB_EVAL_CODEX_APP_SERVER` (path to the codex-app-server
//     binary) is unset, every fixture is reported as `SKIPPED` and the
//     process exits 0. This is what makes the bin safe to leave in `cargo
//     build` output and CI matrices.
//   - On run: each fixture's verdict is independent. A failure prints the
//     raw transcript + assertion notes but never aborts the loop —
//     aggregate report is the deliverable.

// -- Path-included production modules ----------------------------------------
//
// `bootstrap` is the seed-templates source of truth; we use its
// `ensure_user_layer_at` so the eval's per-agent SOUL / IDENTITY / ROLE
// bytes are byte-identical to production's. `team_config::types`
// supplies the TeamConfig schema. `paths` is bootstrap's own dependency.

#[allow(dead_code)]
#[path = "../../paths.rs"]
mod paths;

// `bootstrap/mod.rs` references `crate::team_config::types::AgentConfig`.
// Path-includes on inline submodules resolve relative to the virtual
// directory (`src/bin/opencrab-eval/team_config/`), which doesn't exist on
// disk — so we follow the daemon bin's pattern: include the source file at
// the top level, then expose it under the expected path via a re-export.
#[allow(dead_code)]
#[path = "../../team_config/types.rs"]
mod team_config_types;

mod team_config {
    pub mod types {
        pub(crate) use crate::team_config_types::*;
    }
}

#[allow(dead_code)]
#[path = "../../bootstrap/mod.rs"]
mod bootstrap;

// -- Eval bin modules --------------------------------------------------------

mod assertions;
mod compose;
mod fixture;
mod runner;
mod ws_rpc;

use std::path::PathBuf;
use std::process::ExitCode;

use crate::fixture::FIXTURES;
use crate::runner::RunContext;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let bearer = std::env::var("OPENCRAB_EVAL_BEARER").ok().filter(|v| !v.trim().is_empty());
    let codex_bin = std::env::var("OPENCRAB_EVAL_CODEX_APP_SERVER")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from);

    if bearer.is_none() || codex_bin.is_none() {
        print_skip_banner(bearer.is_some(), codex_bin.is_some());
        return ExitCode::SUCCESS;
    }

    let codex_bin = codex_bin.unwrap();
    if !codex_bin.exists() {
        eprintln!("[opencrab-eval] codex-app-server binary not found: {}", codex_bin.display());
        eprintln!("                set $OPENCRAB_EVAL_CODEX_APP_SERVER to an absolute path");
        return ExitCode::from(2);
    }

    let sidecar_root = match compose::resolve_sidecar_root() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("[opencrab-eval] sidecar resolution failed: {err}");
            return ExitCode::from(2);
        }
    };

    // Step 18 spike — resolve the `opencrab-team-mcp` stub. When found,
    // every fixture runs in tool-mode (MCP server registered + comm guide
    // swapped). When not found, eval falls back to text-tag mode.
    let team_mcp = resolve_team_mcp_binary();

    let ctx = RunContext {
        bearer: bearer.unwrap(),
        sidecar_root,
        codex_app_server: codex_bin,
        team_mcp: team_mcp.clone(),
    };

    println!("opencrab-eval — running {} fixture(s)\n", FIXTURES.len());
    println!("sidecar root:     {}", ctx.sidecar_root.display());
    println!("codex-app-server: {}", ctx.codex_app_server.display());
    match &ctx.team_mcp {
        Some(p) => println!("team-mcp (Step 18 spike): {}", p.display()),
        None => {
            println!("team-mcp (Step 18 spike): NOT FOUND — running text-tag mode");
            println!("  (set $OPENCRAB_EVAL_TEAM_MCP or build opencrab-team-mcp into the same dir)");
        }
    }
    println!();

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut errored = 0usize;

    for fixture in FIXTURES {
        println!("{}", "─".repeat(78));
        println!("FIXTURE  {}", fixture.id);
        println!("         {}", fixture.description);
        println!("USER →   {}", abbreviate_for_log(fixture.user_turn));
        println!();

        match runner::run_fixture(&ctx, fixture).await {
            Ok(turn) => {
                println!("AGENT FINAL TEXT");
                println!("----");
                println!("{}", turn.final_text);
                println!("----");
                println!();
                let outcome = (fixture.assert)(&turn);
                if outcome.passed {
                    println!("VERDICT  ✓ PASS — {}", outcome.notes);
                    passed += 1;
                } else {
                    println!("VERDICT  ✗ FAIL — {}", outcome.notes);
                    failed += 1;
                }
            }
            Err(err) => {
                println!("VERDICT  ⚠ ERROR — harness failed before assertion: {err}");
                errored += 1;
            }
        }
        println!();
    }

    println!("{}", "═".repeat(78));
    println!(
        "SUMMARY  passed={passed}  failed={failed}  error={errored}  total={}",
        FIXTURES.len()
    );

    // Non-zero exit if anything failed or errored — useful for CI gating
    // when the user opts in. Pure pass = exit 0.
    if failed + errored == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn print_skip_banner(has_bearer: bool, has_codex: bool) {
    println!("opencrab-eval — SKIPPED");
    println!();
    println!(
        "  $OPENCRAB_EVAL_BEARER            {}",
        if has_bearer { "set" } else { "NOT SET (required: DashScope/Qwen token)" }
    );
    println!(
        "  $OPENCRAB_EVAL_CODEX_APP_SERVER  {}",
        if has_codex {
            "set"
        } else {
            "NOT SET (required: path to codex-app-server binary, e.g. \
             <repo>/codex-cli/codex-rs/target/debug/codex-app-server)"
        }
    );
    println!();
    println!("Set both env vars to actually run the fixtures (real-model calls — costs tokens).");
    println!("See `src/bin/opencrab-eval/README.md` for the full runbook.");
}

/// Step 18 — locate the stub `opencrab-team-mcp` binary. Resolution
/// order: `$OPENCRAB_EVAL_TEAM_MCP` env override -> sibling of the running
/// eval binary (same `target/debug/`). Returns `None` if neither path
/// produces a file; eval falls back to text-tag mode in that case.
fn resolve_team_mcp_binary() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("OPENCRAB_EVAL_TEAM_MCP") {
        let p = PathBuf::from(value.trim());
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for candidate in ["opencrab-team-mcp", "opencrab-team-mcp.exe"] {
        let p = dir.join(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn abbreviate_for_log(text: &str) -> String {
    const MAX: usize = 200;
    let single_line: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if single_line.chars().count() <= MAX {
        single_line
    } else {
        let truncated: String = single_line.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}
