// Tauri commands for the Phase 1 sidecar.
//
// Surface area:
//   start_sidecar(workspace_id)        → spawn + init
//   sidecar_bump(workspace_id)         → pm_bump op (Spike A)
//   sidecar_read(workspace_id)         → pm_read op (Spike A/B state)
//   sidecar_pm_say(workspace_id, text) → pm_say op (Spike B, drives Codex)
//   stop_sidecar(workspace_id)         → kill child + drop from manager
//
// All commands look up `workspace_path` from `AppState.workspaces` by id, so
// the frontend never has to know paths.

use serde_json::Value;
use tauri::{AppHandle, State};

use super::session::SidecarSession;
use crate::state::AppState;

#[tauri::command]
pub(crate) async fn start_sidecar(
    workspace_id: String,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    if state.sidecar_sessions.has(&workspace_id).await {
        return Err(format!(
            "sidecar already running for workspace `{}`",
            workspace_id
        ));
    }

    let workspace_path = {
        let map = state.workspaces.lock().await;
        map.get(&workspace_id)
            .map(|entry| entry.path.clone())
            .ok_or_else(|| format!("unknown workspace_id: {}", workspace_id))?
    };

    let session =
        SidecarSession::spawn(workspace_id.clone(), workspace_path.clone(), app_handle).await?;

    // Run `init` immediately so the sidecar binds to this workspace before any
    // other op can race. If init fails, kill the child so we don't leave an
    // orphaned, half-initialized sidecar around.
    let init_params = serde_json::json!({ "workspace_path": workspace_path });
    match session.send_request("init", Some(init_params)).await {
        Ok(_) => {
            state.sidecar_sessions.insert(session).await;
            Ok(())
        }
        Err(err) => {
            session.kill().await;
            Err(format!("sidecar init failed: {}", err))
        }
    }
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
pub(crate) async fn stop_sidecar(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let Some(session) = state.sidecar_sessions.remove(&workspace_id).await else {
        return Err(format!(
            "no sidecar running for workspace `{}`",
            workspace_id
        ));
    };
    session.kill().await;
    Ok(())
}
