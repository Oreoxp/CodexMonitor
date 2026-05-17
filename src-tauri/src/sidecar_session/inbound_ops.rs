// Reverse-RPC handlers — sidecar → Tauri.
//
// Phase 2 pivot surface:
//   - codex_start_thread: provision an agent's normal-mode Codex thread
//     (HYBRID developer_instructions). Used at sidecar init for each
//     team.json agent missing a `threadId`.
//   - codex_send_user_message: send a kickoff user message turn to a
//     freshly-provisioned thread, ONCE, so its rollout materializes and
//     subsequent `thread/resume` from the frontend doesn't fail with
//     `"no rollout found for thread id ..."`. See upstream test
//     `thread_resume_rejects_unmaterialized_thread`. Sidecar only calls this
//     from `provisionAndStartRouter`; not exposed as a general send.
//   - team_router_start: Tauri replaces its workspace-scoped router with
//     the supplied agent roster + subscription topology, registering
//     permanent taps and spawning per-thread consumer tasks. The router
//     parses `<send_message>` tags from each turn's final text and
//     dispatches them via normal-mode `send_user_message_to_thread`.
//
// `codex_resume_thread` is gone — sidecar no longer drives resumes; the
// frontend's normal-mode `useThreads` owns resume directly.

use serde_json::{json, Map, Value};
use tauri::AppHandle;

use crate::state::AppState;

pub(crate) async fn dispatch_inbound_op(
    state: &AppState,
    app_handle: &AppHandle,
    op: &str,
    params: &Value,
) -> Result<Value, String> {
    match op {
        "codex_start_thread" => handle_codex_start_thread(state, params).await,
        "codex_send_user_message" => handle_codex_send_user_message(state, params).await,
        "team_router_start" => handle_team_router_start(state, app_handle, params).await,
        other => Err(format!("unknown op: {}", other)),
    }
}

fn required_str(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string param `{}`", key))
}

