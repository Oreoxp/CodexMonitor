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

/// Phase 7 S1 "foundation" — per-thread turn lifecycle state.
///
/// The host-side single source of truth for (a) whether a thread currently
/// has an active turn (the alarm idle-check, future block) and (b) that
/// turn's id (the escape-hatch `turn/interrupt`). An entry's *presence* means
/// a turn is in flight; `clear_turn_active` removes the entry on the terminal
/// `turn/completed` (or a non-retryable `error`) notification.
#[derive(Debug, Clone)]
pub(crate) struct TurnState {
    pub(crate) active: bool,
    pub(crate) turn_id: Option<String>,
}

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
    pub(crate) background_thread_callbacks: Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
    /// `thread_id → fan-out tap senders`. Phase 2.C: sidecar's blocking
    /// `codex_send_user_message` registers a tap so it can drain the
    /// `agentMessage/delta` stream and break on `turn/completed`. Unlike
    /// `background_thread_callbacks`, taps run **alongside** the UI emit —
    /// they never suppress notifications from reaching the frontend.
    pub(crate) tap_thread_callbacks: Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Value>>>>,
    /// Phase 7 S1 — `thread_id → TurnState`. Lives here (not in the team-only
    /// `SharedRouterState`) so normal-mode threads AND `turn_interrupt_core`
    /// read the same source of truth. Written on the `turn/start` ack
    /// (`set_turn_active`) and the terminal `turn/completed` / non-retryable
    /// `error` notification (`clear_turn_active`).
    pub(crate) turn_state: Mutex<HashMap<String, TurnState>>,
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
            turn_state: Mutex::new(HashMap::new()),
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

    // ── Phase 7 S1 — per-thread turn-state helpers ──────────────────────

    /// Write① — record a freshly started turn. Called from the `turn/start`
    /// convergence point (`send_user_message_core`). A new turn supersedes any
    /// prior entry for the thread.
    pub(crate) async fn set_turn_active(&self, thread_id: String, turn_id: String) {
        self.turn_state.lock().await.insert(
            thread_id,
            TurnState {
                active: true,
                turn_id: Some(turn_id),
            },
        );
    }

    /// Write② — clear a thread's turn-state on a terminal notification.
    /// Reconciles by turn id: when `completed_turn_id` is `Some`, only clear
    /// if it matches the recorded turn id, so a late `turn/completed` from a
    /// superseded turn cannot wipe a turn that started after it. `None` (the
    /// non-retryable `error` fallback / thread archival) clears unconditionally.
    pub(crate) async fn clear_turn_active(&self, thread_id: &str, completed_turn_id: Option<&str>) {
        let mut map = self.turn_state.lock().await;
        let should_clear = match map.get(thread_id) {
            None => false,
            Some(state) => match completed_turn_id {
                None => true,
                Some(done) => match state.turn_id.as_deref() {
                    Some(current) => current == done,
                    None => true,
                },
            },
        };
        if should_clear {
            map.remove(thread_id);
        }
    }

    /// The active turn id for `thread_id`, if a turn is in flight. Source for
    /// the system-initiated escape hatch (`interrupt_thread_core`).
    pub(crate) async fn current_turn_id(&self, thread_id: &str) -> Option<String> {
        self.turn_state
            .lock()
            .await
            .get(thread_id)
            .and_then(|state| state.turn_id.clone())
    }

    /// Whether `thread_id` currently has an active turn. Source for the alarm
    /// idle-check (must not `turn/start` while active — that would
    /// `Replaced`-abort the in-flight turn).
    pub(crate) async fn is_active(&self, thread_id: &str) -> bool {
        self.turn_state
            .lock()
            .await
            .get(thread_id)
            .is_some_and(|state| state.active)
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
    let is_drive_path =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
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
            routing
                .workspace_roots
                .lock()
                .await
                .get("ws-other")
                .cloned(),
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
    async fn turn_state_set_then_matching_completion_clears() {
        let routing = SessionRouting::new("ws-1".to_string());
        assert!(!routing.is_active("t1").await);
        assert_eq!(routing.current_turn_id("t1").await, None);

        routing
            .set_turn_active("t1".to_string(), "turn-1".to_string())
            .await;
        assert!(routing.is_active("t1").await);
        assert_eq!(
            routing.current_turn_id("t1").await,
            Some("turn-1".to_string())
        );

        // A `turn/completed` whose id matches the recorded turn clears it.
        routing.clear_turn_active("t1", Some("turn-1")).await;
        assert!(!routing.is_active("t1").await);
        assert_eq!(routing.current_turn_id("t1").await, None);
    }

    #[tokio::test]
    async fn turn_state_stale_completion_does_not_clear_current() {
        let routing = SessionRouting::new("ws-1".to_string());
        routing
            .set_turn_active("t1".to_string(), "turn-2".to_string())
            .await;

        // A late `turn/completed` for a SUPERSEDED turn (turn-1) must not wipe
        // the freshly started turn-2.
        routing.clear_turn_active("t1", Some("turn-1")).await;
        assert!(routing.is_active("t1").await);
        assert_eq!(
            routing.current_turn_id("t1").await,
            Some("turn-2".to_string())
        );

        // The matching completion does clear it.
        routing.clear_turn_active("t1", Some("turn-2")).await;
        assert_eq!(routing.current_turn_id("t1").await, None);
    }

    #[tokio::test]
    async fn turn_state_error_fallback_clears_unconditionally() {
        let routing = SessionRouting::new("ws-1".to_string());
        routing
            .set_turn_active("t1".to_string(), "turn-3".to_string())
            .await;

        // The non-retryable `error` path passes `None` → clears regardless of id.
        routing.clear_turn_active("t1", None).await;
        assert!(!routing.is_active("t1").await);
        assert_eq!(routing.current_turn_id("t1").await, None);
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
        assert!(routing
            .resolve_workspace_for_thread("thr-a")
            .await
            .is_none());
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
        routing
            .register_background_callback("thr-bg".to_string(), tx)
            .await;
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
