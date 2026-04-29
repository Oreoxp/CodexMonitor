use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Clone)]
pub(crate) struct AppServerEvent {
    pub(crate) workspace_id: String,
    pub(crate) message: Value,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct TerminalOutput {
    #[serde(rename = "workspaceId")]
    pub(crate) workspace_id: String,
    #[serde(rename = "terminalId")]
    pub(crate) terminal_id: String,
    pub(crate) data: String,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct TerminalExit {
    #[serde(rename = "workspaceId")]
    pub(crate) workspace_id: String,
    #[serde(rename = "terminalId")]
    pub(crate) terminal_id: String,
}

pub(crate) trait EventSink: Clone + Send + Sync + 'static {
    fn emit_app_server_event(&self, event: AppServerEvent);
    fn emit_terminal_output(&self, event: TerminalOutput);
    fn emit_terminal_exit(&self, event: TerminalExit);
}

/// Notified by the legacy `app_server::setup_session_runtime` reader loop when
/// it exits, so the V1 `CodexSessionManager` can flip the session status to
/// `Disconnected` (clean EOF) or `Crashed` (transport error).
///
/// This trait is intentionally synchronous so it can be called from inside the
/// reader's `tokio::spawn` block without nested async indirection.  The
/// implementer is expected to spawn its own task if the resulting work is
/// async (the V1 manager does this).
pub(crate) trait ReaderExitNotifier: Send + Sync {
    fn notify_exit(&self, workspace_id: String, last_error: Option<String>);
}