fn optional_str(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

async fn handle_codex_start_thread(state: &AppState, params: &Value) -> Result<Value, String> {
    let workspace_id = required_str(params, "workspace_id")?;
    let developer_instructions = optional_str(params, "developer_instructions");

    // HYBRID injection — see docs/architecture/opencrab-3.0-prompt-strategy.md.
    // The agent role + comm guide is passed as `developerInstructions`;
    // `baseInstructions` is deliberately left unset so Codex keeps its default
    // coding-agent prompt and safety guardrails.
    let session =
        crate::shared::codex_core::get_session_clone(&state.sessions, &workspace_id).await?;
    let workspace_path =
        crate::shared::codex_core::resolve_workspace_path_core(&state.workspaces, &workspace_id)
            .await?;
    let mut start_params = Map::new();
    start_params.insert("cwd".to_string(), json!(workspace_path));
    // Phase 2 demo: team agents run with `approvalPolicy: "never"` so the
    // kickoff turn (and any subsequent turn that inherits the thread default)
    // never blocks waiting for human-in-the-loop approval. The kickoff prompt
    // asks for plain-prose ack only, but Codex's base coding-agent prompt
    // can nudge the model into a probe tool call on its first turn; with
    // `on-request` that probe would suspend the turn and the user would see
    // a stalled empty thread. Phase 2 is single-machine isolated demo
    // territory — no real human-in-the-loop value to gate on. (Per-turn
    // approvals from later normal-mode `send_user_message_to_thread` calls
    // still flow through `send_user_message_core`'s access-mode mapping.)
    start_params.insert("approvalPolicy".to_string(), json!("never"));
    if let Some(dev) = developer_instructions {
        start_params.insert("developerInstructions".to_string(), json!(dev));
    }
    let raw = session
        .send_request_for_workspace(&workspace_id, "thread/start", Value::Object(start_params))
        .await?;

    let thread_id = extract_thread_id(&raw);
    Ok(json!({
        "threadId": thread_id,
        "raw": raw,
    }))
}

async fn handle_codex_send_user_message(state: &AppState, params: &Value) -> Result<Value, String> {
    let workspace_id = required_str(params, "workspace_id")?;
    let thread_id = required_str(params, "thread_id")?;
    let text = required_str(params, "text")?;

    // Phase 2 team kickoff path. We deliberately pass `access_mode = "full-access"`
    // because `send_user_message_core` (shared/codex_core.rs) hard-codes the
    // per-turn `approvalPolicy` derived from access_mode and ALWAYS inserts it
    // into the turn/start params — overriding the thread-level
    // `approvalPolicy: "never"` set in `handle_codex_start_thread` above.
    // Of the three access_mode branches in `send_user_message_core`, only
    // "full-access" maps to `approvalPolicy: "never"` (with sandbox
    // `dangerFullAccess`). For an isolated Phase 2 demo with no
    // human-in-the-loop, that's the correct posture: the kickoff prompt asks
    // for a plain-prose ack only, but Codex's base coding-agent prompt can
    // still nudge the model into a probe tool call on its very first turn —
    // we don't want that probe to stall the turn behind an approval gate.
    // (Normal-mode user sends are unaffected: they go through the Tauri
    // `send_user_message_to_thread` command, not this reverse-RPC handler.)
    let result = crate::shared::codex_core::send_user_message_core(
        &state.sessions,
        &state.workspaces,
        workspace_id,
        thread_id.clone(),
        text,
        /*model*/ None,
        /*effort*/ None,
        /*service_tier*/ None,
        /*access_mode*/ Some("full-access".to_string()),
        /*images*/ None,
        /*app_mentions*/ None,
        /*collaboration_mode*/ None,
    )
    .await;
    // Grep-friendly log: `[codex_send_user_message] thread=<id> failed: ...`
    // covers the Err arm; for the Ok arm we also surface the case where the
    // Codex response carries an `error` field that send_user_message_core
    // already maps into `Err` — but only the Err arm reaches here, so a
    // single log site is sufficient.
    if let Err(err) = &result {
        eprintln!(
            "[codex_send_user_message] thread={} failed: {}",
            thread_id, err
        );
    }
    result
}

async fn handle_team_router_start(
    state: &AppState,
    app_handle: &AppHandle,
    params: &Value,
) -> Result<Value, String> {
    let workspace_id = required_str(params, "workspace_id")?;
    // `team_id` is required for the Step-2 propose_plan write path. Step 1
    // teams that predate this field would land here with `team_id` missing —
    // that's a sidecar-side bug (sidecar always has team.id loaded when it
    // calls `team_router_start`), so we surface it as a hard error rather
    // than papering over it.
    let team_id = required_str(params, "team_id")?;
    let agents: Vec<crate::sidecar_session::team_router::AgentInfo> =
        serde_json::from_value(params.get("agents").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid `agents`: {}", e))?;
    let subscriptions: Vec<crate::sidecar_session::team_router::Subscription> =
        serde_json::from_value(params.get("subscriptions").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid `subscriptions`: {}", e))?;

    state
        .team_routers
        .start(
            app_handle.clone(),
            workspace_id,
            team_id,
            agents,
            subscriptions,
        )
        .await?;
    Ok(json!({ "ok": true }))
}

fn extract_thread_id(value: &Value) -> Option<String> {
    let candidates: &[&[&str]] = &[
        &["result", "thread", "id"],
        &["result", "thread", "sessionId"],
        &["thread", "id"],
        &["thread", "sessionId"],
        &["threadId"],
        &["thread_id"],
        &["thread", "threadId"],
        &["thread", "thread_id"],
        &["result", "threadId"],
        &["result", "thread_id"],
    ];
    for path in candidates {
        let mut cur = value;
        let mut ok = true;
        for seg in path.iter() {
            match cur.get(*seg) {
                Some(next) => cur = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        if let Some(s) = cur.as_str() {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}
