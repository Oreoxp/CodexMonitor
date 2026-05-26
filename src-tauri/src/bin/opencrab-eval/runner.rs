// Per-fixture lifecycle.
//
// For each fixture, this module:
//   1. Builds a per-fixture tempdir with `$HOME` / `$CODEX_HOME` layout.
//   2. Writes `team.json` (from fixture bytes) and `config.toml` (from the
//      static template with bearer interpolated from $OPENCRAB_EVAL_BEARER).
//   3. Calls the production `bootstrap::ensure_user_layer_at` to seed SOUL /
//      IDENTITY / ROLE / USER / MEMORY into `<home>/.opencrab/agents/<id>/`
//      with EXACTLY the bytes prod uses.
//   4. Spawns a per-fixture `codex-app-server` child (so each fixture sees
//      its own frozen CODEX_HOME) on a random localhost port.
//   5. Polls-connects until the ws is ready.
//   6. Runs `compose.mts` via tsx in the sidecar dir → gets developer
//      instructions + kickoff prompt, both byte-identical to the real
//      provisioning path.
//   7. Drives the thread: `initialize` → `thread/start` → `sendUserMessage
//      (kickoff)` → drain → `sendUserMessage (fixture turn)` → drain final.
//   8. Returns a `TurnResult` for the assertion to inspect.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout, Instant};

use crate::assertions::TurnResult;
use crate::bootstrap;
use crate::compose::{self, ComposeOutput};
use crate::fixture::Fixture;
use crate::team_config::types::TeamConfig;
use crate::ws_rpc::RpcClient;

const CLIENT_VERSION: &str = "opencrab-eval/0.1";
const TURN_BUDGET: Duration = Duration::from_secs(180);

pub struct RunContext {
    pub bearer: String,
    pub sidecar_root: PathBuf,
    pub codex_app_server: PathBuf,
    /// Step 18 spike — path to the `opencrab-team-mcp` stub binary. When
    /// `Some`, the eval registers it under `mcp_servers.opencrab-team` in
    /// every fixture's `thread/start` config and asks compose.mts for the
    /// tool-mode comm guide. When `None`, eval runs in the original
    /// text-tag mode (assertions still scan agent text).
    pub team_mcp: Option<PathBuf>,
}

pub struct FixtureRun {
    pub tempdir: PathBuf,
    pub workspace_dir: PathBuf,
    /// Kept alive for the duration of the run; killed on drop.
    pub child: Child,
    pub ws_url: String,
    pub developer_instructions: String,
    pub kickoff_prompt: String,
}

impl Drop for FixtureRun {
    fn drop(&mut self) {
        // Best-effort: kill the codex-app-server child + remove tempdir.
        // We can't .await in Drop; start_kill is sync.
        let _ = self.child.start_kill();
        // P6 Step 5 — opt in via OPENCRAB_EVAL_KEEP_TEMPDIR to preserve the
        // tempdir for post-run inspection (e.g. `<home>/.opencrab/agents/
        // _eval/memory.db` for memory-tool fixtures). When set, also print
        // the path to stderr so the caller can find it without grepping.
        if std::env::var("OPENCRAB_EVAL_KEEP_TEMPDIR").is_ok() {
            eprintln!(
                "[opencrab-eval] OPENCRAB_EVAL_KEEP_TEMPDIR set — preserving tempdir: {}",
                self.tempdir.display()
            );
        } else {
            let _ = std::fs::remove_dir_all(&self.tempdir);
        }
    }
}

