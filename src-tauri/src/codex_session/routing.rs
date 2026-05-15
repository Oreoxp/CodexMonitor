//! `SessionRouting` — V2 step 1.
//!
//! All per-session, *router-only* state lifted out of `WorkspaceSession`
//! (in `backend::app_server`) into a single shared struct.  The fields are
//! the same `Mutex`-wrapped collections that already existed; this commit
//! only changes their *home*, so the existing reader loop in
//! `setup_session_runtime` keeps working with a one-line indirection
//! (`session.x.lock()` → `session.routing.x.lock()`).
//!
//! The same `Arc<SessionRouting>` is shared between the legacy
//! `WorkspaceSession` (still owns the reader loop / RPC plumbing) and the
//! V1 `CodexSession` (lifecycle / status owner) so both observe a single
//! source of truth.
//!
//! V2 step 1 explicitly does **not**:
//! - migrate the reader loop from `app_server::setup_session_runtime`,
//! - migrate `thread/list` parsing or notification dispatch,
//! - introduce a watchdog or any new async tasks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

/// Per-session routing state.  Cheap to clone via `Arc`.
pub(crate) struct SessionRouting {
    /// The "owning" workspace this transport was originally spawned for.
    /// Used as the fallback routing target when no other workspace mapping
    /// can be derived from a notification.
    pub(crate) owner_workspace_id: String,
    /// All workspaces sharing this transport (the owner plus any workspaces
    /// that have been "joined" onto it for shared-session reuse).
    pub(crate) workspace_ids: Mutex<HashSet<String>>,
    /// Normalized filesystem root per `workspace_id`.  Used by
    /// `thread/list` → workspace mapping based on each thread's `cwd`.
    pub(crate) workspace_roots: Mutex<HashMap<String, String>>,
    /// `thread_id → workspace_id` derived from notifications and responses.
    pub(crate) thread_workspace: Mutex<HashMap<String, String>>,
    /// Threads that should not bubble up to the UI (memory consolidation,
    /// background helpers, etc.).
    pub(crate) hidden_thread_ids: Mutex<HashSet<String>>,
    /// `thread_id → mpsc sender` for callers awaiting background-thread
    /// notifications (commit-message generation, run-metadata, etc.).
    pub(crate) background_thread_callbacks:
        Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
    /// `thread_id → fan-out tap senders`. Phase 2.C: sidecar's blocking
    /// `codex_send_user_message` registers a tap so it can drain the
    /// `agentMessage/delta` stream and break on `turn/completed`. Unlike
    /// `background_thread_callbacks`, taps run **alongside** the UI emit —
    /// they never suppress notifications from reaching the frontend.
    pub(crate) tap_thread_callbacks:
        Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Value>>>>,
}

impl SessionRouting {
    /// Build a fresh routing keyed on the owner workspace.  The owner is
    /// pre-inserted into `workspace_ids` so callers don't have to remember.
    pub(crate) fn new(owner_workspace_id: String) -> Arc<Self> {
        Arc::new(Self {
            workspace_ids: Mutex::new(HashSet::from([owner_workspace_id.clone()])),
            workspace_roots: Mutex::new(HashMap::new()),
            thread_workspace: Mutex::new(HashMap::new()),
            hidden_thread_ids: Mutex::new(HashSet::new()),
            background_thread_callbacks: Mutex::new(HashMap::new()),
            tap_thread_callbacks: Mutex::new(HashMap::new()),
            owner_workspace_id,
        })
    }

    pub(crate) async fn register_workspace(&self, workspace_id: &str) {
        self.register_workspace_with_path(workspace_id, None).await;
    }

    pub(crate) async fn register_workspace_with_path(
        &self,
        workspace_id: &str,
        workspace_path: Option<&str>,
    ) {
        self.workspace_ids
            .lock()
            .await
            .insert(workspace_id.to_string());
        if let Some(path) = workspace_path {
            let normalized = normalize_root_path(path);
            if !normalized.is_empty() {
                self.workspace_roots
                    .lock()
                    .await
                    .insert(workspace_id.to_string(), normalized);
            }
        }
    }

    pub(crate) async fn unregister_workspace(&self, workspace_id: &str) {
        self.workspace_ids.lock().await.remove(workspace_id);
        self.workspace_roots.lock().await.remove(workspace_id);
    }

    pub(crate) async fn workspace_ids_snapshot(&self) -> Vec<String> {
        self.workspace_ids.lock().await.iter().cloned().collect()
    }

    /// Look up the workspace id mapped to `thread_id`, if any.
    pub(crate) async fn resolve_workspace_for_thread(&self, thread_id: &str) -> Option<String> {
        self.thread_workspace.lock().await.get(thread_id).cloned()
    }

    pub(crate) async fn map_thread_to_workspace(&self, thread_id: &str, workspace_id: &str) {
        self.thread_workspace
            .lock()
            .await
            .insert(thread_id.to_string(), workspace_id.to_string());
    }

    pub(crate) async fn forget_thread_workspace_mapping(&self, thread_id: &str) {
        self.thread_workspace.lock().await.remove(thread_id);
    }

    pub(crate) async fn mark_hidden_thread(&self, thread_id: &str) {
        self.hidden_thread_ids
            .lock()
            .await
            .insert(thread_id.to_string());
    }

    pub(crate) async fn forget_hidden_thread(&self, thread_id: &str) {
        self.hidden_thread_ids.lock().await.remove(thread_id);
    }

