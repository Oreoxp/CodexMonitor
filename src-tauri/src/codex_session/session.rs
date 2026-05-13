//! Per-workspace session handles.
//!
//! Two structs co-live here as of V2 step 3:
//!
//! - `CodexSession` — V1 lifecycle / status owner exposed by the manager.
//!   Holds the `Arc<CodexRpcClient>` and the `Arc<SessionRouting>`.
//! - `WorkspaceSession` — the legacy handle still used by all existing
//!   `state.sessions` callers (`shared::codex_core`, `shared::codex_aux_core`,
//!   etc.).  After V2 step 3 it's just a thin RPC façade: every send_*
//!   delegates to the same `CodexRpcClient` and every routing helper
//!   delegates to the same `SessionRouting`.  `child` / `stdin` /
//!   `transport` slots are kept only for legacy test fixtures.
//!
//! Both share the same underlying transport `Arc`, the same rpc, and the
//! same routing — so `state.sessions` (legacy) and
//! `state.session_manager.sessions` (V1) are two views of the same state.

use std::sync::Arc;

use serde_json::Value;
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex;

use crate::codex_transport::{CodexRpcClient, CodexTransport, CodexTransportKind};
use crate::shared::process_core::kill_child_process_tree;

use super::lifecycle::{extract_thread_id_from_params, record_response};
use super::routing::SessionRouting;
use super::status::CodexSessionStatus;

fn rpc_response_error_message(response: &Value) -> Option<String> {
    let error = response.get("error")?;
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }
    Some(error.to_string())
}

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
    /// Per-session routing state.  V2 step 1 lifts this out of
    /// `WorkspaceSession`; both structs hold the same `Arc<SessionRouting>`
    /// so the legacy reader loop and the new lifecycle layer observe a
    /// single source of truth.
    pub(crate) routing: Arc<SessionRouting>,
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
        routing: Arc<SessionRouting>,
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
            routing,
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

// ──────────────────────────────────────────────────────────────────────
// `WorkspaceSession` — the legacy handle every existing call site still
// uses.  V2 step 3 moved it out of `backend::app_server` so the routing /
// rpc / lifecycle code lives together; behaviour is unchanged.
// ──────────────────────────────────────────────────────────────────────

pub(crate) struct WorkspaceSession {
    pub(crate) codex_args: Option<String>,
    /// Legacy/test child handle.  Production code leaves `None` —
    /// process lifecycle is owned by the underlying transport via `rpc`.
    pub(crate) child: Option<Mutex<Child>>,
    /// Legacy/test stdin handle.  Same caveat as `child`.
    pub(crate) stdin: Option<Mutex<ChildStdin>>,
    /// Held for `is_alive` / `kill`.  Production code holds the same `Arc`
    /// that `rpc` wraps; tests leave this `None`.
    pub(crate) transport: Option<Arc<dyn CodexTransport>>,
    /// All RPC traffic flows through `CodexRpcClient`.  `None` only in
    /// test fixtures that never invoke `send_*`.
    pub(crate) rpc: Option<Arc<CodexRpcClient>>,
    /// Per-session routing state (workspace_ids / workspace_roots /
    /// thread_workspace / hidden_thread_ids / background_thread_callbacks).
    /// Shared via `Arc` with the matching `CodexSession`.
    pub(crate) routing: Arc<SessionRouting>,
}

impl WorkspaceSession {
    /// Owner workspace id (immutable, set at construction).
    pub(crate) fn owner_workspace_id(&self) -> &str {
        &self.routing.owner_workspace_id
    }

    pub(crate) async fn register_workspace(&self, workspace_id: &str) {
        self.routing.register_workspace(workspace_id).await;
    }

    pub(crate) async fn register_workspace_with_path(
        &self,
        workspace_id: &str,
        workspace_path: Option<&str>,
    ) {
        self.routing
            .register_workspace_with_path(workspace_id, workspace_path)
            .await;
    }

    pub(crate) async fn unregister_workspace(&self, workspace_id: &str) {
        self.routing.unregister_workspace(workspace_id).await;
    }

    pub(crate) async fn workspace_ids_snapshot(&self) -> Vec<String> {
        self.routing.workspace_ids_snapshot().await
    }

    /// Check whether the underlying Codex transport is still alive.
    pub(crate) async fn is_alive(&self) -> bool {
        if let Some(ref rpc) = self.rpc {
            return rpc.transport_is_alive().await;
        }
        if let Some(ref transport) = self.transport {
            return transport.is_alive().await;
        }
        if let Some(ref child_mutex) = self.child {
            let mut child = child_mutex.lock().await;
            return matches!(child.try_wait(), Ok(None));
        }
        false
    }

    /// Kill the underlying Codex transport.
    pub(crate) async fn kill(&self) {
        if let Some(ref rpc) = self.rpc {
            let _ = rpc.shutdown().await;
            return;
        }
        if let Some(ref transport) = self.transport {
            let _ = transport.close().await;
            return;
        }
        if let Some(ref child_mutex) = self.child {
            let mut child = child_mutex.lock().await;
            kill_child_process_tree(&mut child).await;
        }
    }

    pub(crate) async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        let owner = self.routing.owner_workspace_id.clone();
        self.send_request_for_workspace(owner.as_str(), method, params)
            .await
    }

    /// Send a JSON-RPC request and post-process the response into routing.
    ///
    /// Thin wrapper around `CodexRpcClient::request` — response-side
    /// bookkeeping happens inline via `lifecycle::record_response`.
    pub(crate) async fn send_request_for_workspace(
        &self,
        workspace_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let rpc = self
            .rpc
            .as_ref()
            .ok_or_else(|| "workspace session has no rpc client".to_string())?;

        self.register_workspace(workspace_id).await;
        if let Some(thread_id) = extract_thread_id_from_params(&params) {
            self.routing
                .map_thread_to_workspace(&thread_id, workspace_id)
                .await;
        }

        let response = rpc
            .request(method, params)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(message) = rpc_response_error_message(&response) {
            return Err(format!("{method} failed: {message}"));
        }
        record_response(&self.routing, workspace_id, method, &response).await;
        Ok(response)
    }

    pub(crate) async fn send_notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), String> {
        let rpc = self
            .rpc
            .as_ref()
            .ok_or_else(|| "workspace session has no rpc client".to_string())?;
        rpc.notify(method, params).await.map_err(|e| e.to_string())
    }

    pub(crate) async fn send_response(&self, id: Value, result: Value) -> Result<(), String> {
        let rpc = self
            .rpc
            .as_ref()
            .ok_or_else(|| "workspace session has no rpc client".to_string())?;
        rpc.send_response(id, result)
            .await
            .map_err(|e| e.to_string())
    }
}
