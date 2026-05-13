// Tauri commands for reading and creating team.json under
// <workspace.path>/.opencrab/. Local-mode only in Phase 0a; remote-backend
// mode is a TODO (see workspaces/commands.rs for the pattern).
//
// The on-disk work is factored into `read_team_at_path` / `create_team_at_path`
// pure helpers that take a workspace root `Path` directly. The
// `#[tauri::command]` wrappers below resolve the workspace_id → path via
// AppState, then delegate. This lets the helpers be unit-tested with a
// tempdir without spinning up a Tauri State<AppState>.

use std::path::{Path, PathBuf};

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

/// Read `<workspace_root>/.opencrab/team.json` if present. Pure function —
/// touches the filesystem only via std::fs.
pub(crate) fn read_team_at_path(
    workspace_root: &Path,
) -> Result<Option<TeamConfig>, String> {
    let team_json = workspace_root.join(OPENCRAB_DIR).join(TEAM_FILE);
    if !team_json.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&team_json)
        .map_err(|err| format!("read {}: {err}", team_json.display()))?;
    let config: TeamConfig = serde_json::from_str(&raw)
        .map_err(|err| format!("parse {}: {err}", team_json.display()))?;
    Ok(Some(config))
}

/// Instantiate `template_id` and write it to `<workspace_root>/.opencrab/team.json`.
/// Refuses to overwrite an existing file. Pure function — touches the
/// filesystem only via std::fs.
pub(crate) fn create_team_at_path(
    workspace_root: &Path,
    template_id: &str,
) -> Result<TeamConfig, String> {
    let opencrab_dir = workspace_root.join(OPENCRAB_DIR);
    std::fs::create_dir_all(&opencrab_dir)
        .map_err(|err| format!("create {}: {err}", opencrab_dir.display()))?;

    let team_json = opencrab_dir.join(TEAM_FILE);
    if team_json.exists() {
        return Err(format!("team already exists at {}", team_json.display()));
    }

    let config = instantiate_template(template_id)?;
    let serialized = serde_json::to_string_pretty(&config)
        .map_err(|err| format!("serialize team config: {err}"))?;
    std::fs::write(&team_json, serialized)
        .map_err(|err| format!("write {}: {err}", team_json.display()))?;

    Ok(config)
}

#[tauri::command]
pub(crate) async fn read_team_config(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Option<TeamConfig>, String> {
    // TODO(remote): branch on remote_backend::is_remote_mode and proxy via RPC.
    let root = workspace_root(&state, &workspace_id).await?;
    read_team_at_path(&root)
}

#[tauri::command]
pub(crate) async fn create_team_from_template(
    workspace_id: String,
    template_id: String,
    state: State<'_, AppState>,
) -> Result<TeamConfig, String> {
    // TODO(remote): branch on remote_backend::is_remote_mode and proxy via RPC.
    let root = workspace_root(&state, &workspace_id).await?;
    create_team_at_path(&root, &template_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_create_team_from_template_writes_file() {
        let dir = tempdir().unwrap();
        // .opencrab does not exist yet.
        assert!(!dir.path().join(OPENCRAB_DIR).exists());

        let config = create_team_at_path(dir.path(), "solo_pm").unwrap();
        assert!(config.id.starts_with("team_"));

        let team_json = dir.path().join(OPENCRAB_DIR).join(TEAM_FILE);
        assert!(team_json.exists());

        let raw = std::fs::read_to_string(&team_json).unwrap();
        let parsed: TeamConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.id, config.id);
        assert_eq!(parsed.template_id, "solo_pm");
    }

    #[test]
    fn test_create_team_from_template_refuses_overwrite() {
        let dir = tempdir().unwrap();
        create_team_at_path(dir.path(), "solo_pm").unwrap();

        let err = create_team_at_path(dir.path(), "solo_pm").unwrap_err();
        assert!(err.contains("team already exists"), "got: {err}");

        // The pre-existing team.json must NOT have been clobbered.
        let raw = std::fs::read_to_string(
            dir.path().join(OPENCRAB_DIR).join(TEAM_FILE),
        )
        .unwrap();
        let parsed: TeamConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.template_id, "solo_pm");
    }

    #[test]
    fn test_read_team_config_missing() {
        let dir = tempdir().unwrap();
        let result = read_team_at_path(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_team_config_existing() {
        let dir = tempdir().unwrap();
        let created = create_team_at_path(dir.path(), "pm_plus_one_dev").unwrap();
        let loaded = read_team_at_path(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.id, created.id);
        assert_eq!(loaded.template_id, "pm_plus_one_dev");
        assert_eq!(loaded.agents.len(), 2);
    }
}
