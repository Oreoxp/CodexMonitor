// `tasks` sqlite store — sibling table inside `<workspace>/.opencrab/state.sqlite`.
//
// Coexistence with LangGraph's SqliteSaver:
//   - Both processes open the same file. SQLite's WAL mode lets writers
//     and readers from different processes share without blocking each
//     other (modulo a short COMMIT lock). We force WAL on every open here;
//     it persists in the file's header, so once enabled it stays even if
//     the sidecar's SqliteSaver opens the file afterwards.
//   - SqliteSaver owns table names prefixed `checkpoint*` / `writes`. Our
//     `tasks` table name does not collide.
//   - Both writers use `IF NOT EXISTS` so init order does not matter.
//
// Migration discipline:
//   - `init_schema` is idempotent: running it twice is a no-op for both the
//     CREATE TABLE and the CREATE INDEX statements. Tested by
//     `tests::migration_idempotent`.
//   - No DROP / ALTER paths in Phase 3 Step 1; future schema changes will
//     gate on a `task_schema_version` row.
//
// State-machine entry point:
//   - `transition` is the ONLY function in this module that mutates the
//     `status` column. `insert` writes the initial status (`proposed` is
//     the only entry point Step 1 needs; future PM-protocol code can
//     widen this if it needs to seed a different status, in which case
//     the same guard applies). Tests prove single-entry-point by mutating
//     `status` only through this function across the legal/illegal matrix.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;

use super::state_machine::{effects_of, validate_transition, TaskError};
use super::types::{Task, TaskStatus};

const CREATE_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id                   TEXT PRIMARY KEY,
    workspace_id         TEXT NOT NULL,
    team_id              TEXT NOT NULL,
    assignee_agent_id    TEXT,
    proposed_by_agent_id TEXT NOT NULL,
    status               TEXT NOT NULL,
    title                TEXT NOT NULL,
    body                 TEXT NOT NULL DEFAULT '',
    approved_at          TEXT,
    completed_at         TEXT,
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    plan_id              TEXT,
    feedback             TEXT
)";

const CREATE_INDEX_WORKSPACE_TEAM_STATUS_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_tasks_workspace_team_status
    ON tasks (workspace_id, team_id, status)";

const CREATE_INDEX_TEAM_UPDATED_AT_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_tasks_team_updated_at
    ON tasks (team_id, updated_at DESC)";

const CREATE_INDEX_PLAN_ID_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_tasks_plan_id
    ON tasks (plan_id) WHERE plan_id IS NOT NULL";

// Plan-level approval batching (Phase 3 bug fix, 2026-05-17): `plan_id`
// groups every task that came out of one `<propose_plan>` block; `feedback`
// captures the user's rejection feedback so the merged plan-completion
// system message can rebuild itself from the DB (durable across
// restarts mid-review). Legacy rows (pre-fix) carry NULL `plan_id`; the
// notify_* path special-cases NULL to mean "fall back to per-task system
// messages" — strict backward compatibility, no migration needed for
// existing workspaces.

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// `<workspace_root>/.opencrab/state.sqlite`, creating `.opencrab` if missing.
/// The sqlite file itself is created by `rusqlite::Connection::open` on first
/// access; only the parent directory is ensured here.
pub(crate) fn state_sqlite_path(workspace_root: &Path) -> Result<PathBuf, TaskError> {
    let dir = crate::paths::project_root(workspace_root);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(crate::paths::project_state_sqlite(&dir))
}

// ---------------------------------------------------------------------------
// Connection lifecycle
// ---------------------------------------------------------------------------

