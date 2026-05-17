//! `CodexSessionManager` — V1 lifecycle manager for per-workspace Codex
//! sessions.
//!
//! Responsibilities (V1):
//! - Drive the connect lifecycle: emit `Starting → Connected → Initialized`,
//!   or `Crashed` on any failure with `lastError` populated.
//! - Hold `workspace_id → Arc<CodexSession>` so the rest of the app can look
//!   up the rpc client / status without scattering the transport handle
//!   across `AppState`.
//! - Provide `disconnect(workspace_id)` for explicit close (emits `Stopped`)
//!   and `shutdown_all()` for app exit (each workspace → `Stopped`).
//! - Provide a hook the legacy router can call when its reader loop exits so
//!   the manager can emit `Disconnected` / `Crashed` accordingly.
//!
//! V1 explicitly does **not**:
//! - Migrate the router from `app_server::setup_session_runtime`.
//! - Run a watchdog / health-check loop.
//! - Emit `Busy` (the variant exists in the enum but is never produced).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;

use crate::backend::events::{AppServerEvent, EventSink, ReaderExitNotifier};
use crate::codex_transport::{create_transport, CodexRpcClient, CodexTransportKind};
use crate::types::WorkspaceEntry;

use super::lifecycle::spawn_workspace_session_with_manager_inner;
use super::routing::SessionRouting;
use super::session::{CodexSession, WorkspaceSession};
use super::status::{CodexSessionStatus, CodexSessionStatusEvent, SESSION_STATUS_EVENT};

/// Sink for the new `codex/sessionStatus` Tauri event.
///
/// Kept as its own trait (rather than overloading `EventSink`) so the existing
/// `EventSink` ABI stays stable and tests can stub it independently.
pub(crate) trait SessionStatusSink: Clone + Send + Sync + 'static {
    fn emit_session_status(&self, payload: CodexSessionStatusEvent);
}

pub(crate) struct CodexSessionManager<E, S>
where
    E: EventSink,
    S: SessionStatusSink,
{
    sessions: Mutex<HashMap<String, Arc<CodexSession>>>,
    event_sink: E,
    status_sink: S,
    client_version: String,
}

