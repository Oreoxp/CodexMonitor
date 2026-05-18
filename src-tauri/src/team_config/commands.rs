// Tauri commands for reading and creating team.json.
//
// Phase 4 Step 2 (2026-05-18): team.json moved from the project layer
// (`<cwd>/.opencrab/team.json`) to the user layer (`~/.opencrab/team.json`)
// as the single, machine-wide location. Migration is performed by
// `bootstrap::migrate_team_json` as a precondition of every command in
// this module — first call moves the project-layer copy to the user layer
// and renames the source to `team.json.legacy`; subsequent calls are
// no-ops via `UsedExistingHomeFile`.
//
// `workspace_id` is **retained** in both Tauri command signatures for
// frontend / wire compatibility (the frontend keeps passing it) but is
// only used to resolve `cwd` for the migration check; once migration is
// settled the file lives at the user layer regardless of workspace.

use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;
use tauri::State;

use crate::bootstrap::{migrate_team_json, team_json_path, write_team_json_atomic, BootstrapError};
use crate::state::AppState;

use super::templates::{instantiate_template, TEMPLATE_METADATA};
use super::types::TeamConfig;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateInfo {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) description: String,
}

#[tauri::command]
pub(crate) async fn list_templates() -> Result<Vec<TemplateInfo>, String> {
    Ok(TEMPLATE_METADATA
        .iter()
        .map(|(id, display_name, description)| TemplateInfo {
            id: (*id).to_string(),
            display_name: (*display_name).to_string(),
            description: (*description).to_string(),
        })
        .collect())
}

async fn workspace_root(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let workspaces = state.workspaces.lock().await;
    let entry = workspaces
        .get(workspace_id)
        .ok_or_else(|| format!("workspace not found: {workspace_id}"))?;
    Ok(PathBuf::from(&entry.path))
}

/// Read `~/.opencrab/team.json` if present (Step 2: user layer is the
/// authoritative location). Pure function — touches the filesystem only
/// via std::fs.
///
/// Callers that own a workspace context should call
/// [`migrate_team_json`] first; the read itself is layer-agnostic.
pub(crate) fn read_team_from_user_layer() -> Result<Option<TeamConfig>, String> {
    let team_json = team_json_path().map_err(map_bootstrap_err)?;
    if !team_json.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&team_json)
        .map_err(|err| format!("read {}: {err}", team_json.display()))?;
    let config: TeamConfig = serde_json::from_str(&raw)
        .map_err(|err| format!("parse {}: {err}", team_json.display()))?;
    Ok(Some(config))
}

/// Instantiate `template_id` and write it to `~/.opencrab/team.json`
/// (Step 2: user layer). Atomic write via `<path>.tmp` + fsync + rename.
/// Refuses to overwrite an existing team — frontend must surface a clear
/// "team already exists" error so the user can choose to load the
/// existing roster vs. discard it manually.
pub(crate) fn create_team_at_user_layer(template_id: &str) -> Result<TeamConfig, String> {
    let team_json = team_json_path().map_err(map_bootstrap_err)?;
    if team_json.exists() {
        return Err(format!("team already exists at {}", team_json.display()));
    }

    let config = instantiate_template(template_id)?;
    let serialized = serde_json::to_string_pretty(&config)
        .map_err(|err| format!("serialize team config: {err}"))?;
    write_team_json_atomic(serialized.as_bytes()).map_err(map_bootstrap_err)?;

    Ok(config)
}

/// Run the Step-2 migration as a precondition. Calls with a fresh
/// workspace cwd; idempotent across repeated invocations within the same
/// process.
fn migrate_for_workspace(workspace_root: &Path) -> Result<(), String> {
    migrate_team_json(workspace_root)
        .map(|_| ())
        .map_err(map_bootstrap_err)
}

fn map_bootstrap_err(err: BootstrapError) -> String {
    format!("{err}")
}

#[tauri::command]
pub(crate) async fn read_team_config(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Option<TeamConfig>, String> {
    // TODO(remote): branch on remote_backend::is_remote_mode and proxy via RPC.
    let root = workspace_root(&state, &workspace_id).await?;
    migrate_for_workspace(&root)?;
    read_team_from_user_layer()
}

#[tauri::command]
pub(crate) async fn create_team_from_template(
    workspace_id: String,
    template_id: String,
    state: State<'_, AppState>,
) -> Result<TeamConfig, String> {
    // TODO(remote): branch on remote_backend::is_remote_mode and proxy via RPC.
    let root = workspace_root(&state, &workspace_id).await?;
    migrate_for_workspace(&root)?;
    create_team_at_user_layer(&template_id)
}

// Internal helper: the Step-1 sidecar_provision wiring uses this to read
// the (post-migration) team config without rebuilding the path. Re-exported
// from the module so call sites stay short.
pub(crate) use read_team_from_user_layer as read_team_for_bootstrap;

// Re-export for any caller that still wants the workspace-scoped migrate
// helper without going through the Tauri-command surface (e.g. the new
// sidecar_provision wiring in Step 2).
pub(crate) fn migrate_team_json_for_workspace(workspace_root: &Path) -> Result<(), String> {
    migrate_for_workspace(workspace_root)
}

// Path-driven I/O test coverage lives in `bootstrap::tests`, which can
// inject a tempdir for HOME. The Tauri-command wrappers above are
// `#[tauri::command]` async fns that resolve workspace_id from
// `AppState`, so they're exercised by integration tests / smoke runs
// rather than unit tests in this module.