/// Open `state.sqlite` and run idempotent migrations.
pub(crate) fn open_and_init(workspace_root: &Path) -> Result<Connection, TaskError> {
    let path = state_sqlite_path(workspace_root)?;
    let conn = Connection::open(&path)?;
    // WAL is the format SqliteSaver expects for concurrent reads; persisting
    // it once via `query_row` (which returns the resulting mode string) is
    // the canonical idempotent set. `synchronous=NORMAL` is the standard
    // pair for WAL — durable enough for our audit semantics, much cheaper
    // than FULL.
    conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    conn.execute("PRAGMA synchronous=NORMAL", [])?;
    // Match the busy_timeout used in tests/wal_coexistence.rs. SqliteSaver
    // sets a similar value; without it any cross-process contention would
    // surface as `SQLITE_BUSY` errors instead of a brief wait.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Path-driven entry point used by the Step-2 router. Opens + migrates the
/// store, runs `insert_proposed_batch`, and returns the persisted rows. The
/// router calls this from inside `tokio::task::spawn_blocking` because
/// `rusqlite::Connection` is `!Send`.
pub(crate) fn insert_proposed_batch_at_path(
    workspace_root: &Path,
    news: Vec<NewTask>,
) -> Result<Vec<super::types::Task>, TaskError> {
    let mut conn = open_and_init(workspace_root)?;
    insert_proposed_batch(&mut conn, news)
}

pub(crate) fn init_schema(conn: &Connection) -> Result<(), TaskError> {
    conn.execute(CREATE_TABLE_SQL, [])?;
    conn.execute(CREATE_INDEX_WORKSPACE_TEAM_STATUS_SQL, [])?;
    conn.execute(CREATE_INDEX_TEAM_UPDATED_AT_SQL, [])?;
    // Idempotent additive migrations for plan-level approval batching.
    // SQLite has no `ALTER TABLE ADD COLUMN IF NOT EXISTS`, so we probe
    // `PRAGMA table_info` first. The CREATE TABLE above already includes
    // the columns for fresh installs; the ALTER is only for upgrading an
    // existing `state.sqlite` that was created before the fix.
    ensure_column_exists(conn, "plan_id", "TEXT")?;
    ensure_column_exists(conn, "feedback", "TEXT")?;
    conn.execute(CREATE_INDEX_PLAN_ID_SQL, [])?;
    Ok(())
}

fn ensure_column_exists(conn: &Connection, column: &str, type_decl: &str) -> Result<(), TaskError> {
    let mut stmt = conn.prepare("PRAGMA table_info(tasks)")?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .collect();
    if columns.iter().any(|c| c == column) {
        return Ok(());
    }
    // Both new columns are NULLable additive; no DEFAULT needed. Using
    // string concat is safe — `column` and `type_decl` are hard-coded
    // constants (not user input). Parameterized DDL is not supported by
    // SQLite anyway.
    conn.execute(
        &format!("ALTER TABLE tasks ADD COLUMN {column} {type_decl}"),
        [],
    )?;
    eprintln!("[tasks] migration: added `{column}` column to existing `tasks` table");
    Ok(())
}

// ---------------------------------------------------------------------------
// Row hydration
// ---------------------------------------------------------------------------

/// Column list shared between `get`, `list`, and `list_by_plan`. Keep in
/// sync with `row_to_task` — any column read here must be present in
/// every SELECT this module issues, and the SELECT order must match the
/// struct field order in `row_to_task`.
const TASK_COLUMNS_SQL: &str = "id, workspace_id, team_id, assignee_agent_id, \
     proposed_by_agent_id, status, title, body, approved_at, completed_at, \
     created_at, updated_at, plan_id, feedback";

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let status_str: String = row.get("status")?;
    let status = TaskStatus::from_str(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown status in row: {status_str}"),
            )),
        )
    })?;
    Ok(Task {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        team_id: row.get("team_id")?,
        assignee_agent_id: row.get("assignee_agent_id")?,
        proposed_by_agent_id: row.get("proposed_by_agent_id")?,
        status,
        title: row.get("title")?,
        body: row.get("body")?,
        approved_at: row.get("approved_at")?,
        completed_at: row.get("completed_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        plan_id: row.get("plan_id")?,
        feedback: row.get("feedback")?,
    })
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

/// Initial-insert seed. Step 1 only writes status `proposed`, which mirrors
/// the PM "plan proposed" entry point that Step 2 will plug in. The actor
/// (`proposed_by_agent_id`) is whoever authored the plan; `assignee_agent_id`
/// is `None` until approval.
pub(crate) struct NewTask {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) team_id: String,
    pub(crate) assignee_agent_id: Option<String>,
    pub(crate) proposed_by_agent_id: String,
    pub(crate) title: String,
    pub(crate) body: String,
}

// `insert_proposed` is the single-task insertion entry point retained
// for unit tests + the `#[cfg(test)]` `create_task_dev_only_at_path`
// helper. Production seed path is `insert_proposed_batch_at_path`
// (called by the propose_plan handler). Marking allow(dead_code)
// rather than #[cfg(test)] keeps the symbol stable so future use
// cases don't have to re-export it.
#[allow(dead_code)]
pub(crate) fn insert_proposed(conn: &Connection, new: NewTask) -> Result<Task, TaskError> {
    // Single-task entry point ALSO gets a plan_id — a "plan of one" goes
    // through the same merged-message code path as a multi-task plan,
    // just with one row. Avoiding a NULL plan_id here keeps the legacy
    // (NULL ⇒ per-task message) path narrow to actual pre-fix rows.
    let plan_id = format!("plan_{}", uuid::Uuid::new_v4());
    insert_proposed_inner(conn, new, &Utc::now().to_rfc3339(), Some(&plan_id))
}

