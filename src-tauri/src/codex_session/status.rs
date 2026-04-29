//! Status enum + Tauri event payload for `CodexSession`.
//!
//! V1 emits the following lifecycle states:
//! `Starting → Connected → Initialized` on connect, `Stopped` on intentional
//! disconnect, `Disconnected` on reader EOF / clean close, and `Crashed` when
//! the transport fails to start, `initialize` fails, or the child exits with a
//! non-zero status / transport error.
//!
//! `Idle` is the default for a newly constructed [`CodexSession`] before any
//! lifecycle work has begun.  `Busy` is reserved in the enum but is **not**
//! emitted in V1 — it will surface in a later iteration once turn-level
//! state plumbs through the manager.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexSessionStatus {
    /// Just constructed; no lifecycle action has been taken yet.
    Idle,
    /// Manager has begun connecting (transport not yet up).
    Starting,
    /// Transport is up but `initialize` has not completed.
    Connected,
    /// `initialize` handshake completed successfully.
    Initialized,
    /// Reserved for future use; never emitted in V1.
    #[allow(dead_code)]
    Busy,
    /// Reader saw EOF / websocket close after a clean lifetime.
    Disconnected,
    /// Transport startup failed, `initialize` failed, child exited non-zero,
    /// or the transport reported an error.
    Crashed,
    /// User actively disconnected the workspace, or the app is shutting down.
    Stopped,
}

impl CodexSessionStatus {
    /// Whether this is a terminal state (no further transitions expected).
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            CodexSessionStatus::Disconnected
                | CodexSessionStatus::Crashed
                | CodexSessionStatus::Stopped
        )
    }
}

/// Payload of the `codex/sessionStatus` Tauri event.
///
/// Field names use camelCase via `#[serde(rename = ...)]` so the frontend can
/// consume them without an extra translation layer.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexSessionStatusEvent {
    #[serde(rename = "workspaceId")]
    pub(crate) workspace_id: String,
    pub(crate) status: CodexSessionStatus,
    /// `"stdio"` | `"websocket"` | `"unknown"` (the latter only when the
    /// transport hasn't been picked yet, e.g. on the first `Starting` emit
    /// when `create_transport` has not run).
    pub(crate) transport: String,
    #[serde(rename = "createdAt")]
    pub(crate) created_at_ms: u64,
    #[serde(rename = "lastError")]
    pub(crate) last_error: Option<String>,
}

/// Tauri event name used for `CodexSessionStatusEvent`.
pub(crate) const SESSION_STATUS_EVENT: &str = "codex/sessionStatus";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_as_snake_case() {
        let json = serde_json::to_string(&CodexSessionStatus::Initialized).unwrap();
        assert_eq!(json, "\"initialized\"");
        let json = serde_json::to_string(&CodexSessionStatus::Disconnected).unwrap();
        assert_eq!(json, "\"disconnected\"");
    }

    #[test]
    fn terminal_states() {
        assert!(CodexSessionStatus::Stopped.is_terminal());
        assert!(CodexSessionStatus::Crashed.is_terminal());
        assert!(CodexSessionStatus::Disconnected.is_terminal());
        assert!(!CodexSessionStatus::Initialized.is_terminal());
        assert!(!CodexSessionStatus::Connected.is_terminal());
        assert!(!CodexSessionStatus::Starting.is_terminal());
        assert!(!CodexSessionStatus::Idle.is_terminal());
    }

    #[test]
    fn event_payload_uses_camel_case() {
        let event = CodexSessionStatusEvent {
            workspace_id: "ws-1".to_string(),
            status: CodexSessionStatus::Connected,
            transport: "websocket".to_string(),
            created_at_ms: 1_700_000_000_000,
            last_error: Some("boom".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["workspaceId"], "ws-1");
        assert_eq!(json["status"], "connected");
        assert_eq!(json["transport"], "websocket");
        assert_eq!(json["createdAt"], 1_700_000_000_000u64);
        assert_eq!(json["lastError"], "boom");
    }
}
