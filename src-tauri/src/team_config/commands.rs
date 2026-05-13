// Tauri commands for reading and creating team.json under
// <workspace.path>/.opencrab/. Local-mode only in Phase 0a; remote-backend
// mode is a TODO (see workspaces/commands.rs for the pattern).

use std::path::PathBuf;

use serde::Serialize;
use tauri::State;

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

const OPENCRAB_DIR: &str = ".opencrab";
const TEAM_FILE: &str = "team.json";

async fn workspace_root(
    state: &AppState,
    workspace_id: &str,
) -> Result<PathBuf, String> {
    let workspaces = state.workspaces.lock().await;
    let entry = workspaces
        .get(workspace_id)
        .ok_or_else(|| format!("workspace not found: {workspace_id}"))?;
    Ok(PathBuf::from(&entry.path))
}

#[tauri::command]
pub(crate) async fn read_team_config(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Option<TeamConfig>, String> {
    // TODO(remote): branch on remote_backend::is_remote_mode and proxy via RPC.
    let root = workspace_root(&state, &workspace_id).await?;
    let team_json = root.join(OPENCRAB_DIR).join(TEAM_FILE);
    if !team_json.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&team_json)
        .map_err(|err| format!("read {}: {err}", team_json.display()))?;
    let config: TeamConfig = serde_json::from_str(&raw)
        .map_err(|err| format!("parse {}: {err}", team_json.display()))?;
    Ok(Some(config))
}

#[tauri::command]
pub(crate) async fn create_team_from_template(
    workspace_id: String,
    template_id: String,
    state: State<'_, AppState>,
) -> Result<TeamConfig, String> {
    // TODO(remote): branch on remote_backend::is_remote_mode and proxy via RPC.
    let root = workspace_root(&state, &workspace_id).await?;
    let opencrab_dir = root.join(OPENCRAB_DIR);
    std::fs::create_dir_all(&opencrab_dir)
        .map_err(|err| format!("create {}: {err}", opencrab_dir.display()))?;

    let team_json = opencrab_dir.join(TEAM_FILE);
    if team_json.exists() {
        return Err(format!("team already exists at {}", team_json.display()));
    }

    let config = instantiate_template(&template_id)?;
    let serialized = serde_json::to_string_pretty(&config)
        .map_err(|err| format!("serialize team config: {err}"))?;
    std::fs::write(&team_json, serialized)
        .map_err(|err| format!("write {}: {err}", team_json.display()))?;

    Ok(config)
}
