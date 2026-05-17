// Task wire shape — what the frontend sees and what `rusqlite` rows are
// hydrated into. Timestamps are RFC3339 UTC strings to match the storage
// doc's serialization discipline (`docs/architecture/opencrab-3.0-storage.md`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TaskStatus {
    Proposed,
    Ready,
    Running,
    Blocked,
    Done,
    Archived,
}

impl TaskStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Proposed => "proposed",
            TaskStatus::Ready => "ready",
            TaskStatus::Running => "running",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
            TaskStatus::Archived => "archived",
        }
    }

    pub(crate) fn from_str(s: &str) -> Option<Self> {
        match s {
            "proposed" => Some(TaskStatus::Proposed),
            "ready" => Some(TaskStatus::Ready),
            "running" => Some(TaskStatus::Running),
            "blocked" => Some(TaskStatus::Blocked),
            "done" => Some(TaskStatus::Done),
            "archived" => Some(TaskStatus::Archived),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Task {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) team_id: String,
    pub(crate) assignee_agent_id: Option<String>,
    pub(crate) proposed_by_agent_id: String,
    pub(crate) status: TaskStatus,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) approved_at: Option<String>,
    pub(crate) completed_at: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    /// Plan-level approval batching key (Phase 3 bug fix, 2026-05-17).
    /// Every task that came out of one `<propose_plan>` block shares a
    /// `plan_id`. `None` is reserved for legacy rows written before the
    /// fix; the notify_* path special-cases `None` to mean "fall back to
    /// per-task system messages" — strict backward compatibility.
    pub(crate) plan_id: Option<String>,
    /// Rejection feedback persisted by `reject_task`. Survives restarts
    /// so the plan-completion merged message can rebuild itself from the
    /// DB after a mid-review crash / close-and-reopen. `None` for tasks
    /// that were not rejected (the merged message renders
    /// `"(no feedback provided)"` if a rejection's feedback is None).
    pub(crate) feedback: Option<String>,
}
