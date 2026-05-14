// Tauri commands for the Phase 1 sidecar.
//
// Surface area:
//   start_sidecar(workspace_id)            → spawn + init (idempotent; workspace-aware)
//   sidecar_bump(workspace_id)             → pm_bump op (Spike A)
//   sidecar_read(workspace_id)             → pm_read op (Spike A/B state)
//   sidecar_pm_say(workspace_id, text)     → pm_say op (Spike B, drives Codex)
//   sidecar_ensure_agent_thread(ws_id)     → agent_ensure_thread op (pre-bootstrap)
//   stop_sidecar(workspace_id)             → kill child + drop from manager
//
// All commands look up `workspace_path` from `AppState.workspaces` by id, so
// the frontend never has to know paths.

use serde_json::Value;
use tauri::{AppHandle, State};

use crate::state::AppState;

#[tauri::command]
pub(crate) async fn start_sidecar(
    workspace_id: String,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    // Resolve workspace_path here (needs AppState); the idempotent +
    // workspace-aware spawn/init sequence — and its serialization against
    // concurrent stop/start — lives in `SidecarSessionManager::start`.
    let workspace_path = {
        let map = state.workspaces.lock().await;
        map.get(&workspace_id)
            .map(|entry| entry.path.clone())
            .ok_or_else(|| format!("unknown workspace_id: {}", workspace_id))?
    };

    state
        .sidecar_sessions
        .start(workspace_id, workspace_path, app_handle)
        .await
}

#[tauri::command]
pub(crate) async fn sidecar_bump(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let session = state
        .sidecar_sessions
        .get(&workspace_id)
        .await
        .ok_or_else(|| format!("no sidecar running for workspace `{}`", workspace_id))?;
    session.send_request("pm_bump", None).await
}

#[tauri::command]
pub(crate) async fn sidecar_read(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let session = state
        .sidecar_sessions
        .get(&workspace_id)
        .await
        .ok_or_else(|| format!("no sidecar running for workspace `{}`", workspace_id))?;
    session.send_request("pm_read", None).await
}

#[tauri::command]
pub(crate) async fn sidecar_pm_say(
    workspace_id: String,
    text: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let session = state
        .sidecar_sessions
        .get(&workspace_id)
        .await
        .ok_or_else(|| format!("no sidecar running for workspace `{}`", workspace_id))?;
    let params = serde_json::json!({ "text": text, "workspace_id": workspace_id });
    session.send_request("pm_say", Some(params)).await
}

#[tauri::command]
pub(crate) async fn sidecar_ensure_agent_thread(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let session = state
        .sidecar_sessions
        .get(&workspace_id)
        .await
        .ok_or_else(|| format!("no sidecar running for workspace `{}`", workspace_id))?;
    let params = serde_json::json!({ "workspace_id": workspace_id });
    session
        .send_request("agent_ensure_thread", Some(params))
        .await
}

#[tauri::command]
pub(crate) async fn stop_sidecar(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.sidecar_sessions.stop(&workspace_id).await
}
