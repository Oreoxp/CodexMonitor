// Phase 4 Step 7 — KANBAN.md mirror (per-agent projection of the tasks
// table, system-written, derived data, agents read-only).
//
// Path:   <cwd>/.opencrab/agents/<agent_id>/KANBAN.md
//
// Generation flow (one-way, tasks → KANBAN, never reverse):
//
//   1. A state-machine transition commits to sqlite.
//   2. Step 6 appends a `*.event` line to events.jsonl.
//   3. **Step 7 (this module)** reads the full tasks table for
//      `(workspace_id, team_id)`, reads the team roster from
//      `~/.opencrab/team.json`, and re-renders KANBAN.md for every agent.
//   4. The Step 5 prompt loader picks up the new bytes on the agent's
//      next turn (inode/mtime-cache misses; cached content invalidated).
//
// Failure mode: best-effort. A KANBAN write failure NEVER aborts the
// underlying transition — the row is already committed, the event is
// already appended, and a stale KANBAN is a UI-quality issue rather than
// a correctness one. Errors go to stderr via the caller's
// `unwrap_or_else(|e| eprintln!(...))` pattern.
//
// Rendering format is locked (`docs/architecture/opencrab-3.0-storage.md`
// will pick up the spec). Three columns:
//
//   ## Proposed     — status == proposed
//   ## In Progress  — status ∈ { ready, running, blocked }
//   ## Done         — status == done
//
// `archived` is filtered out entirely (rejected tasks AND completed-then-
// archived tasks both disappear from KANBAN — the audit trail lives in
// events.jsonl). Per-agent KANBAN only shows tasks whose
// `assignee_agent_id == agent_id` — proposed tasks without an assignee
// are intentionally invisible until a PM assigns them via approve.

mod render;
mod write;

#[cfg(test)]
mod tests;

pub(crate) use write::regenerate_all_kanbans;
