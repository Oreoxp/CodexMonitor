// Phase 4 Step 6 — read-only Tauri command for events.jsonl.
//
// `list_team_events` exists for smoke-tests, Step 7 KANBAN-mirror
// debugging, and P7 UI surface bootstrapping. It is intentionally
// dumb: full-file scan, parse each line, return the last N. Tail-N
// optimization (seek-from-end) is a P5+ concern; Phase 4 file sizes
// are O(events-per-team) and well under a megabyte.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::State;

use crate::state::AppState;

use super::types::TeamEvent;

const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListTeamEventsResult {
    pub(crate) events: Vec<TeamEvent>,
    /// Total parseable rows in the file. `events.len()` may be `<= total`
    /// when `limit` truncates; consumers that need a tail indicator can
    /// compare the two.
    pub(crate) total: usize,
    /// Number of lines that failed to parse (silently skipped — events
    /// file is append-only so a corrupted tail does not destroy older
    /// entries). Surfaced for UI hint / log triage.
    pub(crate) parse_errors: usize,
}

async fn workspace_root(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let workspaces = state.workspaces.lock().await;
    let entry = workspaces
        .get(workspace_id)
        .ok_or_else(|| format!("workspace not found: {workspace_id}"))?;
    Ok(PathBuf::from(&entry.path))
}

fn events_path(workspace_root: &Path, team_id: &str) -> PathBuf {
    crate::paths::project_team_events_jsonl(&crate::paths::project_root(workspace_root), team_id)
}

pub(crate) fn list_team_events_at_path(
    workspace_root: &Path,
    team_id: &str,
    limit: Option<usize>,
) -> Result<ListTeamEventsResult, String> {
    let path = events_path(workspace_root, team_id);
    if !path.exists() {
        return Err(format!(
            "team events file not found: {}; was the team bootstrapped?",
            path.display()
        ));
    }
    let raw =
        std::fs::read_to_string(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut events = Vec::new();
    let mut parse_errors = 0usize;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<TeamEvent>(trimmed) {
            Ok(event) => events.push(event),
            Err(err) => {
                eprintln!("[events] {} skip malformed line: {err}", path.display());
                parse_errors += 1;
            }
        }
    }
    let total = events.len();
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if events.len() > limit {
        let drop = events.len() - limit;
        events.drain(..drop);
    }
    Ok(ListTeamEventsResult {
        events,
        total,
        parse_errors,
    })
}

#[tauri::command]
pub(crate) async fn list_team_events(
    workspace_id: String,
    team_id: String,
    limit: Option<usize>,
    state: State<'_, AppState>,
) -> Result<ListTeamEventsResult, String> {
    let root = workspace_root(&state, &workspace_id).await?;
    list_team_events_at_path(&root, &team_id, limit)
}
