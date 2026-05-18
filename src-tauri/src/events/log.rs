// Phase 4 Step 6 — EventLog writer + per-(workspace, team) registry.
//
// `EventLog` holds an open `File` against `<cwd>/.opencrab/teams/<team_id>
// /events.jsonl`, wrapped in `Mutex<File>` so concurrent Tokio tasks in
// this process cannot interleave bytes within one JSON line. Cross-
// process serialization is out of scope (Phase 4 single-writer
// assumption — only the Rust host emits).
//
// Lifecycle:
//   * `EventLog::open` opens the file in append mode. Parent dirs MUST
//     already exist (Step 1's `ensure_project_layer` is responsible);
//     open does NOT mkdir-recursive — failing loudly here surfaces the
//     "Step 1 didn't run" bug instead of papering over it.
//   * `get_or_create_event_log` is the spec-mandated AppState helper:
//     one instance per `(workspace_id, team_id)`, reused across Tauri
//     command calls.
//   * No explicit close — `Drop` on `File` is enough.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::state::AppState;

use super::types::{TeamEvent, TeamEventBody};

#[derive(Debug)]
pub(crate) enum EventLogError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Serde(serde_json::Error),
    PoisonedLock,
}

impl std::fmt::Display for EventLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventLogError::Io { path, source } => {
                write!(f, "events.jsonl io error at {}: {source}", path.display())
            }
            EventLogError::Serde(e) => write!(f, "events.jsonl serialize error: {e}"),
            EventLogError::PoisonedLock => f.write_str("events.jsonl mutex poisoned"),
        }
    }
}

impl std::error::Error for EventLogError {}

#[derive(Debug)]
pub(crate) struct EventLog {
    path: PathBuf,
    team_id: String,
    file: Mutex<File>,
}

impl EventLog {
    /// Open `<project_dir>/.opencrab/teams/<team_id>/events.jsonl` in
    /// append mode. Step 1's `ensure_project_layer` is responsible for
    /// creating the parent dirs + a 0-byte placeholder file — open here
    /// will error if the path doesn't exist. (We choose `create(false)`
    /// over `create(true)` to surface "Step 1 wiring is wrong" as a
    /// loud failure rather than silently creating an orphan file under
    /// a path the rest of the system doesn't expect.)
    pub(crate) fn open(project_dir: &Path, team_id: &str) -> Result<Self, EventLogError> {
        let path = project_dir
            .join(".opencrab")
            .join("teams")
            .join(team_id)
            .join("events.jsonl");
        let file = OpenOptions::new()
            .append(true)
            .create(false)
            .open(&path)
            .map_err(|err| EventLogError::Io {
                path: path.clone(),
                source: err,
            })?;
        Ok(Self {
            path,
            team_id: team_id.to_string(),
            file: Mutex::new(file),
        })
    }

    /// Append one event. Builds the envelope (schema version + uuid +
    /// timestamp) from the team-id this log was opened against, then
    /// writes a single line under the in-process mutex.
    ///
    /// The mutex is held for the duration of the `writeln!` + `flush`
    /// so a concurrent Tokio task can't slip a partial write into the
    /// middle of someone else's line. The file is opened with
    /// `O_APPEND`, which positions each `write(2)` at EOF atomically;
    /// the mutex is the in-process belt-and-suspenders on top.
    pub(crate) fn emit(
        &self,
        task_id: Option<&str>,
        body: TeamEventBody,
    ) -> Result<(), EventLogError> {
        let event = TeamEvent::new(&self.team_id, task_id, body);
        let line = serde_json::to_string(&event).map_err(EventLogError::Serde)?;
        let mut file = self.file.lock().map_err(|_| EventLogError::PoisonedLock)?;
        writeln!(file, "{line}").map_err(|err| EventLogError::Io {
            path: self.path.clone(),
            source: err,
        })?;
        file.flush().map_err(|err| EventLogError::Io {
            path: self.path.clone(),
            source: err,
        })?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// AppState registry — one EventLog per (workspace_id, team_id)
// ---------------------------------------------------------------------------
//
// The registry itself lives directly on `AppState` as
// `std::sync::Mutex<HashMap<(String, String), Arc<EventLog>>>`; this module
// provides the lookup-or-create helper below.

/// Look up the singleton `EventLog` for `(workspace_id, team_id)`, or
/// open a new one and cache it. Both Tauri commands (`approve_task` /
/// `reject_task` / `transition_task`) and the team_router propose_plan
/// site funnel through here so every emission shares the same in-process
/// file handle + mutex.
///
/// `workspace_root` is the workspace's filesystem root (not the
/// `.opencrab/` subdir) — `EventLog::open` joins `.opencrab/teams/<id>/`
/// internally.
pub(crate) fn get_or_create_event_log(
    state: &AppState,
    workspace_root: &Path,
    workspace_id: &str,
    team_id: &str,
) -> Result<Arc<EventLog>, EventLogError> {
    let key = (workspace_id.to_string(), team_id.to_string());
    let mut map = state
        .event_logs
        .lock()
        .map_err(|_| EventLogError::PoisonedLock)?;
    if let Some(existing) = map.get(&key) {
        return Ok(existing.clone());
    }
    let log = Arc::new(EventLog::open(workspace_root, team_id)?);
    map.insert(key, log.clone());
    Ok(log)
}