pub async fn run_fixture(ctx: &RunContext, fixture: &Fixture) -> Result<TurnResult, String> {
    let mut run = setup_fixture_environment(ctx, fixture).await?;

    let mut rpc = RpcClient::connect(&run.ws_url, CLIENT_VERSION).await?;

    // thread/start — matches `handle_codex_start_thread`. We skip the MCP
    // memory-server config (eval doesn't test memory tools) and the
    // sessionDir (codex defaults are fine for an ephemeral fixture).
    //
    // Step 18 spike: when `team_mcp` is set we register the stub team
    // MCP server under `mcp_servers.opencrab-team`. codex starts a child
    // for the server and the tool schemas land in the model's `tools`
    // surface alongside any built-in tools.
    let cwd_str = run.workspace_dir.to_string_lossy().into_owned();
    let mut start_params = serde_json::Map::new();
    start_params.insert("cwd".to_string(), json!(cwd_str));
    start_params.insert("approvalPolicy".to_string(), json!("never"));
    start_params.insert("sandbox".to_string(), json!("danger-full-access"));
    start_params.insert(
        "developerInstructions".to_string(),
        json!(run.developer_instructions),
    );
    if let Some(team_mcp_path) = ctx.team_mcp.as_deref() {
        let mut mcp_cfg = serde_json::Map::new();
        mcp_cfg.insert(
            "mcp_servers.opencrab-team".to_string(),
            json!({
                "command": team_mcp_path.to_string_lossy(),
                "args": [],
            }),
        );
        // Step 19 diagnostic — ALSO register the production memory MCP
        // server. If the model calls memory_search/memory_get but not
        // send_message/propose_plan, we know qwen+codex+MCP works
        // generally and the spike's failure is something specific to
        // opencrab-team. If neither set surfaces, the diagnosis is
        // "qwen3.5-plus has no model metadata → MCP path degraded".
        // The server resolves its own `~/.opencrab/agents/_eval/memory.db`
        // from `--agent-id` (P6 Step 1) — no path passes through here.
        let memory_mcp_path = team_mcp_path
            .parent()
            .map(|p| p.join("opencrab-memory-mcp"))
            .filter(|p| p.exists());
        if let Some(memory_path) = memory_mcp_path {
            mcp_cfg.insert(
                "mcp_servers.opencrab-memory".to_string(),
                json!({
                    "command": memory_path.to_string_lossy(),
                    "args": ["--agent-id", "_eval"],
                }),
            );
        }
        start_params.insert("config".to_string(), Value::Object(mcp_cfg));
    }
    let start_resp = rpc
        .request_with_timeout(
            "thread/start",
            Value::Object(start_params),
            Duration::from_secs(30),
        )
        .await?;
    let thread_id = extract_thread_id(&start_resp).ok_or_else(|| {
        format!(
            "thread/start response missing thread id: {}",
            start_resp
        )
    })?;

    // Kickoff turn — materializes the rollout (production path does this
    // via `sendKickoffMessageWithRetry`). Sent as `turn/start` (a REQUEST,
    // not a notification — the response just carries the turn id; the
    // agent's actual processing streams in via notifications until
    // `turn/completed`). The shape of `input` is the `UserInput::Text`
    // variant of the codex protocol: `#[serde(tag = "type", rename_all =
    // "camelCase")]` → `{ "type": "text", "text": "...", "textElements": [] }`.
    let _kickoff_resp = rpc
        .request_with_timeout(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{
                    "type": "text",
                    "text": run.kickoff_prompt,
                    "textElements": [],
                }],
            }),
            Duration::from_secs(60),
        )
        .await?;
    let _kickoff_drain = rpc
        .drain_notifications_until("turn/completed", TURN_BUDGET)
        .await?;

    // The fixture's real user turn.
    let _fixture_turn_resp = rpc
        .request_with_timeout(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{
                    "type": "text",
                    "text": fixture.user_turn,
                    "textElements": [],
                }],
            }),
            Duration::from_secs(60),
        )
        .await?;
    let turn_notifs = rpc
        .drain_notifications_until("turn/completed", TURN_BUDGET)
        .await?;

    // Step 18 spike debug — when OPENCRAB_EVAL_DUMP_NOTIFS is set, print a
    // tally of notification methods + every mcpToolCall item's
    // (server, tool, has_args) seen on this turn. Helps diagnose
    // "no tool call observed" failures (model didn't call vs codex
    // didn't route vs runner scan misses).
    if std::env::var("OPENCRAB_EVAL_DUMP_NOTIFS").is_ok() {
        eprintln!("[notif-dump] fixture turn notifications ({}):", turn_notifs.len());
        let mut by_method: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for (m, _) in &turn_notifs {
            *by_method.entry(m.clone()).or_insert(0) += 1;
        }
        for (m, n) in &by_method {
            eprintln!("[notif-dump]   {n:3}x  {m}");
        }
        for (method, params) in &turn_notifs {
            if method != "item/completed" {
                continue;
            }
            let Some(item) = params.get("item") else {
                continue;
            };
            let Some(t) = item.get("type").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if t == "mcpToolCall" {
                eprintln!(
                    "[notif-dump]   mcpToolCall server={} tool={} args_keys={:?}",
                    item.get("server").and_then(serde_json::Value::as_str).unwrap_or("?"),
                    item.get("tool").and_then(serde_json::Value::as_str).unwrap_or("?"),
                    item.get("arguments")
                        .and_then(serde_json::Value::as_object)
                        .map(|o| o.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default(),
                );
            }
        }
        // Also dump every `warning` notification so we can see what codex
        // (or the model provider) is telling us about the turn.
        for (method, params) in &turn_notifs {
            if method == "warning" {
                eprintln!("[notif-dump]   warning params={}", params);
            }
        }
    }

    rpc.close().await;

    // Pull final assistant text out of the captured notification stream.
    // We accept either the streaming `agentMessage` payload or the
    // completed-item text — codex versions vary on which lands first.
    let final_text = extract_final_text(&turn_notifs);

    // Stop the child explicitly here (rather than letting Drop do it
    // best-effort) so the next fixture's spawn doesn't race with this
    // one's port hold.
    let _ = run.child.start_kill();
    let _ = timeout(Duration::from_secs(5), run.child.wait()).await;

    Ok(TurnResult {
        final_text,
        notifications: turn_notifs,
    })
}