// ---------------------------------------------------------------------------
// Step 4: content-edit patch for `proposed` rows (modal "edit + approve" path).
//
// Why this lives next to `transition`: both are mutators of a `tasks` row,
// and both must enforce that the row is still in `proposed` status — once
// the latch lifts (→ ready / archived / etc.), the row is no longer
// editable from the modal. Unlike `transition`, this is a content edit,
// not a status flip, so it does NOT go through the state-machine guard;
// the gate is just `status='proposed'` check + `updated_at` bump.
//
// Assignee tri-state: see `AssigneeUpdate`. The modal needs three distinct
// intents: leave the field alone, set it to a specific agent, or null it
// out. A flat `Option<String>` collapses #2 and #3 with a sentinel
// (Some("") → null) which is ugly and unsafe under serde. A tagged enum
// keeps each intent named at the wire level.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(crate) enum AssigneeUpdate {
    /// Don't touch the assignee column. Default when the modal user did
    /// not interact with the assignee dropdown.
    Keep,
    /// Write `assignee_agent_id = ?value`. The router-side proposer write
    /// path coerces unknown agent ids to NULL + warn; the modal's edit
    /// path trusts the caller to send a real id (the dropdown is built
    /// from `team.json agents`) so we don't repeat that check here.
    Set { value: String },
    /// Write `assignee_agent_id = NULL` (e.g. user explicitly chose
    /// "Unassigned" from the dropdown after an assignee was set).
    Clear,
}

