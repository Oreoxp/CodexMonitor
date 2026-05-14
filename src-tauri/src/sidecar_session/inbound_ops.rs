// Reverse-RPC handlers — sidecar → Tauri.
//
// Spike B surface area is intentionally narrow: just the three Codex calls
// `pm_say` needs to drive a Codex thread. Anything else returns an
// `unknown op` error which the sidecar surfaces as a rejection.
//
// These handlers run in the stdout reader task. They MUST NOT take long-lived
// locks beyond what the existing `codex_core::*_core` helpers already do.

use serde_json::{json, Map, Value};

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

fn optional_str(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(|s| s.to_string())
}

async fn handle_codex_start_thread(state: &AppState, params: &Value) -> Result<Value, String> {
    let workspace_id = required_str(params, "workspace_id")?;
    let developer_instructions = optional_str(params, "developer_instructions");

    // HYBRID injection — see docs/architecture/opencrab-3.0-prompt-strategy.md.
    // The PM role is passed as `developerInstructions`; `baseInstructions` is
    // deliberately left unset so Codex keeps its default coding-agent prompt
    // and safety guardrails. The `thread/start` params are built inline here
    // (rather than via `start_thread_core`) so the normal-mode thread/start
    // path stays byte-identical — `start_thread_core` has other callers.
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

    // Normalize: hand the sidecar a top-level `threadId` even if Codex nested
    // it (varies by upstream version). Keep the original payload too for debug.
    let thread_id = extract_thread_id(&raw);
    Ok(json!({
        "threadId": thread_id,
        "raw": raw,
    }))
}

async fn handle_codex_resume_thread(state: &AppState, params: &Value) -> Result<Value, String> {
    // Resume Discipline — see docs/architecture/opencrab-3.0-prompt-strategy.md.
    // `thread/resume` must NEVER carry prompt-shaping fields: passing one
    // silently overrides the frozen system prompt and busts the prefix cache
    // (Codex accepts it with no error/warning). The sidecar's
    // `CodexResumeThreadReq` type omits these fields by construction; this
    // runtime guard backstops any other path that could reach here.
    for forbidden in [
        "base_instructions",
        "baseInstructions",
        "instructions",
        "developer_instructions",
        "developerInstructions",
    ] {
        if params.get(forbidden).is_some() {
            return Err(format!(
                "codex_resume_thread rejected: `{forbidden}` is a prompt-shaping \
                 field forbidden on resume (see prompt-strategy.md, Resume Discipline)"
            ));
        }
    }

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
