use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::codex::WorkspaceSession;
use crate::state::AppState;

const DEFAULT_TURN_TIMEOUT_SECS: u64 = 90;

#[derive(Debug, Deserialize)]
pub(crate) struct CodexRunTaskRequest {
    pub(crate) thread_id: String,
    pub(crate) workspace_id: String,
    pub(crate) node: String,
    #[serde(default)]
    pub(crate) input: CodexRunTaskInput,
    #[serde(default)]
    pub(crate) codex_thread_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CodexRunTaskInput {
    #[serde(default)]
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) phase: String,
    #[serde(default)]
    pub(crate) allow_file_write: bool,
    #[serde(default)]
    pub(crate) expected_output: String,
    #[serde(default)]
    pub(crate) codex_thread_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CodexRunTaskResult {
    pub(crate) codex_thread_id: String,
    pub(crate) codex_thread_created: bool,
    pub(crate) summary: String,
    pub(crate) raw_text: String,
    pub(crate) phase: String,
}

pub(crate) async fn run_codex_task(
    state: &AppState,
    req: CodexRunTaskRequest,
) -> Result<CodexRunTaskResult, String> {
    if req.workspace_id.trim().is_empty() {
        return Err("codex_run_task requires non-empty workspace_id".to_string());
    }
    if req.input.prompt.trim().is_empty() {
        return Err("codex_run_task requires non-empty prompt".to_string());
    }
    if req.input.allow_file_write {
        return Err(
            "codex_run_task currently rejects allow_file_write=true (read-only nodes only)"
                .to_string(),
        );
    }

    let workspace_path = {
        let workspaces = state.workspaces.lock().await;
        workspaces
            .get(&req.workspace_id)
            .ok_or_else(|| format!("workspace not found: {}", req.workspace_id))?
            .path
            .clone()
    };
    let session: std::sync::Arc<WorkspaceSession> = {
        let sessions = state.sessions.lock().await;
        sessions
            .get(&req.workspace_id)
            .ok_or_else(|| {
                format!(
                    "workspace not connected: {} (connect_workspace first)",
                    req.workspace_id
                )
            })?
            .clone()
    };

    let provided_thread_id = req
        .codex_thread_id
        .clone()
        .or_else(|| req.input.codex_thread_id.clone())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let (codex_thread_id, codex_thread_created) = match provided_thread_id {
        Some(id) => (id, false),
        None => {
            let thread_params = json!({
                "cwd": workspace_path.clone(),
                "approvalPolicy": "never",
            });
            let thread_response = session
                .send_request_for_workspace(&req.workspace_id, "thread/start", thread_params)
                .await
                .map_err(|err| format!("thread/start failed: {err}"))?;
            let new_id = extract_thread_id(&thread_response).ok_or_else(|| {
                format!(
                    "thread/start returned no threadId: {}",
                    thread_response
                )
            })?;
            session.routing.mark_hidden_thread(&new_id).await;
            (new_id, true)
        }
    };

    // Hide this thread from normal-mode UI for its lifetime.
    session.routing.mark_hidden_thread(&codex_thread_id).await;

    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    session
        .routing
        .register_background_callback(codex_thread_id.clone(), tx)
        .await;

    let turn_params = json!({
        "threadId": codex_thread_id,
        "input": [{ "type": "text", "text": req.input.prompt }],
        "cwd": workspace_path,
        "approvalPolicy": "never",
        "sandboxPolicy": { "type": "readOnly" },
    });
    let turn_result = session
        .send_request_for_workspace(&req.workspace_id, "turn/start", turn_params)
        .await;
    if let Err(err) = turn_result {
        session
            .routing
            .take_background_callback(&codex_thread_id)
            .await;
        return Err(format!("turn/start failed: {err}"));
    }

    let mut response_text = String::new();
    let collect = timeout(Duration::from_secs(DEFAULT_TURN_TIMEOUT_SECS), async {
        loop {
            let Some(event) = rx.recv().await else {
                return Err("background response stream closed".to_string());
            };
            let method = event.get("method").and_then(Value::as_str).unwrap_or("");
            match method {
                "item/agentMessage/delta" => {
                    if let Some(delta) = event
                        .get("params")
                        .and_then(|p| p.get("delta"))
                        .and_then(Value::as_str)
                    {
                        response_text.push_str(delta);
                    }
                }
                "item/agentMessage" => {
                    if response_text.is_empty() {
                        if let Some(text) = event
                            .get("params")
                            .and_then(|p| p.get("item"))
                            .and_then(|i| i.get("text"))
                            .and_then(Value::as_str)
                        {
                            response_text.push_str(text);
                        }
                    }
                }
                "turn/completed" => break,
                "turn/error" => {
                    let msg = event
                        .get("params")
                        .and_then(|p| p.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("turn/error")
                        .to_string();
                    return Err(msg);
                }
                _ => {}
            }
        }
        Ok::<(), String>(())
    })
    .await;

    session
        .routing
        .take_background_callback(&codex_thread_id)
        .await;

    match collect {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            return Err(format!(
                "timed out after {DEFAULT_TURN_TIMEOUT_SECS}s waiting for codex_run_task response"
            ));
        }
    }

    let trimmed = response_text.trim().to_string();
    let summary = if trimmed.is_empty() {
        return Err("codex_run_task produced empty response".to_string());
    } else {
        trimmed.clone()
    };

    Ok(CodexRunTaskResult {
        codex_thread_id,
        codex_thread_created,
        summary,
        raw_text: trimmed,
        phase: req.input.phase.clone(),
    })
}

fn extract_thread_id(response: &Value) -> Option<String> {
    let candidates = [
        response.pointer("/result/threadId"),
        response.pointer("/result/thread/id"),
        response.pointer("/threadId"),
        response.pointer("/thread/id"),
    ];
    for candidate in candidates {
        if let Some(value) = candidate.and_then(Value::as_str) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}
