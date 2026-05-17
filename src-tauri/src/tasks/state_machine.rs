// State-machine guard for `tasks.status`.
//
// This module is intentionally IO-free: it just answers
// "is `(from, to)` a legal transition?" and "what timestamp side-effects
// does that transition imply?". The `store` module is the only writer of
// `status`, and it MUST call `validate_transition` before doing anything
// that updates the column. Tests assert this single-entry-point invariant
// by going through `store::transition` for every legal status change.

use super::types::TaskStatus;

#[derive(Debug)]
pub(crate) enum TaskError {
    /// `(from, to)` is not in the legal transition matrix.
    IllegalTransition { from: TaskStatus, to: TaskStatus },
    /// `task_id` was not in the table.
    NotFound(String),
    /// Underlying SQLite failure.
    Sqlite(rusqlite::Error),
    /// Filesystem failure preparing the sqlite path.
    Io(std::io::Error),
}

// Stable error-code prefixes for frontend detection. The Tauri command
// layer turns `TaskError` into `Err(String)` via `Display`; the frontend
// pattern-matches on the leading `[ERR_*]` prefix to render friendly
// per-variant UX (e.g. IllegalTransition → "this task was resolved
// elsewhere — Refresh" instead of the raw "illegal task transition" text).
//
// Wire contract: any change to a prefix value here is a breaking change
// for the frontend. The unit test `display_pins_error_code_prefixes`
// below pins each format string verbatim.
pub(crate) const ERR_CODE_ILLEGAL_TRANSITION: &str = "[ERR_ILLEGAL_TRANSITION]";
pub(crate) const ERR_CODE_NOT_FOUND: &str = "[ERR_NOT_FOUND]";
pub(crate) const ERR_CODE_SQLITE: &str = "[ERR_SQLITE]";
pub(crate) const ERR_CODE_IO: &str = "[ERR_IO]";

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskError::IllegalTransition { from, to } => {
                write!(
                    f,
                    "{ERR_CODE_ILLEGAL_TRANSITION} illegal task transition: {from} → {to}"
                )
            }
            TaskError::NotFound(id) => {
                write!(f, "{ERR_CODE_NOT_FOUND} task not found: {id}")
            }
            TaskError::Sqlite(e) => write!(f, "{ERR_CODE_SQLITE} sqlite: {e}"),
            TaskError::Io(e) => write!(f, "{ERR_CODE_IO} io: {e}"),
        }
    }
}

impl std::error::Error for TaskError {}

impl From<rusqlite::Error> for TaskError {
    fn from(e: rusqlite::Error) -> Self {
        TaskError::Sqlite(e)
    }
}

impl From<std::io::Error> for TaskError {
    fn from(e: std::io::Error) -> Self {
        TaskError::Io(e)
    }
}

/// Legal transitions for Phase 3 Step 1. Matches the matrix in the user's
/// Step 1 spec verbatim:
///
/// ```text
///   proposed → ready          (approve)
///   proposed → archived       (reject)
///   ready    → running        (dispatch; Step 5)
///   ready    → archived       (cancel before run)
///   running  → blocked
///   running  → done
///   blocked  → running        (unblock)
///   blocked  → archived
///   done     → archived
/// ```
///
/// Anything not listed here is rejected. No `(X → X)` self-loops, no
/// reverse arrows, no skipping (e.g. `proposed → running`).
pub(crate) fn is_legal(from: TaskStatus, to: TaskStatus) -> bool {
    matches!(
        (from, to),
        (TaskStatus::Proposed, TaskStatus::Ready)
            | (TaskStatus::Proposed, TaskStatus::Archived)
            | (TaskStatus::Ready, TaskStatus::Running)
            | (TaskStatus::Ready, TaskStatus::Archived)
            | (TaskStatus::Running, TaskStatus::Blocked)
            | (TaskStatus::Running, TaskStatus::Done)
            | (TaskStatus::Blocked, TaskStatus::Running)
            | (TaskStatus::Blocked, TaskStatus::Archived)
            | (TaskStatus::Done, TaskStatus::Archived)
    )
}

pub(crate) fn validate_transition(from: TaskStatus, to: TaskStatus) -> Result<(), TaskError> {
    if is_legal(from, to) {
        Ok(())
    } else {
        Err(TaskError::IllegalTransition { from, to })
    }
}

/// Side-effects implied by a legal transition. The caller (store) bumps
/// `updated_at` unconditionally; this returns whether the transition also
/// stamps `approved_at` or `completed_at`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TransitionEffects {
    pub(crate) set_approved_at: bool,
    pub(crate) set_completed_at: bool,
}

pub(crate) fn effects_of(from: TaskStatus, to: TaskStatus) -> TransitionEffects {
    // Side-effect rules from the spec:
    //   → ready : set approved_at = now (this is the user-approve moment)
    //   → done  : set completed_at = now
    let _ = from;
    TransitionEffects {
        set_approved_at: matches!(to, TaskStatus::Ready),
        set_completed_at: matches!(to, TaskStatus::Done),
    }
}