impl Default for AssigneeUpdate {
    fn default() -> Self {
        AssigneeUpdate::Keep
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskPatch {
    /// `None` = don't touch title. `Some(s)` = write `title = s`.
    #[serde(default)]
    pub(crate) title: Option<String>,
    /// `None` = don't touch body. `Some(s)` = write `body = s`.
    #[serde(default)]
    pub(crate) body: Option<String>,
    /// Tri-state. See `AssigneeUpdate`.
    #[serde(default)]
    pub(crate) assignee: AssigneeUpdate,
}

impl TaskPatch {
    pub(crate) fn is_noop(&self) -> bool {
        self.title.is_none() && self.body.is_none() && matches!(self.assignee, AssigneeUpdate::Keep)
    }
}

/// Apply `patch` to a `proposed` task row. Returns the updated `Task`.
///
/// Refuses to edit a row that is not currently `proposed` — returns
/// `IllegalTransition { from: <current>, to: Proposed }` to surface the
/// "you can't edit a task that's already been approved / archived" error
/// through the same typed-error channel the state machine uses. The `to`
/// argument is a slight stretch (this isn't a real transition), but it
/// keeps the Tauri-side error shape consistent for the modal.
///
/// `is_noop()` patches return the current `Task` unchanged AND **do not**
/// bump `updated_at`. The modal should not call this for empty edits;
/// guarding here is defense-in-depth.
pub(crate) fn update_content(
    conn: &Connection,
    task_id: &str,
    patch: TaskPatch,
    actor: &str,
) -> Result<Task, TaskError> {
    let mut task = get(conn, task_id)?.ok_or_else(|| TaskError::NotFound(task_id.to_string()))?;
    if task.status != TaskStatus::Proposed {
        return Err(TaskError::IllegalTransition {
            from: task.status,
            to: TaskStatus::Proposed,
        });
    }
    if patch.is_noop() {
        return Ok(task);
    }
    let now = Utc::now().to_rfc3339();
    if let Some(title) = patch.title {
        task.title = title;
    }
    if let Some(body) = patch.body {
        task.body = body;
    }
    match patch.assignee {
        AssigneeUpdate::Keep => {}
        AssigneeUpdate::Set { value } => task.assignee_agent_id = Some(value),
        AssigneeUpdate::Clear => task.assignee_agent_id = None,
    }
    task.updated_at = now;
    conn.execute(
        "UPDATE tasks SET title = ?1, body = ?2, assignee_agent_id = ?3, \
         updated_at = ?4 WHERE id = ?5",
        params![
            task.title,
            task.body,
            task.assignee_agent_id,
            task.updated_at,
            task.id,
        ],
    )?;
    eprintln!("[tasks] {id} content edit (actor={actor})", id = task.id);
    Ok(task)
}

pub(crate) fn update_content_at_path(
    workspace_root: &Path,
    task_id: &str,
    patch: TaskPatch,
    actor: &str,
) -> Result<Task, TaskError> {
    let conn = open_and_init(workspace_root)?;
    update_content(&conn, task_id, patch, actor)
}

/// Batch insert. The whole plan goes in ONE transaction: if any row fails
/// to insert (e.g. UUID collision — vanishing-rare but the SQL still
/// errors), nothing is persisted. Step 2's malformed-plan policy ("drop the
/// whole `<propose_plan>` block") is enforced upstream in the parser; this
/// function trusts that all rows in the batch are well-formed and that the
/// caller has already decided to commit the plan as a unit.
///
/// All rows in the batch share the same `created_at` / `updated_at`
/// timestamp so a downstream consumer (Step 4 plan-review modal, Step 5
/// PM continuation message) can group them by "plan emission moment"
/// without a separate `plan_id` column. The shared timestamp is the wire
/// stand-in until a plan-grouping column gets added (Phase 4+).
pub(crate) fn insert_proposed_batch(
    conn: &mut Connection,
    news: Vec<NewTask>,
) -> Result<Vec<Task>, TaskError> {
    let tx = conn.transaction()?;
    let now = Utc::now().to_rfc3339();
    // Phase 3 bug fix: every batch shares one `plan_id`. The notify_*
    // path uses this to wait until ALL tasks in the plan have been
    // resolved (approved / archived) before sending the merged
    // continuation message to the proposer.
    let plan_id = format!("plan_{}", uuid::Uuid::new_v4());
    let mut out = Vec::with_capacity(news.len());
    for new in news {
        out.push(insert_proposed_inner(&tx, new, &now, Some(&plan_id))?);
    }
    tx.commit()?;
    Ok(out)
}

fn insert_proposed_inner(
    conn: &Connection,
    new: NewTask,
    now: &str,
    plan_id: Option<&str>,
) -> Result<Task, TaskError> {
    let task = Task {
        id: new.id,
        workspace_id: new.workspace_id,
        team_id: new.team_id,
        assignee_agent_id: new.assignee_agent_id,
        proposed_by_agent_id: new.proposed_by_agent_id,
        status: TaskStatus::Proposed,
        title: new.title,
        body: new.body,
        approved_at: None,
        completed_at: None,
        created_at: now.to_string(),
        updated_at: now.to_string(),
        plan_id: plan_id.map(str::to_string),
        feedback: None,
    };
    conn.execute(
        "INSERT INTO tasks (id, workspace_id, team_id, assignee_agent_id, \
         proposed_by_agent_id, status, title, body, approved_at, completed_at, \
         created_at, updated_at, plan_id, feedback) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            task.id,
            task.workspace_id,
            task.team_id,
            task.assignee_agent_id,
            task.proposed_by_agent_id,
            task.status.as_str(),
            task.title,
            task.body,
            task.approved_at,
            task.completed_at,
            task.created_at,
            task.updated_at,
            task.plan_id,
            task.feedback,
        ],
    )?;
    Ok(task)
}

pub(crate) fn get(conn: &Connection, task_id: &str) -> Result<Option<Task>, TaskError> {
    let sql = format!("SELECT {TASK_COLUMNS_SQL} FROM tasks WHERE id = ?1");
    let task = conn
        .query_row(&sql, params![task_id], row_to_task)
        .optional()?;
    Ok(task)
}

/// Plan-level approval batching support (Phase 3 bug fix). Returns every
/// task in `plan_id`, ordered by `created_at` (which is identical across
/// the batch — see `insert_proposed_batch` — but kept as the ORDER BY
/// key so the merged-message ordering is deterministic if a future
/// migration adds per-row timestamps).
pub(crate) fn list_by_plan(conn: &Connection, plan_id: &str) -> Result<Vec<Task>, TaskError> {
    let sql =
        format!("SELECT {TASK_COLUMNS_SQL} FROM tasks WHERE plan_id = ?1 ORDER BY created_at, id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![plan_id], row_to_task)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// "Does this plan still have any `proposed` rows?" — the wait-for-plan-
/// completion predicate. Cheap query (the `plan_id` partial index makes
/// it index-only). Returns `false` once every task in the plan has been
/// resolved (transitioned out of `proposed`).
pub(crate) fn plan_still_has_proposed(conn: &Connection, plan_id: &str) -> Result<bool, TaskError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE plan_id = ?1 AND status = 'proposed'",
        params![plan_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// `status_filter == None` means "any status". Empty Vec also means "any".
/// Result is ordered by `updated_at DESC` so the most-recently-touched task
/// lands at the top (matches the `idx_tasks_team_updated_at` index).
pub(crate) fn list(
    conn: &Connection,
    workspace_id: &str,
    team_id: &str,
    status_filter: Option<&[TaskStatus]>,
) -> Result<Vec<Task>, TaskError> {
    let filter_active = status_filter.is_some_and(|f| !f.is_empty());
    let sql_base =
        format!("SELECT {TASK_COLUMNS_SQL} FROM tasks WHERE workspace_id = ?1 AND team_id = ?2");
    let sql_base = sql_base.as_str();

    let mut tasks: Vec<Task> = Vec::new();
    if !filter_active {
        let sql = format!("{sql_base} ORDER BY updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![workspace_id, team_id], row_to_task)?;
        for row in rows {
            tasks.push(row?);
        }
    } else {
        // SAFETY: the IN-list is built from `as_str()` of a closed enum, not
        // from user input — no SQL-injection surface. We still parameterize
        // workspace_id/team_id.
        let filter = status_filter.unwrap();
        let placeholders = vec!["?"; filter.len()].join(",");
        let sql = format!("{sql_base} AND status IN ({placeholders}) ORDER BY updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let mut param_values: Vec<String> = vec![workspace_id.to_string(), team_id.to_string()];
        for s in filter {
            param_values.push(s.as_str().to_string());
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(param_values.iter()), row_to_task)?;
        for row in rows {
            tasks.push(row?);
        }
    }
    Ok(tasks)
}

/// The single function in this crate that writes `tasks.status`. Anything
/// that wants to change a task's status goes through here; the state-machine
/// guard runs first, the timestamp side-effects fire on legal transitions,
/// and `updated_at` is bumped unconditionally.
///
/// `actor` is captured for the future audit log (jsonl mirror in Step 1+);
/// Step 1 only logs the (id, from, to, actor) tuple to stderr so the
/// invariant ("transition is the only writer") is visible while we are
/// bedding the contract down. We deliberately do not panic or silent-ignore
/// on illegal transitions: an `IllegalTransition` error bubbles to the
/// Tauri command boundary and turns into a normal `Err(String)` response.
pub(crate) fn transition(
    conn: &Connection,
    task_id: &str,
    new_status: TaskStatus,
    actor: &str,
) -> Result<Task, TaskError> {
    transition_with_feedback(conn, task_id, new_status, actor, None)
}

/// Same as `transition`, plus a one-shot `feedback` write that lands in
/// the `feedback` column when the caller has a value to persist. Used by
/// the reject path (Phase 3 bug fix): the user's rejection feedback is
/// committed to the DB so the plan-completion merged message can rebuild
/// the rejection wording from the DB after a restart. `feedback=None` is
/// the legacy path (no write to the column).
pub(crate) fn transition_with_feedback(
    conn: &Connection,
    task_id: &str,
    new_status: TaskStatus,
    actor: &str,
    feedback: Option<&str>,
) -> Result<Task, TaskError> {
    let mut task = get(conn, task_id)?.ok_or_else(|| TaskError::NotFound(task_id.to_string()))?;
    let from = task.status;
    validate_transition(from, new_status)?;
    let effects = effects_of(from, new_status);
    let now = Utc::now().to_rfc3339();

    task.status = new_status;
    task.updated_at = now.clone();
    if effects.set_approved_at {
        task.approved_at = Some(now.clone());
    }
    if effects.set_completed_at {
        task.completed_at = Some(now.clone());
    }
    if let Some(fb) = feedback {
        task.feedback = Some(fb.to_string());
    }

    conn.execute(
        "UPDATE tasks SET status = ?1, updated_at = ?2, \
         approved_at = COALESCE(?3, approved_at), \
         completed_at = COALESCE(?4, completed_at), \
         feedback = COALESCE(?5, feedback) \
         WHERE id = ?6",
        params![
            task.status.as_str(),
            task.updated_at,
            if effects.set_approved_at {
                Some(now.clone())
            } else {
                None
            },
            if effects.set_completed_at {
                Some(now)
            } else {
                None
            },
            feedback,
            task.id,
        ],
    )?;

    // Lightweight breadcrumb until the jsonl audit lands in a later step.
    // `Step 5` is the planned home for the structured `tasks.jsonl` mirror.
    eprintln!(
        "[tasks] {id} {from} → {to} (actor={actor})",
        id = task.id,
        from = from,
        to = new_status,
    );

    Ok(task)
}
