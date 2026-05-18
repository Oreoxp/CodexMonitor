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

use std::path::PathBuf;

use serde_json::Value;
use tauri::{AppHandle, State};

use crate::bootstrap;
use crate::state::AppState;
use crate::team_config::{migrate_team_json_for_workspace, read_team_for_bootstrap};

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

    let workspace_path: PathBuf = {
        let map = state.workspaces.lock().await;
        map.get(&workspace_id)
            .map(|entry| PathBuf::from(&entry.path))
            .ok_or_else(|| format!("unknown workspace_id: {}", workspace_id))?
    };

    // Phase 4 Steps 1 + 2 — team.json migration + double-layer bootstrap.
    // The order is load-bearing:
    //   1. migrate `<cwd>/.opencrab/team.json` → `~/.opencrab/team.json`
    //      (one-shot, idempotent). Must run before any read so a freshly-
    //      opened P3 workspace doesn't see "no team" and prompt the user
    //      to recreate one.
    //   2. read the canonical team.json from the user layer. Absence here
    //      simply means no team is configured yet — sidecar provisioning
    //      will return early downstream; the bootstrap skips the agent-
    //      aware seeding (idempotent retry on next provision).
    //   3. seed the user-layer and project-layer skeletons.
    // Failures propagate to the caller — silent partial state is a known
    // P3 anti-pattern.
    migrate_team_json_for_workspace(&workspace_path)?;
    if let Some(team) = read_team_for_bootstrap()? {
        bootstrap::ensure_user_layer(&team.agents)
            .map_err(|err| format!("bootstrap user layer: {err}"))?;
        bootstrap::ensure_project_layer(&workspace_path, &team.id, &team.agents)
            .map_err(|err| format!("bootstrap project layer: {err}"))?;
    }

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
