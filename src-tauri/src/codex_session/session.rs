//! `CodexSession` — per-workspace handle that owns an `Arc<CodexRpcClient>`
//! and tracks lifecycle status / last error.
//!
//! V1 keeps the existing `WorkspaceSession` (in `backend::app_server`) for
//! the actual JSON-RPC traffic.  `CodexSession` is the *new* observable layer
//! the manager exposes; it shares the same underlying transport `Arc` with
//! `WorkspaceSession`, but does not consume its reader (the legacy router
//! loop in `app_server::setup_session_runtime` still does that for V1).
//!
//! The fields are intentionally minimal — anything router-related stays on
//! `WorkspaceSession`.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::codex_transport::{CodexRpcClient, CodexTransportKind};

use super::status::CodexSessionStatus;

pub(crate) struct CodexSession {
    pub(crate) workspace_id: String,
    pub(crate) workspace_path: String,
    pub(crate) transport_kind: CodexTransportKind,
    pub(crate) created_at_ms: u64,
    /// RPC client wrapping the same transport `Arc` shared with the legacy
    /// `WorkspaceSession`.  In V1 we do NOT call `rpc.start()`; the legacy
    /// reader loop owns the consume side.  Future versions can flip to a
    /// pure-rpc model by enabling `rpc.start()` and rewiring the router.
    pub(crate) rpc: Arc<CodexRpcClient>,
    /// Has the manager been told to disconnect this session intentionally?
    /// Used to disambiguate `Stopped` from `Disconnected`/`Crashed` when the
    /// reader exits.
    intentional_stop: Mutex<bool>,
    status: Mutex<CodexSessionStatus>,
    last_error: Mutex<Option<String>>,
}

impl CodexSession {
    pub(crate) fn new(
        workspace_id: String,
        workspace_path: String,
        transport_kind: CodexTransportKind,
        rpc: Arc<CodexRpcClient>,
    ) -> Self {
        let created_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            workspace_id,
            workspace_path,
            transport_kind,
            created_at_ms,
            rpc,
            intentional_stop: Mutex::new(false),
            status: Mutex::new(CodexSessionStatus::Idle),
            last_error: Mutex::new(None),
        }
    }

    pub(crate) async fn status(&self) -> CodexSessionStatus {
        *self.status.lock().await
    }

    pub(crate) async fn last_error(&self) -> Option<String> {
        self.last_error.lock().await.clone()
    }

    /// Internal status setter — only the manager should call this so it can
    /// pair the change with an emitted event.
    pub(crate) async fn set_status_internal(
        &self,
        status: CodexSessionStatus,
        last_error: Option<String>,
    ) {
        *self.status.lock().await = status;
        if last_error.is_some() {
            *self.last_error.lock().await = last_error;
        } else if matches!(
            status,
            CodexSessionStatus::Initialized
                | CodexSessionStatus::Connected
                | CodexSessionStatus::Starting
        ) {
            // On a happy-path transition, clear stale errors from prior runs.
            *self.last_error.lock().await = None;
        }
    }

    pub(crate) async fn mark_intentional_stop(&self) {
        *self.intentional_stop.lock().await = true;
    }

    pub(crate) async fn was_intentional_stop(&self) -> bool {
        *self.intentional_stop.lock().await
    }

    /// Shut down the underlying RPC client / transport.
    pub(crate) async fn shutdown(&self) {
        let _ = self.rpc.shutdown().await;
    }
}