impl<E, S> CodexSessionManager<E, S>
where
    E: EventSink + Clone + 'static,
    S: SessionStatusSink + 'static,
{
    pub(crate) fn new(event_sink: E, status_sink: S, client_version: String) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            event_sink,
            status_sink,
            client_version,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn client_version(&self) -> &str {
        self.client_version.as_str()
    }

    #[allow(dead_code)]
    pub(crate) fn event_sink(&self) -> &E {
        &self.event_sink
    }

    /// Snapshot of the current `CodexSession` for `workspace_id`.
    pub(crate) async fn get(&self, workspace_id: &str) -> Option<Arc<CodexSession>> {
        self.sessions.lock().await.get(workspace_id).cloned()
    }

    /// Connect a workspace.  Returns the underlying `WorkspaceSession` so the
    /// legacy `state.sessions` map can be populated for backwards compat.
    ///
    /// On any failure mid-flight, no entry is left in `self.sessions` and a
    /// `Crashed` status event is emitted.
    pub(crate) async fn connect(
        self: &Arc<Self>,
        entry: WorkspaceEntry,
        default_codex_bin: Option<String>,
        codex_args: Option<String>,
        codex_home: Option<PathBuf>,
        transport_kind: Option<CodexTransportKind>,
    ) -> Result<Arc<WorkspaceSession>, String> {
        let workspace_id = entry.id.clone();
        let workspace_path = entry.path.clone();

        // ── 1. Starting ─────────────────────────────────────────────────
        // No CodexSession exists yet, so we emit a synthetic event with
        // `transport: "unknown"`.  Once the transport is up the next event
        // will carry the real value.
        self.emit_status_synthetic(&workspace_id, CodexSessionStatus::Starting, "unknown", None);

        // ── 2. Create transport ─────────────────────────────────────────
        let bundle = match create_transport(
            default_codex_bin,
            codex_args.as_deref(),
            &entry.path,
            codex_home.as_ref(),
            transport_kind,
        )
        .await
        {
            Ok(b) => b,
            Err(err) => {
                self.emit_status_synthetic(
                    &workspace_id,
                    CodexSessionStatus::Crashed,
                    "unknown",
                    Some(err.clone()),
                );
                return Err(err);
            }
        };

        let transport_kind_value = bundle.kind;
        let transport_kind_str = transport_kind_value.to_string();

        // Forward the transport-selected / fallback notifications to the
        // existing `app-server-event` channel so the UI's transport badge
        // keeps working unchanged.
        self.event_sink.emit_app_server_event(AppServerEvent {
            workspace_id: workspace_id.clone(),
            message: json!({
                "method": "codex/transportSelected",
                "params": {
                    "transport": &transport_kind_str,
                    "fallback": bundle.fallback_warning.is_some(),
                }
            }),
        });
        if let Some(warning) = bundle.fallback_warning.as_ref() {
            self.event_sink.emit_app_server_event(AppServerEvent {
                workspace_id: workspace_id.clone(),
                message: json!({
                    "method": "codex/transportFallback",
                    "params": {
                        "warning": warning,
                        "activeTransport": &transport_kind_str,
                    }
                }),
            });
        }

        // ── 3. Build CodexRpcClient (passive in V1) and SessionRouting ──
        // Share the same `Arc<dyn CodexTransport>` with the legacy reader
        // loop.  We do **not** call `rpc.start()` here — the legacy router
        // owns the consume side.  The rpc handle is exposed on
        // `CodexSession.rpc` so future iterations can flip to a pure-rpc
        // dispatcher without touching this entrypoint.
        //
        // V2 step 1: build the routing here (one source of truth) and hand
        // a clone to both `CodexSession` and the legacy `WorkspaceSession`
        // via `setup_session_runtime`.
        let rpc = Arc::new(CodexRpcClient::new_from_arc(Arc::clone(&bundle.transport)));
        let routing = SessionRouting::new(workspace_id.clone());

        let codex_session = Arc::new(CodexSession::new(
            workspace_id.clone(),
            workspace_path.clone(),
            transport_kind_value,
            Arc::clone(&rpc),
            Arc::clone(&routing),
        ));

        codex_session
            .set_status_internal(CodexSessionStatus::Connected, None)
            .await;
        self.emit_status(&codex_session).await;

        // Build the reader-exit notifier adapter.  Holds an `Arc<Self>` so
        // it can call `notify_reader_exit` on the manager when the legacy
        // reader loop terminates.
        let exit_notifier: Arc<dyn ReaderExitNotifier> = Arc::new(ManagerExitNotifier::<E, S> {
            manager: Arc::clone(self),
        });

        // ── 4. Hand off to setup_session_runtime, sharing the rpc handle.
        // setup_session_runtime calls `rpc.start()` + `rpc.initialize()` and
        // spawns `lifecycle::start_router`.
        let workspace_session = match spawn_workspace_session_with_manager_inner(
            entry,
            Arc::clone(&rpc),
            bundle.transport,
            bundle.stderr_rx,
            codex_args,
            self.client_version.clone(),
            self.event_sink.clone(),
            exit_notifier,
            Arc::clone(&routing),
        )
        .await
        {
            Ok(session) => session,
            Err(err) => {
                codex_session
                    .set_status_internal(CodexSessionStatus::Crashed, Some(err.clone()))
                    .await;
                self.emit_status(&codex_session).await;
                return Err(err);
            }
        };

        // ── 5. Initialized ──────────────────────────────────────────────
        codex_session
            .set_status_internal(CodexSessionStatus::Initialized, None)
            .await;
        self.emit_status(&codex_session).await;

        // ── 6. Insert into manager registry ─────────────────────────────
        self.sessions
            .lock()
            .await
            .insert(workspace_id, codex_session);

        Ok(workspace_session)
    }

    /// Mark the session as intentionally stopped, drop it from the registry,
    /// shut down the underlying transport, and emit `Stopped`.
    pub(crate) async fn disconnect(&self, workspace_id: &str) {
        let removed = self.sessions.lock().await.remove(workspace_id);
        if let Some(session) = removed {
            session.mark_intentional_stop().await;
            session
                .set_status_internal(CodexSessionStatus::Stopped, None)
                .await;
            self.emit_status(&session).await;
            session.shutdown().await;
        }
    }

    /// Disconnect every tracked session.  Used on app exit to ensure no
    /// `codex app-server` child process is left behind.
    pub(crate) async fn shutdown_all(&self) {
        let all: Vec<(String, Arc<CodexSession>)> = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().collect()
        };
        for (_id, session) in all {
            session.mark_intentional_stop().await;
            session
                .set_status_internal(CodexSessionStatus::Stopped, None)
                .await;
            self.emit_status(&session).await;
            session.shutdown().await;
        }
    }

    /// Called by `ManagerExitNotifier` (an internal adapter) when the legacy
    /// router's reader loop terminates.  Translates the exit cause into
    /// `Stopped` / `Disconnected` / `Crashed` based on the intentional-stop
    /// flag and the supplied error message.
    pub(crate) async fn notify_reader_exit(&self, workspace_id: &str, last_error: Option<String>) {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(workspace_id).cloned()
        };
        let Some(session) = session else { return };

        // Don't override a Stopped state set by an intentional disconnect.
        if session.was_intentional_stop().await {
            return;
        }

        let status = if last_error.is_some() {
            CodexSessionStatus::Crashed
        } else {
            CodexSessionStatus::Disconnected
        };
        session.set_status_internal(status, last_error).await;
        self.emit_status(&session).await;
    }

    // ── internal helpers ───────────────────────────────────────────────

    async fn emit_status(&self, session: &Arc<CodexSession>) {
        let payload = CodexSessionStatusEvent {
            workspace_id: session.workspace_id.clone(),
            status: session.status().await,
            transport: session.transport_kind.to_string(),
            created_at_ms: session.created_at_ms,
            last_error: session.last_error().await,
        };
        self.status_sink.emit_session_status(payload);
    }

    fn emit_status_synthetic(
        &self,
        workspace_id: &str,
        status: CodexSessionStatus,
        transport: &str,
        last_error: Option<String>,
    ) {
        let created_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let payload = CodexSessionStatusEvent {
            workspace_id: workspace_id.to_string(),
            status,
            transport: transport.to_string(),
            created_at_ms,
            last_error,
        };
        self.status_sink.emit_session_status(payload);
    }
}