// ---------------------------------------------------------------------------
// Environment setup
// ---------------------------------------------------------------------------

async fn setup_fixture_environment(
    ctx: &RunContext,
    fixture: &Fixture,
) -> Result<FixtureRun, String> {
    let tempdir = std::env::temp_dir().join(format!(
        "opencrab-eval-{}-{}",
        fixture.id,
        uuid::Uuid::new_v4()
    ));
    let home_dir = tempdir.join("home");
    let user_data_dir = home_dir.join(".opencrab");
    let workspace_dir = tempdir.join("workspace");
    let project_data_dir = workspace_dir.join(".opencrab");

    std::fs::create_dir_all(&user_data_dir)
        .map_err(|err| format!("mkdir {}: {err}", user_data_dir.display()))?;
    std::fs::create_dir_all(&workspace_dir)
        .map_err(|err| format!("mkdir {}: {err}", workspace_dir.display()))?;

    // 1. team.json
    let team_json_path = user_data_dir.join("team.json");
    std::fs::write(&team_json_path, fixture.team_json)
        .map_err(|err| format!("write team.json: {err}"))?;

    // 2. config.toml — points codex at the user's Qwen provider with the
    // bearer injected from env. `personality` left blank deliberately
    // (P6 Step 7: eval tests intended prompt, no global personality_spec).
    // Step 18: also embed `[mcp_servers.opencrab-team]` here so codex
    // discovers the stub at config-load time even if the thread/start
    // override path drops it.
    let config_toml = build_config_toml(&ctx.bearer, ctx.team_mcp.as_deref());
    std::fs::write(user_data_dir.join("config.toml"), config_toml)
        .map_err(|err| format!("write config.toml: {err}"))?;

    // 3. bootstrap — production path. Writes SOUL / IDENTITY / ROLE / USER
    // / MEMORY with the exact bytes prod uses.
    let team: TeamConfig = serde_json::from_str(fixture.team_json)
        .map_err(|err| format!("parse fixture team.json: {err}"))?;
    bootstrap::ensure_user_layer_at(&user_data_dir, &team.agents)
        .map_err(|err| format!("ensure_user_layer_at: {err}"))?;
    bootstrap::ensure_project_layer_at(&project_data_dir, &team.id, &team.agents)
        .map_err(|err| format!("ensure_project_layer_at: {err}"))?;

    // 4. compose.mts — get the on-wire developer_instructions + kickoff.
    //    Step 18: tools_mode = ctx.team_mcp.is_some() — when we register
    //    the stub MCP server, also swap the comm guide to tool-mode prose.
    let compose_output: ComposeOutput = compose::compose(
        &ctx.sidecar_root,
        &tempdir,
        &user_data_dir,
        &project_data_dir,
        fixture.user_correspondent_id,
        ctx.team_mcp.is_some(),
    )
    .await?;

    // 5. Pick a free port + spawn codex-app-server.
    let port = pick_free_port()?;
    let ws_url = format!("ws://127.0.0.1:{port}");
    let child = spawn_codex_app_server(&ctx.codex_app_server, &home_dir, port).await?;

    // 6. Wait for ws ready (poll-connect, up to ~10s).
    wait_for_ws_ready(&ws_url).await?;

    Ok(FixtureRun {
        tempdir,
        workspace_dir,
        child,
        ws_url,
        developer_instructions: compose_output.developer_instructions,
        kickoff_prompt: compose_output.kickoff_prompt,
    })
}

