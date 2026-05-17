// Phase 3 Step 4 — task types for the plan-review modal + approval queue.
//
// Manually mirrored from:
//   - Rust:    CodexMonitor/src-tauri/src/tasks/types.rs (Task / TaskStatus)
//   - Rust:    CodexMonitor/src-tauri/src/tasks/store.rs (TaskPatch / AssigneeUpdate)
//   - Rust:    CodexMonitor/src-tauri/src/tasks/commands.rs (ApprovalResult)
//   - Rust:    CodexMonitor/src-tauri/src/sidecar_session/team_router.rs
//              (TasksProposedEvent)
//
// `serde(rename_all = "camelCase")` is set on every Rust struct here, so the
// names below match the wire shape verbatim. Keep these in sync — there is
// no codegen.

// ---------------------------------------------------------------------------
// Task state machine — string union (rusqlite stores the lowercase tag).
// ---------------------------------------------------------------------------

export type TaskStatus =
  | "proposed"
  | "ready"
  | "running"
  | "blocked"
  | "done"
  | "archived";

// ---------------------------------------------------------------------------
// Task — wire shape returned by list_tasks / get_task / approve_task /
// reject_task / update_task / transition_task.
// ---------------------------------------------------------------------------

export type Task = {
  id: string;
  workspaceId: string;
  teamId: string;
  assigneeAgentId: string | null;
  proposedByAgentId: string;
  status: TaskStatus;
  title: string;
  body: string;
  approvedAt: string | null;     // RFC3339 UTC, stamped on → ready
  completedAt: string | null;    // RFC3339 UTC, stamped on → done
  createdAt: string;             // RFC3339 UTC
  updatedAt: string;             // RFC3339 UTC
  // Plan-level approval batching (Phase 3 bug fix, 2026-05-17). Every
  // task that came out of one `<propose_plan>` block shares a `planId`.
  // `null` for legacy rows written before the fix. The frontend doesn't
  // currently branch on this — it's available for future plan-grouping
  // UI in Phase 4 polish.
  planId: string | null;
  // Rejection feedback persisted by `reject_task`. `null` for tasks that
  // weren't rejected. The merged plan-completion system message reads
  // this from the DB to embed verbatim feedback into the PM's continuation
  // message.
  feedback: string | null;
};

// ---------------------------------------------------------------------------
// TaskPatch — update_task wire shape (content edit on a proposed row).
//
// The Rust side uses a tagged enum (`#[serde(tag = "op")]`) for assignee
// because flat `Option<String>` collapses three distinct intents:
//   keep    : leave the column alone (the default if the modal user didn't
//             interact with the assignee dropdown)
//   set     : write a specific agent id
//   clear   : write NULL
// ---------------------------------------------------------------------------

export type AssigneeUpdate =
  | { op: "keep" }
  | { op: "set"; value: string }
  | { op: "clear" };

export type TaskPatch = {
  title?: string;        // omitted = don't touch
  body?: string;         // omitted = don't touch
  assignee?: AssigneeUpdate;  // omitted = { op: "keep" } (server-side default)
};

// ---------------------------------------------------------------------------
// ApprovalResult — approve_task / reject_task response shape.
//
// `pmNotified` is true iff the system continuation reply was successfully
// dispatched to the proposer's Codex thread. False can mean either "no
// live router for this workspace" or "send_user_message_core rejected the
// turn" — the DB transition is durable either way. The modal surfaces
// `pmNotified=false` with a warning chip so the user can manually re-prod
// PM.
// ---------------------------------------------------------------------------

export type ApprovalResult = {
  task: Task;
  pmNotified: boolean;
};

// ---------------------------------------------------------------------------
// `tasks-proposed` Tauri event payload — emitted by the team router after
// `<propose_plan>` blocks commit. Used to auto-open the modal + populate
// the approval queue.
//
// Versioning (Phase 3 closeout — #19): `schemaVersion` is the wire-stable
// contract. Bump policy on the Rust side at
// `tasks::tasks_proposed_event::CURRENT_SCHEMA_VERSION`. Widen the literal
// here to `1 | 2 | …` when a breaking change ships; the modal / hook
// should ignore events whose `schemaVersion` they do not recognize.
// Today consumers do not branch on this — they just round-trip it — so
// landing the field is non-breaking.
// ---------------------------------------------------------------------------

export type TasksProposedEvent = {
  schemaVersion: 1;
  workspaceId: string;
  teamId: string;
  proposedByAgentId: string;
  tasks: Task[];
};
