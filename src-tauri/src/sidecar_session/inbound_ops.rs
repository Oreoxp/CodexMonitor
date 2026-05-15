// Reverse-RPC handlers — sidecar → Tauri.
//
// Phase 2 pivot surface:
//   - codex_start_thread: provision an agent's normal-mode Codex thread
//     (HYBRID developer_instructions). Used at sidecar init for each
//     team.json agent missing a `threadId`.
//   - team_router_start: Tauri replaces its workspace-scoped router with
//     the supplied agent roster + subscription topology, registering
//     permanent taps and spawning per-thread consumer tasks. The router
//     parses `<send_message>` tags from each turn's final text and
//     dispatches them via normal-mode `send_user_message_to_thread`.
//
// `codex_resume_thread` and `codex_send_user_message` are gone — sidecar no
// longer drives turns, so neither op has a caller.

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
    params.get(key).and_then(Value::as_str).map(|s| s.to_string())
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
    let workspace_path = crate::shared::codex_core::resolve_workspace_path_core(
        &state.workspaces,
        &workspace_id,
    )
    .await?;
    let mut start_params = Map::new();
    start_params.insert("cwd".to_string(), json!(workspace_path));
    start_params.insert("approvalPolicy".to_string(), json!("on-request"));
    if let Some(dev) = developer_instructions {
        start_params.insert("developerInstructions".to_string(), json!(dev));
    }
    let raw = session
        .send_request_for_workspace(
            &workspace_id,
            "thread/start",
            Value::Object(start_params),
        )
        .await?;

    let thread_id = extract_thread_id(&raw);
    Ok(json!({
        "threadId": thread_id,
        "raw": raw,
    }))
}

async fn handle_team_router_start(
    state: &AppState,
    app_handle: &AppHandle,
    params: &Value,
) -> Result<Value, String> {
    let workspace_id = required_str(params, "workspace_id")?;
    let agents: Vec<crate::sidecar_session::team_router::AgentInfo> =
        serde_json::from_value(params.get("agents").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid `agents`: {}", e))?;
    let subscriptions: Vec<crate::sidecar_session::team_router::Subscription> =
        serde_json::from_value(params.get("subscriptions").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid `subscriptions`: {}", e))?;

    state
        .team_routers
        .start(app_handle.clone(), workspace_id, agents, subscriptions)
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