fn build_config_toml(bearer: &str, team_mcp: Option<&Path>) -> String {
    // Mirrors the user's actual `~/.opencrab/config.toml` shape so the eval
    // exercises the same provider path. Bearer is the only injected secret.
    //
    // Step 18 spike — when `team_mcp` is Some, also register the stub
    // server directly in the config.toml under `[mcp_servers.opencrab-team]`.
    // Codex picks this up at config-load time (same path memory MCP uses in
    // the user's normal config); production code paths register it via
    // `thread/start config` overrides instead but the codex-side merge is
    // the same. Writing it here side-steps any thread/start override
    // shape issues — useful as a sanity check during the spike.
    let mut out = format!(
        r#"model = "qwen3.5-plus"
model_provider = "opencrab"

[model_providers]

[model_providers.opencrab]
name = "opencrab"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
experimental_bearer_token = "{bearer}"
wire_api = "responses"

[features]
collaboration_modes = true
steer = true
unified_exec = true
apps = false
"#
    );
    if let Some(path) = team_mcp {
        out.push_str(&format!(
            r#"
[mcp_servers.opencrab-team]
command = "{}"
args = []
"#,
            path.display()
        ));
    }
    out
}

fn pick_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|err| format!("bind random port: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("local_addr: {err}"))?
        .port();
    // Drop closes the listener so the port goes back to the OS. There's a
    // small race between drop and codex-app-server claiming it; in
    // practice codex-app-server retries on bind failure and this works.
    drop(listener);
    Ok(port)
}

async fn spawn_codex_app_server(
    binary: &Path,
    home_dir: &Path,
    port: u16,
) -> Result<Child, String> {
    let codex_home = home_dir.join(".opencrab");
    Command::new(binary)
        .arg("--listen")
        .arg(format!("ws://127.0.0.1:{port}"))
        .env("HOME", home_dir)
        .env("CODEX_HOME", &codex_home)
        // Quiet by default — flip via OPENCRAB_EVAL_VERBOSE.
        .env("RUST_LOG", std::env::var("OPENCRAB_EVAL_VERBOSE").unwrap_or_else(|_| "warn".into()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Step 18 spike debug — let codex stderr through when
        // OPENCRAB_EVAL_DUMP_NOTIFS is set, so MCP spawn / tool
        // registration errors surface. Otherwise stay quiet.
        .stderr(if std::env::var("OPENCRAB_EVAL_DUMP_NOTIFS").is_ok() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("spawn codex-app-server: {err}"))
}

async fn wait_for_ws_ready(url: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut attempt = 0u32;
    while Instant::now() < deadline {
        attempt += 1;
        match tokio_tungstenite::connect_async(url).await {
            Ok((_socket, _resp)) => return Ok(()),
            Err(_) => {
                sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Err(format!(
        "codex-app-server at {url} never became reachable ({attempt} attempts in 15s)"
    ))
}

// ---------------------------------------------------------------------------
// Notification → final text extraction
// ---------------------------------------------------------------------------

fn extract_thread_id(value: &Value) -> Option<String> {
    // Same shapes the production `state.ts::extractThreadId` walks; we mirror
    // a subset here because we only need to parse the start response.
    if let Some(s) = value.get("threadId").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    if let Some(s) = value
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
    {
        return Some(s.to_string());
    }
    if let Some(s) = value
        .get("result")
        .and_then(|r| r.get("thread"))
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
    {
        return Some(s.to_string());
    }
    None
}

/// Pull the agent's final assistant text from a turn's notification
/// stream.
///
/// Strategy (per the codex v2 protocol's `ThreadItem` enum,
/// `app-server-protocol/src/protocol/v2/item.rs`):
///
///   * Only `item/completed` notifications carry the final state of an
///     item. `item/started` carries the same id with an in-progress
///     payload — including it caused step-10 (續) transcripts to show
///     every agent message twice, and inflated the fixture-01 verdict
///     count from 1 plan / 6 tasks to a false 2 plans / 12 tasks.
///   * Only items with `type == "agentMessage"` are the assistant's reply
///     text. `userMessage` items would echo the user_turn back into the
///     transcript (step-10 (續) also surfaced this — every fixture's
///     transcript opened with the user_turn pasted twice).
///   * Dedupe by `item.id` defensively, even though `item/completed` is
///     specified to fire once per item: protects against future protocol
///     drift where a refresh / steer might re-emit `completed`.
///
/// Multiple AgentMessage items per turn (long replies split into chunks)
/// are joined with a blank line. Tags like `<propose_plan>` / `<send_message>`
/// land inline inside the agent's text and survive verbatim — assertions
/// scan strings directly, no extra unwrapping needed.
fn extract_final_text(notifs: &[(String, Value)]) -> String {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut chunks: Vec<&str> = Vec::new();
    for (method, params) in notifs {
        if method != "item/completed" {
            continue;
        }
        let Some(item) = params.get("item") else {
            continue;
        };
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if item_type != "agentMessage" {
            continue;
        }
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                chunks.push(text);
            }
        }
    }
    chunks.join("\n\n")
}
