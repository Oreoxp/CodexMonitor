// Phase 3 Step 1 — minimal `tasks` table + Rust-side state-machine guard.
//
// Layered responsibility (read this first):
//   - `types`        — `Task` struct, `TaskStatus` enum (wire shape).
//   - `state_machine`— transition matrix + `TaskError` enum. Pure logic, no
//                      IO; the only place a status string ever turns into a
//                      validated `(from, to)` decision.
//   - `store`        — `rusqlite` access to `<workspace>/.opencrab/state.sqlite`.
//                      Idempotent `init_schema`, CRUD, and the single
//                      `transition` writer that goes through the state
//                      machine before touching `status`.
//   - `commands`     — `#[tauri::command]` wrappers; resolve `workspace_id`
//                      → workspace root → sqlite path, then delegate.
//
// Decision summary (full rationale in §2 of the Step 1 report): tasks live
// as a sibling table inside the existing `state.sqlite` (option (a)). Rust
// opens the file with `rusqlite` directly; LangGraph's SqliteSaver continues
// to own its own checkpoint tables in the same DB. The two share the file
// via SQLite's WAL mode.

pub(crate) mod commands;
mod state_machine;
pub(crate) mod store;
mod types;

#[cfg(test)]
mod tests;

pub(crate) use commands::*;
pub(crate) use store::{insert_proposed_batch_at_path, AssigneeUpdate, NewTask, TaskPatch};
pub(crate) use types::{Task, TaskStatus};

/// Tauri event name + payload shape for the Step-2 `propose_plan` write
/// path. The constant lives here so both the emitter (router) and any
/// future test that asserts the event surface can reference one string.
pub(crate) mod tasks_proposed_event {
    /// Tauri event name. The frontend (Step 4) will subscribe to this. Do
    /// not rename — additive payload changes only.
    pub(crate) const NAME: &str = "tasks-proposed";

    /// Payload `schemaVersion`. Phase 3 closeout (#19): the frontend now
    /// pins on this literal so a future breaking field rename can force a
    /// type widening (`1 | 2`) rather than a silent payload mismatch.
    ///
    /// Versioning policy:
    /// - Additive fields = no bump. Both old and new consumers see the
    ///   new field; the old type widens to optional and life goes on.
    /// - Field rename / type change / removal = bump. Old consumers get
    ///   a payload they cannot parse; they should ignore the event and
    ///   surface a "frontend is older than backend" warning if they want
    ///   to be polite. Frontend type widens to `1 | 2 | …`.
    pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;
}
