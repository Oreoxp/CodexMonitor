// Sidecar session manager — owns the workspace_id → SidecarSession map.
//
// Phase 1 Spike A scope: just a Mutex<HashMap>. No status events, no
// reconnect, no shutdown-all on app exit (the child is left orphaned if the
// user closes the app without calling `stop_sidecar`; that's a known
// spike-only behavior).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::session::SidecarSession;

#[derive(Default)]
pub(crate) struct SidecarSessionManager {
    sessions: Mutex<HashMap<String, Arc<SidecarSession>>>,
}

impl SidecarSessionManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn has(&self, workspace_id: &str) -> bool {
        self.sessions.lock().await.contains_key(workspace_id)
    }

    pub(crate) async fn get(&self, workspace_id: &str) -> Option<Arc<SidecarSession>> {
        self.sessions.lock().await.get(workspace_id).cloned()
    }

    pub(crate) async fn insert(&self, session: Arc<SidecarSession>) {
        let mut map = self.sessions.lock().await;
        map.insert(session.workspace_id.clone(), session);
    }

    pub(crate) async fn remove(&self, workspace_id: &str) -> Option<Arc<SidecarSession>> {
        self.sessions.lock().await.remove(workspace_id)
    }
}
