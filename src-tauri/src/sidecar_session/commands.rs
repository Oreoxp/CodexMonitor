// Tauri commands for the sidecar.
//
// Surface area (Phase 2 pivot):
//   start_sidecar(workspace_id)            → spawn + init (idempotent; workspace-aware)
//   sidecar_bump(workspace_id)             → pm_bump op (Spike A)
//   sidecar_read(workspace_id)             → pm_read op (Spike A state)
//   sidecar_provision(workspace_id)        → provision_and_start_router op
//                                            (sidecar provisions any agent
//                                            missing a threadId, writes
//                                            team.json back, then asks Tauri
//                                            to start the team router)
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
pub(crate) async fn sidecar_provision(
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
        .send_request("provision_and_start_router", Some(params))
        .await
}

#[tauri::command]
pub(crate) async fn stop_sidecar(
    workspace_id: String,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    // Tear down the workspace's team router (drops permanent taps + spawned
    // consumer tasks) before killing the sidecar child.
    state.team_routers.stop(&app_handle, &workspace_id).await;
    state.sidecar_sessions.stop(&workspace_id).await
}
