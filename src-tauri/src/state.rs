use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Manager};
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::codex_session::CodexSessionManager;
use crate::dictation::DictationState;
use crate::event_sink::TauriEventSink;
use crate::shared::codex_core::CodexLoginCancelState;
use crate::sidecar_session::SidecarSessionManager;
use crate::storage::{read_settings, read_workspaces};
use crate::types::{AppSettings, TcpDaemonState, TcpDaemonStatus, WorkspaceEntry};

pub(crate) struct TcpDaemonRuntime {
    pub(crate) child: Option<Child>,
    pub(crate) status: TcpDaemonStatus,
}

impl Default for TcpDaemonRuntime {
    fn default() -> Self {
        Self {
            child: None,
            status: TcpDaemonStatus {
                state: TcpDaemonState::Stopped,
                pid: None,
                started_at_ms: None,
                last_error: None,
                listen_addr: None,
            },
        }
    }
}

pub(crate) struct AppState {
    pub(crate) workspaces: Mutex<HashMap<String, WorkspaceEntry>>,
    /// Legacy per-workspace `WorkspaceSession` map.  Kept for V1 backwards
    /// compatibility — the existing `codex_core::*_core` helpers still drive
    /// JSON-RPC traffic through it.  New connect requests are funneled
    /// through `session_manager` first; the resulting `WorkspaceSession` is
    /// still inserted here so command handlers don't need to change.
    pub(crate) sessions: Mutex<HashMap<String, Arc<crate::codex::WorkspaceSession>>>,
    /// V1 lifecycle owner: drives `Starting → Connected → Initialized`,
    /// emits `codex/sessionStatus` events, and ensures `codex app-server`
    /// children are torn down on app exit.
    pub(crate) session_manager: Arc<CodexSessionManager<TauriEventSink, TauriEventSink>>,
    /// Phase 1 Spike A: one sidecar (`npx tsx sidecar/src/main.ts`) per workspace.
    pub(crate) sidecar_sessions: Arc<SidecarSessionManager>,
    /// Phase 2 pivot: per-workspace team router (permanent taps + tag dispatch
    /// consumer tasks for inter-agent `<send_message>` routing). Replaced
    /// wholesale on each `team_router_start` reverse-RPC from the sidecar.
    pub(crate) team_routers: Arc<crate::sidecar_session::team_router::TeamRouters>,
    pub(crate) terminal_sessions: Mutex<HashMap<String, Arc<crate::terminal::TerminalSession>>>,
    pub(crate) remote_backend: Mutex<Option<crate::remote_backend::RemoteBackend>>,
    pub(crate) storage_path: PathBuf,
    pub(crate) settings_path: PathBuf,
    pub(crate) app_settings: Mutex<AppSettings>,
    pub(crate) dictation: Mutex<DictationState>,
    pub(crate) codex_login_cancels: Mutex<HashMap<String, CodexLoginCancelState>>,
    pub(crate) tcp_daemon: Mutex<TcpDaemonRuntime>,
}

impl AppState {
    pub(crate) fn load(app: &AppHandle) -> Self {
        let data_dir = app
            .path()
            .app_data_dir()
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| ".".into()));
        let storage_path = data_dir.join("workspaces.json");
        let settings_path = data_dir.join("settings.json");
        let workspaces = read_workspaces(&storage_path).unwrap_or_default();
        let app_settings = read_settings(&settings_path).unwrap_or_default();

        let event_sink = TauriEventSink::new(app.clone());
        let client_version = app.package_info().version.to_string();
        let session_manager =
            CodexSessionManager::new(event_sink.clone(), event_sink, client_version);

        Self {
            workspaces: Mutex::new(workspaces),
            sessions: Mutex::new(HashMap::new()),
            session_manager,
            sidecar_sessions: Arc::new(SidecarSessionManager::new()),
            team_routers: Arc::new(crate::sidecar_session::team_router::TeamRouters::new()),
            terminal_sessions: Mutex::new(HashMap::new()),
            remote_backend: Mutex::new(None),
            storage_path,
            settings_path,
            app_settings: Mutex::new(app_settings),
            dictation: Mutex::new(DictationState::default()),
            codex_login_cancels: Mutex::new(HashMap::new()),
            tcp_daemon: Mutex::new(TcpDaemonRuntime::default()),
        }
    }
}
