// Tauri commands for the Phase 3 Step 1 task store.
//
// Pattern mirrors `team_config/commands.rs`:
//   - Pure helpers (`*_at_path`) take the workspace root `&Path` directly so
//     unit tests can drive them with a tempdir.
//   - `#[tauri::command]` wrappers resolve `workspace_id` → workspace path
//     via `AppState`, then delegate.
//
// `create_task_dev_only` was the Step 1 dev-console seeder. Step 2's
// `<propose_plan>` parser + `insert_proposed_batch` made it the only
// seed path the production frontend ever uses; Phase 3 closeout (Step 4
// §11.7) retires the Tauri command. The `create_task_dev_only_at_path`
// helper survives under `#[cfg(test)]` so unit tests + the WAL smoke
// script can still seed rows without going through the parser.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::State;

use crate::state::AppState;

use super::state_machine::TaskError;
use super::store::{self, TaskPatch};
use super::types::{Task, TaskStatus};

#[cfg(test)]
use serde::Deserialize;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateTaskDevOnlyInput {
    pub(crate) workspace_id: String,
    pub(crate) team_id: String,
    pub(crate) proposed_by_agent_id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) assignee_agent_id: Option<String>,
}

fn map_err(err: TaskError) -> String {
    err.to_string()
}

async fn workspace_root(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let workspaces = state.workspaces.lock().await;
    let entry = workspaces
        .get(workspace_id)
        .ok_or_else(|| format!("workspace not found: {workspace_id}"))?;
    Ok(PathBuf::from(&entry.path))
}

// ---------------------------------------------------------------------------
// Pure helpers (tempdir-driven from tests)
// ---------------------------------------------------------------------------

pub(crate) fn list_tasks_at_path(
    workspace_root: &Path,
    workspace_id: &str,
    team_id: &str,
    status_filter: Option<&[TaskStatus]>,
) -> Result<Vec<Task>, TaskError> {
    let conn = store::open_and_init(workspace_root)?;
    store::list(&conn, workspace_id, team_id, status_filter)
}

pub(crate) fn get_task_at_path(
    workspace_root: &Path,
    task_id: &str,
) -> Result<Option<Task>, TaskError> {
    let conn = store::open_and_init(workspace_root)?;
    store::get(&conn, task_id)
}

pub(crate) fn transition_task_at_path(
    workspace_root: &Path,
    task_id: &str,
    new_status: TaskStatus,
    actor: &str,
) -> Result<Task, TaskError> {
    let conn = store::open_and_init(workspace_root)?;
    store::transition(&conn, task_id, new_status, actor)
}

/// Reject path (Phase 3 bug fix): same as `transition_task_at_path` but
/// persists `feedback` to the DB column so the plan-completion merged
/// message can rebuild itself across restarts.
pub(crate) fn transition_task_with_feedback_at_path(
    workspace_root: &Path,
    task_id: &str,
    new_status: TaskStatus,
    actor: &str,
    feedback: Option<&str>,
) -> Result<Task, TaskError> {
    let conn = store::open_and_init(workspace_root)?;
    store::transition_with_feedback(&conn, task_id, new_status, actor, feedback)
}

/// Plan-level helpers exposed for the router's wait-for-plan-completion
/// logic. Both run inside `spawn_blocking` from `notify_*`.
pub(crate) fn list_tasks_by_plan_at_path(
    workspace_root: &Path,
    plan_id: &str,
) -> Result<Vec<Task>, TaskError> {
    let conn = store::open_and_init(workspace_root)?;
    store::list_by_plan(&conn, plan_id)
}

pub(crate) fn plan_still_has_proposed_at_path(
    workspace_root: &Path,
    plan_id: &str,
) -> Result<bool, TaskError> {
    let conn = store::open_and_init(workspace_root)?;
    store::plan_still_has_proposed(&conn, plan_id)
}

/// Test-only seed: insert a single `proposed` row directly, skipping the
/// PM-protocol parser. Production seed path is
/// `store::insert_proposed_batch_at_path`; this helper exists for unit
/// tests (which need a one-liner) and for the WAL coexistence smoke
/// script (which exercises the rusqlite path WITHOUT a sidecar in the
/// loop). Compiled out of release / non-test builds.
#[cfg(test)]
pub(crate) fn create_task_dev_only_at_path(
    workspace_root: &Path,
    input: CreateTaskDevOnlyInput,
) -> Result<Task, TaskError> {
    let conn = store::open_and_init(workspace_root)?;
    let new = store::NewTask {
        id: Uuid::new_v4().to_string(),
        workspace_id: input.workspace_id,
        team_id: input.team_id,
        assignee_agent_id: input.assignee_agent_id,
        proposed_by_agent_id: input.proposed_by_agent_id,
        title: input.title,
        body: input.body,
    };
    store::insert_proposed(&conn, new)
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) async fn list_tasks(
    workspace_id: String,
    team_id: String,
    status_filter: Option<Vec<TaskStatus>>,
    state: State<'_, AppState>,
) -> Result<Vec<Task>, String> {
    let root = workspace_root(&state, &workspace_id).await?;
    let filter = status_filter.as_deref();
    list_tasks_at_path(&root, &workspace_id, &team_id, filter).map_err(map_err)
}

#[tauri::command]
pub(crate) async fn get_task(
    workspace_id: String,
    task_id: String,
    state: State<'_, AppState>,
) -> Result<Option<Task>, String> {
    let root = workspace_root(&state, &workspace_id).await?;
    get_task_at_path(&root, &task_id).map_err(map_err)
}

