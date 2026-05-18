// Phase 4 Step 7 — KANBAN.md atomic writer + multi-agent regenerate.
//
// Per-agent path: `<project_dir>/.opencrab/agents/<agent_id>/KANBAN.md`.
// Step 1's `ensure_project_layer` creates the parent directories +
// placeholder file; this module assumes they exist and DOES NOT mkdir
// (per spec — auto-creating the parent would mask a bootstrap bug).
//
// Atomic write pattern: write to `KANBAN.md.tmp` in the same directory,
// then `fs::rename` onto `KANBAN.md`. Same-directory rename is atomic on
// POSIX and `MOVEFILE_REPLACE_EXISTING` on Windows (`std::fs::rename`'s
// documented behavior). Crash between the tmp write and the rename
// leaves the previous KANBAN intact; the next regenerate cycle picks up
// the latest tasks state and rewrites cleanly.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::tasks::{store, Task, TaskStatus};

use super::render::render_kanban_markdown;

#[derive(Debug)]
pub(crate) enum KanbanError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Cannot resolve the team — typically `~/.opencrab/team.json`
    /// missing or unparseable. Caller logs + skips; transition is
    /// already committed and events.jsonl is already written.
    TeamUnavailable(String),
    /// SQLite read failure on the tasks table.
    Tasks(String),
}

impl std::fmt::Display for KanbanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KanbanError::Io { path, source } => {
                write!(f, "kanban io error at {}: {source}", path.display())
            }
            KanbanError::TeamUnavailable(msg) => {
                write!(f, "kanban team unavailable: {msg}")
            }
            KanbanError::Tasks(msg) => write!(f, "kanban tasks read failed: {msg}"),
        }
    }
}

impl std::error::Error for KanbanError {}

fn kanban_path(project_dir: &Path, agent_id: &str) -> PathBuf {
    project_dir
        .join(".opencrab")
        .join("agents")
        .join(agent_id)
        .join("KANBAN.md")
}

/// Render + atomic-write a single agent's KANBAN.md.
///
/// `tasks` is the full set (any assignee, any status) — this function
/// filters to `assignee_agent_id == agent_id` + drops `archived`
/// internally so callers don't have to repeat the filter logic.
///
/// `agent_display_name` is the human-readable name (`agent.name` from
/// team.json); the agent_id is used only to resolve the file path.
pub(crate) fn regenerate_kanban_for_agent(
    project_dir: &Path,
    agent_id: &str,
    agent_display_name: &str,
    tasks: &[Task],
) -> Result<(), KanbanError> {
    let path = kanban_path(project_dir, agent_id);
    let parent = path.parent().ok_or_else(|| KanbanError::Io {
        path: path.clone(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "kanban path has no parent",
        ),
    })?;
    // Step 1 contract: `ensure_project_layer` already created the
    // per-agent directory. If it didn't, fail loudly — auto-mkdir would
    // paper over the bootstrap bug.
    if !parent.exists() {
        return Err(KanbanError::Io {
            path: parent.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "per-agent dir missing (was ensure_project_layer skipped?)",
            ),
        });
    }

    let filtered: Vec<Task> = tasks
        .iter()
        .filter(|t| t.assignee_agent_id.as_deref() == Some(agent_id))
        .filter(|t| t.status != TaskStatus::Archived)
        .cloned()
        .collect();

    let markdown = render_kanban_markdown(agent_display_name, &filtered, Utc::now());
    write_atomic(&path, markdown.as_bytes())
}

/// One-shot regenerate of every agent's KANBAN.md for a given team.
///
/// Reads `~/.opencrab/team.json` for the roster + opens the tasks store
/// once, then loops the roster. Per-agent failures log to stderr and
/// continue — best-effort projection per the Step-7 spec ("KANBAN is
/// observation, not source of truth").
pub(crate) fn regenerate_all_kanbans(
    project_dir: &Path,
    workspace_id: &str,
    team_id: &str,
) -> Result<(), KanbanError> {
    // Roster: user-layer team.json (Step 2 canonical location).
    let team = crate::team_config::read_team_from_user_layer()
        .map_err(KanbanError::TeamUnavailable)?
        .ok_or_else(|| {
            KanbanError::TeamUnavailable("team.json not present at user layer".to_string())
        })?;
    // Sanity: caller's team_id must match the roster's team. If they
    // diverge we still rewrite (data is what it is) but log a warn —
    // the only way this fires is a multi-team mis-wiring bug.
    if team.id != team_id {
        eprintln!(
            "[kanban] team_id mismatch: caller={team_id} roster={}; \
             writing KANBAN.md against caller team_id's task rows",
            team.id
        );
    }

    // Tasks: full set for (workspace_id, team_id), unfiltered (we
    // filter per-agent below).
    let tasks = {
        let conn =
            store::open_and_init(project_dir).map_err(|e| KanbanError::Tasks(e.to_string()))?;
        store::list(&conn, workspace_id, team_id, None)
            .map_err(|e| KanbanError::Tasks(e.to_string()))?
    };

    for agent in &team.agents {
        if let Err(err) = regenerate_kanban_for_agent(project_dir, &agent.id, &agent.name, &tasks) {
            eprintln!(
                "[kanban] regenerate failed for agent={} ({}): {err}",
                agent.id, agent.name
            );
        }
    }
    Ok(())
}

/// Atomic write: `<path>.tmp` → fsync → rename.
///
/// Failure cleanup: if rename fails, we leave the `.tmp` artifact —
/// removing it could race with a concurrent regenerate (we'd delete the
/// fresh tmp). Subsequent regenerates simply overwrite the tmp via
/// `create(true) + truncate(true)` open mode.
fn write_atomic(path: &Path, content: &[u8]) -> Result<(), KanbanError> {
    let tmp = path.with_extension({
        let original = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        if original.is_empty() {
            "tmp".to_string()
        } else {
            format!("{original}.tmp")
        }
    });
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|err| KanbanError::Io {
                path: tmp.clone(),
                source: err,
            })?;
        file.write_all(content).map_err(|err| KanbanError::Io {
            path: tmp.clone(),
            source: err,
        })?;
        file.sync_all().map_err(|err| KanbanError::Io {
            path: tmp.clone(),
            source: err,
        })?;
    }
    fs::rename(&tmp, path).map_err(|err| KanbanError::Io {
        path: path.to_path_buf(),
        source: err,
    })
}
