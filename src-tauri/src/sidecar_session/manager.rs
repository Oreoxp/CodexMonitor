// Sidecar session manager — owns the workspace_id → SidecarSession map and
// serializes all sidecar lifecycle operations.
//
// Phase 1 Spike A scope: just a Mutex<HashMap>. No status events, no
// reconnect. `shutdown_all` (added in Phase 1.0 cleanup) is called on app exit
// so no `npx tsx` child is left orphaned when the user quits without calling
// `stop_sidecar`.
//
// Phase 1.1.A added idempotent + workspace-aware `start`. Phase 1.1.A follow-up
// added the `lifecycle` gate below: React StrictMode mounts an effect twice
// synchronously (effect → cleanup → effect), firing start → stop → start on the
// same workspace concurrently. Without serialization an unlucky interleaving
// leaves the sidecar dead (start sees a still-registered session and returns
// early, then the stop's kill lands). The gate makes each lifecycle op's
// check→spawn→register / find→unregister sequence atomic w.r.t. the others.

use std::collections::HashMap;
use std::sync::Arc;

use tauri::AppHandle;
use tokio::sync::Mutex;

use super::session::SidecarSession;

#[derive(Default)]
pub(crate) struct SidecarSessionManager {
    sessions: Mutex<HashMap<String, Arc<SidecarSession>>>,
    /// Serializes `start` / `stop` / `shutdown_all`. A single manager-wide gate
    /// (not per-workspace) — lifecycle ops are sparse (mode toggle / workspace
    /// switch / team create), never hot, so serializing across workspaces too
    /// costs nothing observable and also covers `start`'s cross-workspace
    /// teardown. Held only across map mutations + spawn / kill *initiation*;
    /// the child-exit wait inside `kill` runs after the gate is released.
    lifecycle: Mutex<()>,
}

impl SidecarSessionManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn get(&self, workspace_id: &str) -> Option<Arc<SidecarSession>> {
        self.sessions.lock().await.get(workspace_id).cloned()
    }

    /// Start (or reuse) the sidecar for `workspace_id`. Idempotent and
    /// workspace-aware: a tracked session is reused; otherwise every other
    /// workspace's sidecar is torn down and a fresh child is spawned + `init`ed.
    /// Serialized via the `lifecycle` gate.
    pub(crate) async fn start(
        &self,
        workspace_id: String,
        workspace_path: String,
        app_handle: AppHandle,
    ) -> Result<(), String> {
        // Sessions to hard-kill once the gate is released — their child-exit
        // wait must not block the gate.
        let to_kill: Vec<Arc<SidecarSession>>;
        let result: Result<(), String>;

        {
            let _gate = self.lifecycle.lock().await;

            // Idempotent: a tracked session is treated as healthy and reused.
            // Liveness probing is out of scope for Phase 1.1.A — the manager
            // map is the source of truth.
            if self.sessions.lock().await.contains_key(&workspace_id) {
                return Ok(());
            }

            // Workspace switch: pull every other workspace's session out of the
            // map now (a map op, under the gate); they are killed below, after
            // the gate is released. Sidecar state is per-workspace
            // (`state.sqlite` under `<cwd>/.opencrab/`), so only the active
            // workspace's sidecar should be live.
            let mut others: Vec<Arc<SidecarSession>> = {
                let mut map = self.sessions.lock().await;
                let stale: Vec<String> = map
                    .keys()
                    .filter(|id| id.as_str() != workspace_id)
                    .cloned()
                    .collect();
                stale.into_iter().filter_map(|id| map.remove(&id)).collect()
            };

            match SidecarSession::spawn(
                workspace_id.clone(),
                workspace_path.clone(),
                app_handle,
            )
            .await
            {
                Ok(session) => {
                    // Bind the sidecar to this workspace before releasing the
                    // gate so no other op can race the half-initialized child.
                    // `init` is a bounded control handshake (workspace_path
                    // only), not a model/work message — holding the gate across
                    // it is fine and is what keeps check→spawn→register atomic.
                    let init_params =
                        serde_json::json!({ "workspace_path": workspace_path });
                    match session.send_request("init", Some(init_params)).await {
                        Ok(_) => {
                            self.sessions
                                .lock()
                                .await
                                .insert(workspace_id.clone(), session);
                            to_kill = others;
                            result = Ok(());
                        }
                        Err(err) => {
                            // Init failed: the child was never registered, so
                            // kill it alongside the others, outside the gate.
                            others.push(session);
                            to_kill = others;
                            result = Err(format!("sidecar init failed: {err}"));
                        }
                    }
                }
                Err(err) => {
                    to_kill = others;
                    result = Err(err);
                }
            }
        } // gate released here

        for session in to_kill {
            session.kill().await;
        }
        result
    }

    /// Stop the sidecar for `workspace_id`. The map removal is done under the
    /// gate; the kill (which waits for child exit) runs after it is released —
    /// safe because the removed session Arc is no longer reachable by any other
    /// lifecycle op.
    pub(crate) async fn stop(&self, workspace_id: &str) -> Result<(), String> {
        let session = {
            let _gate = self.lifecycle.lock().await;
            self.sessions.lock().await.remove(workspace_id)
        }; // gate released here

        match session {
            Some(session) => {
                session.kill().await;
                Ok(())
            }
            None => Err(format!(
                "no sidecar running for workspace `{workspace_id}`"
            )),
        }
    }

    /// Drain the session map and hard-kill every sidecar child. Called on app
    /// exit so no `npx tsx` process is orphaned. Drains under the gate so a
    /// concurrent `start` cannot register a child that escapes teardown.
    pub(crate) async fn shutdown_all(&self) {
        let sessions: Vec<Arc<SidecarSession>> = {
            let _gate = self.lifecycle.lock().await;
            let mut map = self.sessions.lock().await;
            map.drain().map(|(_, session)| session).collect()
        }; // gate released here

        for session in sessions {
            session.kill().await;
        }
    }
}
