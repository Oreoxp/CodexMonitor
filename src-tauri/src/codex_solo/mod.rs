//! Solo Agent → Codex bridge.
//!
//! Minimal surface used by the LangGraph sidecar to drive a single
//! long-lived Codex thread per Solo Agent run.  Reuses the existing
//! `WorkspaceSession` / `CodexSessionManager` machinery via
//! `state.sessions`; never spawns or modifies the Codex CLI.
//!
//! Boundary:
//! - One Codex thread per Solo Agent thread (created lazily, reused).
//! - The Codex thread is hidden from normal-mode UI via the existing
//!   `register_background_callback` routing, so it does not appear in
//!   the user's chat list.
//! - This module owns NO graph state.  The sidecar persists the
//!   `codex_thread_id` in `mission.json` and passes it back on each call.

mod bridge;

pub(crate) use bridge::{run_codex_task, CodexRunTaskRequest, CodexRunTaskResult};
