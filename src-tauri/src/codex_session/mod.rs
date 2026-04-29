//! V1 Codex session lifecycle layer.
//!
//! This module owns the new `CodexSessionManager` introduced as the first
//! step of P1 Runtime productization.  It is intentionally thin: it tracks
//! per-workspace status (`Starting`/`Connected`/`Initialized`/`Disconnected`/
//! `Crashed`/`Stopped`), emits the `codex/sessionStatus` Tauri event, and
//! orchestrates clean shutdown.
//!
//! The actual JSON-RPC traffic is still served by `WorkspaceSession` in
//! `backend::app_server`.  V1 keeps that path untouched; the manager merely
//! shares a transport `Arc` with it via `CodexSession.rpc` and observes
//! lifecycle transitions.

mod manager;
mod session;
mod status;

pub(crate) use manager::{CodexSessionManager, SessionStatusSink};
pub(crate) use session::CodexSession;
#[allow(unused_imports)]
pub(crate) use status::{
    CodexSessionStatus, CodexSessionStatusEvent, SESSION_STATUS_EVENT,
};