    pub(crate) async fn is_hidden_thread(&self, thread_id: &str) -> bool {
        self.hidden_thread_ids.lock().await.contains(thread_id)
    }

    pub(crate) async fn register_background_callback(
        &self,
        thread_id: String,
        sender: mpsc::UnboundedSender<Value>,
    ) {
        self.background_thread_callbacks
            .lock()
            .await
            .insert(thread_id, sender);
    }

    pub(crate) async fn take_background_callback(
        &self,
        thread_id: &str,
    ) -> Option<mpsc::UnboundedSender<Value>> {
        self.background_thread_callbacks
            .lock()
            .await
            .remove(thread_id)
    }

    /// Register a parallel observer (tap) for events on `thread_id`. Multiple
    /// taps may coexist, and they fire alongside (never instead of) the UI
    /// emit. Returned handle's `Drop` must call `unregister_tap_callback` —
    /// callers wrap registration in their own RAII or explicit cleanup.
    pub(crate) async fn register_tap_callback(
        &self,
        thread_id: String,
        sender: mpsc::UnboundedSender<Value>,
    ) {
        self.tap_thread_callbacks
            .lock()
            .await
            .entry(thread_id)
            .or_insert_with(Vec::new)
            .push(sender);
    }

    /// Remove a single tap by sender identity. Compares using
    /// `UnboundedSender::same_channel` so the caller's local handle is the
    /// matching key. If the entry's vec is emptied the map slot is dropped.
    pub(crate) async fn unregister_tap_callback(
        &self,
        thread_id: &str,
        sender: &mpsc::UnboundedSender<Value>,
    ) {
        let mut map = self.tap_thread_callbacks.lock().await;
        if let Some(vec) = map.get_mut(thread_id) {
            vec.retain(|s| !s.same_channel(sender));
            if vec.is_empty() {
                map.remove(thread_id);
            }
        }
    }
}

/// Normalize a filesystem root for cross-platform comparison.
///
/// Originally lived in `backend::app_server` (since the reader loop's
/// `thread/list` parsing also uses it).  Moved here because it is
/// fundamentally a routing concern; `app_server` re-exports through this
/// module via a `pub(crate) use` re-import.
pub(crate) fn normalize_root_path(value: &str) -> String {
    let normalized = value.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() {
        return String::new();
    }
    let lower = normalized.to_ascii_lowercase();
    let normalized = if lower.starts_with("//?/unc/") {
        format!("//{}", &normalized[8..])
    } else if lower.starts_with("//?/") || lower.starts_with("//./") {
        normalized[4..].to_string()
    } else {
        normalized.to_string()
    };
    if normalized.is_empty() {
        return String::new();
    }

    let bytes = normalized.as_bytes();
    let is_drive_path = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'/';
    if is_drive_path || normalized.starts_with("//") {
        normalized.to_ascii_lowercase()
    } else {
        normalized.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_unregister_round_trip() {
        let routing = SessionRouting::new("ws-owner".to_string());
        routing
            .register_workspace_with_path("ws-other", Some("/tmp/repo"))
            .await;
        assert_eq!(
            routing.workspace_ids_snapshot().await.len(),
            2,
            "owner + new workspace"
        );
        assert_eq!(
            routing.workspace_roots.lock().await.get("ws-other").cloned(),
            Some("/tmp/repo".to_string())
        );
        routing.unregister_workspace("ws-other").await;
        assert_eq!(routing.workspace_ids_snapshot().await.len(), 1);
        assert!(routing
            .workspace_roots
            .lock()
            .await
            .get("ws-other")
            .is_none());
    }

    #[tokio::test]
    async fn thread_workspace_map_and_resolve() {
        let routing = SessionRouting::new("ws-1".to_string());
        routing.map_thread_to_workspace("thr-a", "ws-1").await;
        assert_eq!(
            routing.resolve_workspace_for_thread("thr-a").await,
            Some("ws-1".to_string())
        );
        routing.forget_thread_workspace_mapping("thr-a").await;
        assert!(routing.resolve_workspace_for_thread("thr-a").await.is_none());
    }

    #[tokio::test]
    async fn hidden_thread_round_trip() {
        let routing = SessionRouting::new("ws-1".to_string());
        assert!(!routing.is_hidden_thread("thr-x").await);
        routing.mark_hidden_thread("thr-x").await;
        assert!(routing.is_hidden_thread("thr-x").await);
        routing.forget_hidden_thread("thr-x").await;
        assert!(!routing.is_hidden_thread("thr-x").await);
    }

    #[tokio::test]
    async fn background_callback_round_trip() {
        let routing = SessionRouting::new("ws-1".to_string());
        let (tx, _rx) = mpsc::unbounded_channel::<Value>();
        routing.register_background_callback("thr-bg".to_string(), tx).await;
        assert!(routing.take_background_callback("thr-bg").await.is_some());
        assert!(routing.take_background_callback("thr-bg").await.is_none());
    }

    #[test]
    fn normalize_strips_windows_namespace_prefix() {
        assert_eq!(
            normalize_root_path("\\\\?\\UNC\\SERVER\\Share\\Repo\\"),
            "//server/share/repo"
        );
    }

    #[test]
    fn normalize_keeps_posix_paths_case() {
        assert_eq!(normalize_root_path("/tmp/Repo/"), "/tmp/Repo");
    }
}