/// Internal adapter that bridges the synchronous `ReaderExitNotifier` callback
/// from the legacy reader loop into the asynchronous
/// `CodexSessionManager::notify_reader_exit`.
struct ManagerExitNotifier<E, S>
where
    E: EventSink,
    S: SessionStatusSink,
{
    manager: Arc<CodexSessionManager<E, S>>,
}

impl<E, S> ReaderExitNotifier for ManagerExitNotifier<E, S>
where
    E: EventSink + Clone + 'static,
    S: SessionStatusSink + 'static,
{
    fn notify_exit(&self, workspace_id: String, last_error: Option<String>) {
        let manager = Arc::clone(&self.manager);
        tokio::spawn(async move {
            manager.notify_reader_exit(&workspace_id, last_error).await;
        });
    }
}

/// Tauri event name re-exported so callers don't have to depend on `status`.
#[allow(dead_code)]
pub(crate) const SESSION_STATUS_EVENT_NAME: &str = SESSION_STATUS_EVENT;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::events::{AppServerEvent, EventSink, TerminalExit, TerminalOutput};
    use std::sync::Mutex as StdMutex;

    #[derive(Clone, Default)]
    struct CapturingStatusSink {
        events: Arc<StdMutex<Vec<CodexSessionStatusEvent>>>,
    }

    impl SessionStatusSink for CapturingStatusSink {
        fn emit_session_status(&self, payload: CodexSessionStatusEvent) {
            self.events.lock().unwrap().push(payload);
        }
    }

    #[derive(Clone, Default)]
    struct NoopEventSink;

    impl EventSink for NoopEventSink {
        fn emit_app_server_event(&self, _event: AppServerEvent) {}
        fn emit_terminal_output(&self, _event: TerminalOutput) {}
        fn emit_terminal_exit(&self, _event: TerminalExit) {}
    }

    #[tokio::test]
    async fn disconnect_unknown_workspace_is_silent_noop() {
        let manager = CodexSessionManager::new(
            NoopEventSink,
            CapturingStatusSink::default(),
            "0.0.0-test".to_string(),
        );
        manager.disconnect("ws-not-here").await;
        assert!(manager.status_sink.events.lock().unwrap().is_empty());
        assert!(manager.get("ws-not-here").await.is_none());
    }

    #[tokio::test]
    async fn shutdown_all_on_empty_registry_is_noop() {
        let manager = CodexSessionManager::new(
            NoopEventSink,
            CapturingStatusSink::default(),
            "0.0.0-test".to_string(),
        );
        manager.shutdown_all().await;
        assert!(manager.status_sink.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn notify_reader_exit_for_unknown_workspace_is_noop() {
        let manager = CodexSessionManager::new(
            NoopEventSink,
            CapturingStatusSink::default(),
            "0.0.0-test".to_string(),
        );
        manager
            .notify_reader_exit("ws-not-here", Some("boom".to_string()))
            .await;
        assert!(manager.status_sink.events.lock().unwrap().is_empty());
    }
}
