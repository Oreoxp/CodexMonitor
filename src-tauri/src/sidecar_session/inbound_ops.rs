// Reverse-RPC handlers — sidecar → Tauri.
//
// Spike B surface area is intentionally narrow: just the three Codex calls
// `pm_say` needs to drive a Codex thread. Anything else returns an
// `unknown op` error which the sidecar surfaces as a rejection.
//
// These handlers run in the stdout reader task. They MUST NOT take long-lived
// locks beyond what the existing `codex_core::*_core` helpers already do.

use serde_json::{json, Value};

use crate::state::AppState;

pub(crate) async fn dispatch_inbound_op(
    state: &AppState,
    op: &str,
    params: &Value,
) -> Result<Value, String> {
    match op {
        "codex_start_thread" => handle_codex_start_thread(state, params).await,
        "codex_resume_thread" => handle_codex_resume_thread(state, params).await,
        "codex_send_user_message" => handle_codex_send_user_message(state, params).await,
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

async fn handle_codex_start_thread(state: &AppState, params: &Value) -> Result<Value, String> {
    let workspace_id = required_str(params, "workspace_id")?;
    let raw = crate::shared::codex_core::start_thread_core(
        &state.sessions,
        &state.workspaces,
        workspace_id,
    )
    .await?;
    // Normalize: hand the sidecar a top-level `threadId` even if Codex nested
    // it (varies by upstream version). Keep the original payload too for debug.
    let thread_id = extract_thread_id(&raw);
    Ok(json!({
        "threadId": thread_id,
        "raw": raw,
    }))
}

async fn handle_codex_resume_thread(state: &AppState, params: &Value) -> Result<Value, String> {
    let workspace_id = required_str(params, "workspace_id")?;
    let thread_id = required_str(params, "thread_id")?;
    let raw = crate::shared::codex_core::resume_thread_core(
        &state.sessions,
        workspace_id,
        thread_id.clone(),
    )
    .await?;
    Ok(json!({ "threadId": thread_id, "raw": raw }))
}

async fn handle_codex_send_user_message(
    state: &AppState,
    params: &Value,
) -> Result<Value, String> {
    let workspace_id = required_str(params, "workspace_id")?;
    let thread_id = required_str(params, "thread_id")?;
    let text = required_str(params, "text")?;
    // Spike defaults: no model override, no effort/service-tier, no images or
    // mentions, no collaboration mode. access_mode=None falls through to
    // `current` which gives workspaceWrite + network + on-request approvals.
    let raw = crate::shared::codex_core::send_user_message_core(
        &state.sessions,
        &state.workspaces,
        workspace_id,
        thread_id,
        text,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await?;
    Ok(json!({ "raw": raw }))
}

fn extract_thread_id(value: &Value) -> Option<String> {
    // `send_request_for_workspace` returns the full JSON-RPC envelope, so the
    // real payload lives under `result.…`. Probe both with and without the
    // `result` unwrap in case a caller hands us pre-unwrapped data.
    // Codex's `thread/start` response shape (as of 2026-05-14): the thread id
    // is at `result.thread.id` (with `result.thread.sessionId` as same-value
    // fallback). Older shapes used flat `threadId` / `thread_id` keys.
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
        &["turn", "threadId"],
        &["turn", "thread_id"],
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