#[tauri::command]
pub(crate) async fn transition_task(
    workspace_id: String,
    task_id: String,
    new_status: TaskStatus,
    actor: String,
    state: State<'_, AppState>,
) -> Result<Task, String> {
    let root = workspace_root(&state, &workspace_id).await?;
    transition_task_at_path(&root, &task_id, new_status, &actor).map_err(map_err)
}

// Retired Phase 3 closeout (Step 4 §11.7): the Tauri command form of
// `create_task_dev_only` is gone. The frontend dev-console seed path it
// used to serve has not been needed since Step 2 wired the real
// `<propose_plan>` parser. The `_at_path` helper above survives under
// `#[cfg(test)]` for unit tests + the WAL smoke script.

// ---------------------------------------------------------------------------
// Step 3: approve_task / reject_task
//
// Each command is a thin orchestrator over three primitives:
//   1. State-machine transition at the DB layer (`store::transition` via
//      `transition_task_at_path`). Source of truth for status; an illegal
//      transition (e.g. approving an already-approved task) bubbles up
//      as `Err` here.
//   2. Latch mutation + PM continuation reply via `TeamRouters::notify_*`.
//      Both are router-owned state; missing router => silently false.
//   3. `ApprovalResult { task, pm_notified }`. The Step-4 modal pins
//      against this shape.
//
// The state-machine transition is the gate: we do NOT pre-check whether
// the task is in `pending_approvals` before transitioning. If the user
// has already directly flipped the status via `transition_task`, our
// `transition` call here will return `IllegalTransition` — same as
// any other illegal flip. The router's pending_approvals map stays in
// sync because `transition` is the single writer of `status` (Step 1
// invariant); the only way the map can drift is through a router
// restart, and hydration re-reads `tasks.status='proposed'` to rebuild.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApprovalResult {
    pub(crate) task: Task,
    /// Semantics changed by the Phase 3 bug fix (2026-05-17), plan-level
    /// approval batching:
    ///
    /// **Plan-level rows** (task carries a `plan_id`, the common case):
    /// `true` iff the resolve is durable AND either (a) the plan is
    /// still under review — no message needed for this call, or (b) the
    /// plan completed and the merged message dispatch succeeded.
    /// `false` ONLY when the FINAL approve/reject of a plan triggers a
    /// merged-message dispatch that fails (target Codex thread busy,
    /// upstream Codex error, etc.). Intermediate resolves never set
    /// `false` — they always succeed in the "no message yet" sense.
    ///
    /// **Legacy NULL plan_id rows** (pre-fix rows, vanishing):
    /// retains the original per-task semantics: `false` covers (1) no
    /// live router OR (2) per-task system reply dispatch failed. Step
    /// 4 modal's warning chip uses this signal as before.
    ///
    /// **Why the field name didn't change**: a rename would force the
    /// frontend modal + tests to update in lockstep; the new semantics
    /// is a strict refinement of the old (any case that used to be
    /// `true` still is). Documented here so future readers don't get
    /// confused by intermediate-resolve `true` returns.
    pub(crate) pm_notified: bool,
}

#[tauri::command]
pub(crate) async fn approve_task(
    workspace_id: String,
    task_id: String,
    actor: String,
    state: State<'_, AppState>,
) -> Result<ApprovalResult, String> {
    let root = workspace_root(&state, &workspace_id).await?;
    let task =
        transition_task_at_path(&root, &task_id, TaskStatus::Ready, &actor).map_err(map_err)?;
    // Clone the Arc<TeamRouters> so we can release the State<AppState>
    // borrow before awaiting the router call (which itself takes locks on
    // pending_approvals and may dispatch a system reply that runs other
    // async work).
    let routers = state.team_routers.clone();
    let pm_notified = routers.notify_approved(&workspace_id, &task).await;
    Ok(ApprovalResult { task, pm_notified })
}

#[tauri::command]
pub(crate) async fn reject_task(
    workspace_id: String,
    task_id: String,
    feedback: Option<String>,
    actor: String,
    state: State<'_, AppState>,
) -> Result<ApprovalResult, String> {
    let root = workspace_root(&state, &workspace_id).await?;
    // Phase 3 bug fix: persist feedback to the `feedback` column so the
    // plan-completion merged message can rebuild from the DB across
    // restarts. Empty / whitespace-only feedback writes NULL — the
    // rebuilt message renders "(no feedback provided)" for those rows.
    let feedback_clean = feedback.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let task = transition_task_with_feedback_at_path(
        &root,
        &task_id,
        TaskStatus::Archived,
        &actor,
        feedback_clean,
    )
    .map_err(map_err)?;
    let routers = state.team_routers.clone();
    let pm_notified = routers
        .notify_rejected(&workspace_id, &task, feedback.as_deref())
        .await;
    Ok(ApprovalResult { task, pm_notified })
}

// ---------------------------------------------------------------------------
// Step 4: update_task — content edit on a `proposed` row (modal edit path).
//
// Wire shape mirrors `TaskPatch` in store.rs; `assignee` is a tagged enum
// (Keep / Set { value } / Clear) so the modal's three intents stay
// distinct over JSON. Refuses to edit a row that is no longer `proposed`
// (returns `IllegalTransition { from: <current>, to: Proposed }`) — same
// typed-error channel the state machine uses, so the modal's error
// handling stays unified.
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) async fn update_task(
    workspace_id: String,
    task_id: String,
    patch: TaskPatch,
    actor: String,
    state: State<'_, AppState>,
) -> Result<Task, String> {
    let root = workspace_root(&state, &workspace_id).await?;
    store::update_content_at_path(&root, &task_id, patch, &actor).map_err(map_err)
}
