// Phase 2 pivot — Tauri-side team router.
//
// On `team_router_start` reverse-RPC from sidecar, we:
//   1. Replace any existing router for this workspace (drop old senders so
//      consumer tasks exit; remove old taps via SessionRouting helpers).
//   2. For each agent's bound Codex thread, register a permanent tap on
//      `SessionRouting::tap_thread_callbacks` and spawn a tokio consumer
//      task that loops over the tap's mpsc receiver.
//   3. The consumer accumulates `item/agentMessage/delta` text and on
//      `turn/completed` parses `<send_message to="..." channel="...">...
//      </send_message>` tags from the accumulated text. For each tag:
//         - ACL: subscriptions-driven publisher/subscribers/channels match
//           (USER_PUBLISHER `"user"` is treated as a regular topology node)
//         - target == "user": no Codex dispatch (user has no thread; the
//           tag remains visible in the sender's transcript stream)
//         - otherwise: framing `[From <sender_name>]\n<content>` and a
//           non-blocking `send_user_message_core` to the target's thread
//
// All routing data (agent roster, subscription topology) is supplied by the
// sidecar in the start RPC payload — Tauri does not re-read team.json.
//
// Hand-rolled string parser: `regex` is not in Cargo.toml and adding deps is
// out of scope. The grammar we need is trivially scannable in ~30 lines.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::shared::codex_core::{
    get_session_clone, resolve_workspace_path_core, send_user_message_core,
};
use crate::state::AppState;
use crate::tasks::{list_tasks_at_path, tasks_proposed_event, Task, TaskStatus};

use super::plan_parser::{parse_propose_plan_blocks, ParsedPlan};

const USER_PUBLISHER: &str = "user";

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct AgentInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(rename = "threadId")]
    pub(crate) thread_id: String,
    /// Phase 6 — Block C (inherited from P5) — `true` iff the sidecar just provisioned this
    /// agent's thread in the current call (it ran the kickoff with today's
    /// daily-memory prelude). `false` for resumed threads (bound from a
    /// previous session, no kickoff this session). Drives the initial value
    /// of `SharedRouterState.prelude_dates`. Defaults to `false` for back-
    /// compat with older sidecars that do not send the field.
    #[serde(rename = "freshlyProvisioned", default)]
    pub(crate) freshly_provisioned: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct Subscription {
    pub(crate) publisher: String,
    pub(crate) subscribers: Vec<String>,
    pub(crate) channels: Vec<String>,
}

struct ParsedTag {
    to: String,
    channel: String,
    content: String,
}

#[derive(Default)]
pub(crate) struct TeamRouters {
    // workspace_id → handles to spawned consumer tasks; replaced wholesale on
    // every team_router_start. The mpsc senders inside SessionRouting are
    // dropped via `unregister_tap_callback`, which causes each consumer's
    // `rx.recv().await` to return None and the task to exit.
    inner: Mutex<HashMap<String, WorkspaceRouter>>,
}

struct WorkspaceRouter {
    // Senders we registered so we can `unregister_tap_callback` them on
    // restart. Same handles that drive the consumer tasks.
    taps: Vec<(String, mpsc::UnboundedSender<Value>)>,
    tasks: Vec<JoinHandle<()>>,
    // Shared state that consumer tasks and Tauri commands (approve_task /
    // reject_task) reach into. Held here so the command path can locate
    // `pending_approvals` for this workspace.
    shared: Arc<SharedRouterState>,
}

impl TeamRouters {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn start(
        &self,
        app_handle: AppHandle,
        workspace_id: String,
        team_id: String,
        agents: Vec<AgentInfo>,
        subscriptions: Vec<Subscription>,
    ) -> Result<(), String> {
        // Tear down any pre-existing router for this workspace first. Order
        // matters: unregister taps (so dispatch stops fanning to old senders)
        // before dropping senders (so tasks see the channel close).
        self.stop(&app_handle, &workspace_id).await;

        let state = app_handle.state::<AppState>();
        let session = get_session_clone(&state.sessions, &workspace_id).await?;
        let routing = session.routing.clone();
        drop(state);

        let by_thread: HashMap<String, AgentInfo> = agents
            .iter()
            .cloned()
            .map(|a| (a.thread_id.clone(), a))
            .collect();
        let by_agent_id: HashMap<String, String> = agents
            .iter()
            .map(|a| (a.id.clone(), a.thread_id.clone()))
            .collect();
        let known_agent_ids: HashSet<String> = agents.iter().map(|a| a.id.clone()).collect();

        // Hydrate pending_approvals from the DB. Phase 1.3-style: any
        // `tasks.status='proposed'` row that survived a previous run is the
        // ground truth — restarting the workspace must reattach the latch
        // for those rows without the user having to re-touch the modal.
        let pending_seed = hydrate_pending_approvals(&app_handle, &workspace_id, &team_id).await;
        // Phase 6 — Block C (inherited from P5) — initial prelude-date state per thread.
        // Freshly-provisioned threads have today's prelude in their live
        // context (Step 1's kickoff injected it). Resumed threads start at
        // `None` so their next outgoing message re-injects.
        let today_local = chrono::Local::now().format("%Y-%m-%d").to_string();
        let prelude_init: HashMap<String, Option<String>> = agents
            .iter()
            .map(|a| {
                let initial = initial_prelude_date(a.freshly_provisioned, &today_local);
                (a.thread_id.clone(), initial)
            })
            .collect();
        let shared = Arc::new(SharedRouterState {
            workspace_id: workspace_id.clone(),
            team_id,
            by_thread,
            by_agent_id,
            known_agent_ids,
            subscriptions,
            app_handle: app_handle.clone(),
            prelude_dates: Mutex::new(prelude_init),
            pending_approvals: Mutex::new(pending_seed),
        });

        let mut taps = Vec::with_capacity(agents.len());
        let mut tasks = Vec::with_capacity(agents.len());
        for agent in agents {
            let (tx, rx) = mpsc::unbounded_channel::<Value>();
            routing
                .register_tap_callback(agent.thread_id.clone(), tx.clone())
                .await;
            taps.push((agent.thread_id.clone(), tx));
            let task = spawn_consumer(rx, agent.thread_id.clone(), shared.clone());
            tasks.push(task);
        }
        self.inner.lock().await.insert(
            workspace_id,
            WorkspaceRouter {
                taps,
                tasks,
                shared,
            },
        );
        Ok(())
    }

    pub(crate) async fn stop(&self, app_handle: &AppHandle, workspace_id: &str) {
        let removed = self.inner.lock().await.remove(workspace_id);
        let Some(router) = removed else { return };
        let state = app_handle.state::<AppState>();
        if let Ok(session) = get_session_clone(&state.sessions, workspace_id).await {
            for (thread_id, sender) in &router.taps {
                session
                    .routing
                    .unregister_tap_callback(thread_id, sender)
                    .await;
            }
        }
        drop(router);
    }

    /// Phase 6 — Block C (inherited from P5) — Re-injection intercept.
    ///
    /// Returns `text` unchanged when sending into a team thread whose live
    /// context already has today's daily-memory prelude. Otherwise fetches
    /// today's prelude from the sidecar (single source of truth — Step 1's
    /// `buildDailyMemoryPrelude` in `sidecar/src/prompt/daily-memory.ts`,
    /// reached via the `daily_memory_prelude` op rather than a second Rust
    /// implementation per the task brief), prepends it to `text`, and marks
    /// the thread's prelude fresh. The three re-injection triggers all reach
    /// this path through the `current_prelude_date == today` check:
    ///   1. `thread/compacted` cleared `prelude_dates[thread_id] = None`.
    ///   2. The thread's prelude was set on a previous local calendar day
    ///      (long thread crossed a day).
    ///   3. Resumed thread — initial `prelude_dates` value is `None`.
    ///
    /// Safe no-op for non-team threads / non-team workspaces: returns `text`
    /// unchanged. Soft-fails on sidecar RPC error (logs + returns text
    /// unchanged) — a missed re-injection means the agent answers without
    /// the latest prelude this once; the next outgoing message will retry.
    pub(crate) async fn prepend_prelude_if_stale(
        &self,
        app_handle: &AppHandle,
        workspace_id: &str,
        thread_id: &str,
        text: String,
    ) -> String {
        // Resolve the team router + the target agent. Bail with `text`
        // unchanged if the thread is not a team thread.
        let (shared, agent_id) = {
            let guard = self.inner.lock().await;
            let Some(router) = guard.get(workspace_id) else {
                return text;
            };
            let Some(agent) = router.shared.by_thread.get(thread_id) else {
                return text;
            };
            (router.shared.clone(), agent.id.clone())
        };

        // Fresh? Early-exit avoids the RPC round-trip when no re-injection
        // is needed. (`finalize_text_with_prelude` also handles the fresh
        // case below — keeping it covered there as a defence-in-depth — but
        // hitting the RPC for nothing would still be wasteful.)
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let current_date = shared
            .prelude_dates
            .lock()
            .await
            .get(thread_id)
            .cloned()
            .flatten();
        if !prelude_is_stale(current_date.as_deref(), &today) {
            return text;
        }

        // Stale — ask the sidecar for today's prelude (single source of
        // truth: Step 1's `buildDailyMemoryPrelude`). Any failure becomes
        // `Err(())`, which `finalize_text_with_prelude` turns into a soft
        // failure: the message goes through unprepended and the per-thread
        // accounting stays stale so the NEXT message retries the fetch. A
        // prelude-fetch failure MUST NEVER block a real user / agent message.
        let fetch_outcome: Result<String, ()> = match app_handle
            .state::<AppState>()
            .sidecar_sessions
            .get(workspace_id)
            .await
        {
            None => {
                eprintln!(
                    "[memory-prelude] thread={thread_id} prelude re-injection: no sidecar \
                     running for workspace {workspace_id}; sending without prelude"
                );
                Err(())
            }
            Some(session) => match session
                .send_request(
                    "daily_memory_prelude",
                    Some(serde_json::json!({ "agent_id": agent_id })),
                )
                .await
            {
                Err(err) => {
                    eprintln!(
                        "[memory-prelude] thread={thread_id} agent={agent_id} prelude RPC \
                         failed: {err}; sending without prelude"
                    );
                    Err(())
                }
                Ok(response) => Ok(response
                    .get("prelude")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()),
            },
        };

        let prelude_len = fetch_outcome.as_ref().ok().map(|p| p.len()).unwrap_or(0);
        let (new_text, new_date) =
            finalize_text_with_prelude(text, current_date.as_deref(), &today, fetch_outcome);
        if let Some(date) = new_date {
            shared
                .prelude_dates
                .lock()
                .await
                .insert(thread_id.to_string(), Some(date));
            if prelude_len > 0 {
                eprintln!(
                    "[memory-prelude] thread={thread_id} agent={agent_id} re-injected \
                     daily-memory prelude ({prelude_len} chars)"
                );
            }
        }
        new_text
    }

    /// S6-3a — record one human team message into the `chats` render-DB
    /// (`kind=user_input`). Producer side of chats user-input; the structured
    /// agent side lives in `process_team_tool_calls` (S6-2). Best-effort and
    /// **team threads only**: the same lookup as `prepend_prelude_if_stale`
    /// bails when this is not a team workspace / not a team thread, so normal
    /// solo chats never enter `chats`. Called from the human-input Tauri
    /// command (`codex::send_user_message`).
    pub(crate) async fn record_user_chat_best_effort(
        &self,
        workspace_id: &str,
        thread_id: &str,
        text: &str,
    ) {
        let (shared, agent_id) = {
            let guard = self.inner.lock().await;
            let Some(router) = guard.get(workspace_id) else {
                return;
            };
            let Some(agent) = router.shared.by_thread.get(thread_id) else {
                return;
            };
            (router.shared.clone(), agent.id.clone())
        };
        let chat = build_user_input_chat(
            &shared.workspace_id,
            &shared.team_id,
            thread_id,
            &agent_id,
            text,
            &chrono::Utc::now().to_rfc3339(),
        );
        append_chat_best_effort(&shared, chat).await;
    }
}

/// Build the `kind=user_input` chats row for a human team message: the user
/// sent `content` into `agent_id`'s thread, so it is recorded under that
/// thread with `recipient = agent_id` (mirrors how an agent's own
/// `send_message` row is keyed in `decide_send_message`). Pure, for testing.
fn build_user_input_chat(
    workspace_id: &str,
    team_id: &str,
    thread_id: &str,
    agent_id: &str,
    content: &str,
    ts: &str,
) -> crate::chats::store::NewChat {
    crate::chats::store::NewChat {
        workspace_id: workspace_id.to_string(),
        team_id: team_id.to_string(),
        thread_id: Some(thread_id.to_string()),
        sender: USER_PUBLISHER.to_string(),
        recipient: Some(agent_id.to_string()),
        role: None,
        kind: crate::chats::store::ChatKind::UserInput,
        content: content.to_string(),
        ts: ts.to_string(),
    }
}

struct SharedRouterState {
    workspace_id: String,
    team_id: String,
    by_thread: HashMap<String, AgentInfo>,
    by_agent_id: HashMap<String, String>,
    // Set of valid agent ids for this team — used by the propose_plan write
    // path to decide whether to clear an `assignee` that doesn't match a
    // teammate (write NULL + warn log instead of rejecting the plan).
    known_agent_ids: HashSet<String>,
    subscriptions: Vec<Subscription>,
    app_handle: AppHandle,
    /// Phase 6 — Block C (inherited from P5) — per-thread "which date's daily-memory prelude
    /// is currently in the thread's live context". `Some(d)` after a (re-)
    /// injection on date `d`; `None` after a `thread/compacted` cleared it,
    /// or initial-state for a resumed thread we did not just kickoff. The
    /// re-injection invariant is checked against this on every outgoing
    /// user-role message via `prepend_prelude_if_stale`.
    prelude_dates: Mutex<HashMap<String, Option<String>>>,
    // Step 3 approval-gate latch.
    //
    // Invariant: `pending_approvals.contains_key(task_id)` ↔
    //   the `tasks` row exists with `status='proposed'`. The DB is the source
    //   of truth (Phase 1.3-style hydration on `start()` rebuilds the map);
    //   the in-memory copy carries the `proposed_by_agent_id` + `title`
    //   metadata we need on the hot path (send_message ACL check + system
    //   reply text) without a per-message DB round-trip.
    //
    // Lifecycle: populated by `handle_propose_plan_blocks` after the batch
    // commits; drained by `mark_approved` / `mark_rejected` (the two paths
    // that flip a `proposed` row out of that status). `transition_task`
    // called directly bypasses this map — the state machine guard still
    // enforces correctness, but the in-memory latch goes stale until the
    // next router restart's hydration. Documented in §10 of the Step 3
    // report; revisit if direct transitions become a common flow.
    pending_approvals: Mutex<HashMap<String, PendingTaskInfo>>,
}

/// Per-task metadata we cache in `pending_approvals` so the hot path
/// (`sender_has_pending_approvals` predicate) doesn't need to re-query the
/// DB. Today we only need `proposed_by_agent_id` for the predicate; the
/// approve/reject paths read `title` from the `Task` returned by
/// `transition_task_at_path`, not from this map. If a future caller needs
/// title/body without a DB round-trip (e.g. a "show pending plans"
/// command), widen this struct then.
#[derive(Debug, Clone)]
struct PendingTaskInfo {
    proposed_by_agent_id: String,
}

/// Read all `status='proposed'` rows for `(workspace_id, team_id)` from the
/// `tasks` table and build the `pending_approvals` seed map. Failing to read
/// the DB returns an empty map + stderr warn — we do NOT want a corrupt /
/// missing DB to block router start; the team can still operate, just
/// without the latch reattached. (Router will rebuild on the next restart
/// once the DB comes back.)
async fn hydrate_pending_approvals(
    app_handle: &AppHandle,
    workspace_id: &str,
    team_id: &str,
) -> HashMap<String, PendingTaskInfo> {
    let state = app_handle.state::<AppState>();
    let workspace_path = resolve_workspace_path_core(&state.workspaces, workspace_id).await;
    drop(state);
    let workspace_path = match workspace_path {
        Ok(p) => std::path::PathBuf::from(p),
        Err(err) => {
            eprintln!(
                "[team-router] pending_approvals hydration: cannot resolve \
                 workspace path for {workspace_id}: {err}; latch will start empty"
            );
            return HashMap::new();
        }
    };
    let workspace_id_owned = workspace_id.to_string();
    let team_id_owned = team_id.to_string();
    let result = tokio::task::spawn_blocking(move || {
        list_tasks_at_path(
            &workspace_path,
            &workspace_id_owned,
            &team_id_owned,
            Some(&[TaskStatus::Proposed]),
        )
    })
    .await;
    match result {
        Ok(Ok(rows)) => {
            let mut map = HashMap::with_capacity(rows.len());
            for task in rows {
                map.insert(
                    task.id,
                    PendingTaskInfo {
                        proposed_by_agent_id: task.proposed_by_agent_id,
                    },
                );
            }
            map
        }
        Ok(Err(err)) => {
            eprintln!(
                "[team-router] pending_approvals hydration: list_tasks_at_path \
                 failed for ({workspace_id}, {team_id}): {err}; latch starts empty"
            );
            HashMap::new()
        }
        Err(join_err) => {
            eprintln!(
                "[team-router] pending_approvals hydration: spawn_blocking \
                 panicked: {join_err}; latch starts empty"
            );
            HashMap::new()
        }
    }
}

fn spawn_consumer(
    mut rx: mpsc::UnboundedReceiver<Value>,
    thread_id: String,
    shared: Arc<SharedRouterState>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = String::new();
        // S6-2 (dormant): structured opencrab-team tool calls observed during
        // this turn, flushed at turn/completed. Always empty until S6-3
        // registers the team MCP server.
        let mut tool_calls: Vec<TeamToolCall> = Vec::new();
        loop {
            let Some(event) = rx.recv().await else {
                // Channel closed — sidecar restarted the router (or workspace
                // tore down). Just exit.
                return;
            };
            let method = event.get("method").and_then(|m| m.as_str()).unwrap_or("");

            match method {
                "item/agentMessage/delta" => {
                    if let Some(delta) = event
                        .get("params")
                        .and_then(|p| p.get("delta"))
                        .and_then(|d| d.as_str())
                    {
                        buf.push_str(delta);
                    }
                }
                "turn/completed" => {
                    // Codex emits `turn/completed` for failed turns too — the
                    // failure surfaces as `params.turn.status == "failed"`
                    // and/or non-null `params.turn.error`.
                    let turn = event.get("params").and_then(|p| p.get("turn"));
                    let status = turn
                        .and_then(|t| t.get("status"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    let error = turn.and_then(|t| t.get("error"));
                    let has_error = error.map(|e| !e.is_null()).unwrap_or(false);

                    if status == "failed" || has_error {
                        // Grep-friendly trace for any turn failure; discard
                        // the buffer rather than route partial text.
                        eprintln!(
                            "[team_router] thread={} turn failed: status={} error={}",
                            thread_id,
                            status,
                            error
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "null".to_string())
                        );
                        buf.clear();
                        tool_calls.clear();
                    } else {
                        let final_text = std::mem::take(&mut buf);
                        process_final_text(&shared, &thread_id, &final_text).await;
                        // S6-2 (dormant): flush structured tool calls observed
                        // this turn. Empty until S6-3 registers the team MCP
                        // server, so this is a no-op in production today.
                        let turn_tool_calls = std::mem::take(&mut tool_calls);
                        process_team_tool_calls(&shared, &thread_id, turn_tool_calls).await;
                    }
                }
                "error" => {
                    // ErrorNotification (v2): wraps `TurnError` with a
                    // `willRetry` flag. `will_retry: true` is an intermediate
                    // retry-able stream error; `false` is terminal. Log both
                    // so silent "model returned nothing" patterns get caught
                    // in real time. See app-server-protocol/v2/notification.rs
                    // (`ErrorNotification`).
                    let params = event.get("params");
                    let will_retry = params
                        .and_then(|p| p.get("willRetry"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let err_payload = params
                        .and_then(|p| p.get("error"))
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "(no error payload)".to_string());
                    eprintln!(
                        "[team_router] thread={} turn error: will_retry={} error={}",
                        thread_id, will_retry, err_payload
                    );
                    if !will_retry {
                        buf.clear();
                        tool_calls.clear();
                    }
                }
                "turn/error" => {
                    // Legacy: Codex's current wire-format calls this `error`
                    // (handled above), not `turn/error`. Keep this arm so any
                    // upstream rename / split still surfaces in logs.
                    let payload = event
                        .get("params")
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "(no params)".to_string());
                    eprintln!(
                        "[team_router] thread={} legacy turn/error: {}",
                        thread_id, payload
                    );
                    buf.clear();
                    tool_calls.clear();
                }
                "thread/compacted" => {
                    // Phase 6 — Block C re-injection trigger. Codex compacted
                    // this thread (its own auto-compact, a frontend-triggered
                    // manual compact, or any future system-triggered
                    // compaction — the originator does not matter for this
                    // contract). See `mark_prelude_stale_on_compaction` for
                    // the load-bearing state op + the wider Block C contract.
                    mark_prelude_stale_on_compaction(&shared.prelude_dates, &thread_id).await;
                }
                "item/completed" => {
                    // S6-2 (dormant): buffer structured opencrab-team tool
                    // calls for flush at turn/completed. Non-team item/completed
                    // events return None and are ignored exactly as before.
                    if let Some(tc) = parse_team_tool_call(&event) {
                        tool_calls.push(tc);
                    }
                }
                _ => {}
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Phase 6 — Block C (inherited from P5) — daily-memory prelude re-injection
// (the rest of P5's write layer was deleted in P6 Step 3; this Block stays)
// ---------------------------------------------------------------------------

/// Phase 6 — Block C (inherited from P5) — pure date-comparison predicate for the re-
/// injection invariant. The three re-injection triggers all reach `true`
/// through this check:
///   1. Post-compaction: `thread/compacted` arm cleared the entry → `current
///      == None` → stale.
///   2. Long thread crossed a day: `current == Some(yesterday)` → stale.
///   3. Resumed thread: `prelude_dates` was initialised at `None` (no
///      freshly-provisioned kickoff this session) → stale.
fn prelude_is_stale(current_prelude_date: Option<&str>, today_local: &str) -> bool {
    current_prelude_date != Some(today_local)
}

/// Phase 6 — Block C — the body the `"thread/compacted"` arm runs. Clearing
/// `prelude_dates[thread] = None` is the load-bearing state op: it makes
/// `prelude_is_stale` flag `true` on the next outgoing message, which
/// `prepend_prelude_if_stale` then turns into a fresh prelude fetch + a
/// `compose_with_prelude` prepend.
///
/// Extracted so the real consumer arm and the unit-test that exercises
/// "compacted → next message re-injects" both bind to the same function —
/// a literal-copy in the test would silently drift if the arm body
/// changed. Takes the `prelude_dates` field directly (rather than the
/// whole `SharedRouterState`) so the test can drive it without faking
/// the rest of the router fixture.
async fn mark_prelude_stale_on_compaction(
    prelude_dates: &Mutex<HashMap<String, Option<String>>>,
    thread_id: &str,
) {
    eprintln!(
        "[team_router] thread={thread_id} thread/compacted; clearing prelude \
         date so next outgoing message re-injects today's prelude"
    );
    prelude_dates
        .lock()
        .await
        .insert(thread_id.to_string(), None);
}

/// Phase 6 — Block C (inherited from P5) — initial value of a thread's `prelude_dates` entry
/// when the team router starts. `freshly_provisioned` is the sidecar's
/// "this run kicked the agent off with today's prelude" signal — when true,
/// the live context has today's prelude and we start fresh; otherwise (a
/// resumed thread), we start `None` so the next outgoing message re-injects.
fn initial_prelude_date(freshly_provisioned: bool, today_local: &str) -> Option<String> {
    if freshly_provisioned {
        Some(today_local.to_string())
    } else {
        None
    }
}

/// Phase 6 — Block C (inherited from P5) — compose an outgoing message with a daily-
/// memory prelude. Empty prelude → text passes through unchanged (the legit
/// "agent has no log entries yet" case); non-empty → prelude + blank
/// line + text. Separated from `finalize_text_with_prelude` to pin the
/// exact `\n\n` separator under unit tests.
fn compose_with_prelude(prelude: &str, text: String) -> String {
    if prelude.is_empty() {
        text
    } else {
        format!("{prelude}\n\n{text}")
    }
}

/// Phase 6 — Block C (inherited from P5) — pure state-machine of
/// `prepend_prelude_if_stale`. Captures every decision the re-injection
/// intercept makes, so regression tests can drive it directly with mocked
/// fetch results.
///
/// Returns `(new_text, new_prelude_date)`:
///   * `new_prelude_date = None` → leave per-thread accounting unchanged.
///   * `new_prelude_date = Some(today)` → record this thread as having
///     today's prelude in its live context.
///
/// Branches:
///   * Fresh (current matches today) → `(text, None)` — invariant holds.
///   * Stale + RPC ok → `(maybe_prepended, Some(today))` — state updated
///     whether or not the prelude was non-empty (avoid re-fetching the
///     legitimately-empty prelude on every subsequent message).
///   * Stale + RPC err → `(text, None)` — **soft fail**: message goes
///     through unprepended and accounting stays stale so the NEXT outgoing
///     message retries the fetch. A prelude-fetch failure must never block
///     a real user/agent message.
fn finalize_text_with_prelude(
    text: String,
    current_prelude_date: Option<&str>,
    today_local: &str,
    fetch_result: Result<String, ()>,
) -> (String, Option<String>) {
    if !prelude_is_stale(current_prelude_date, today_local) {
        return (text, None);
    }
    match fetch_result {
        Err(()) => (text, None),
        Ok(prelude) => (
            compose_with_prelude(&prelude, text),
            Some(today_local.to_string()),
        ),
    }
}

async fn process_final_text(shared: &SharedRouterState, sender_thread: &str, text: &str) {
    let Some(sender) = shared.by_thread.get(sender_thread) else {
        // Tap fired for an unknown thread — nothing we can do with it.
        return;
    };

    // Phase 7 S1 — agent self-scheduling. A `<alarm>` re-arms this thread's
    // pending self-wake (one per thread; the last `<alarm>` of the turn wins).
    // Independent of send_message ACL — an alarm only targets the sender's own
    // thread — so it is processed even when the approval-gate below drops the
    // turn's send_message tags. The background scheduler fires it once the
    // thread is idle (see `crate::alarm`).
    let parsed_alarms = crate::alarm::parse_alarm_tags(text);
    if !parsed_alarms.is_empty() {
        let state = shared.app_handle.state::<AppState>();
        let mut reg = state.alarms.lock().expect("alarms mutex poisoned");
        for parsed in parsed_alarms {
            reg.insert(
                sender_thread.to_string(),
                crate::alarm::Alarm {
                    workspace_id: shared.workspace_id.clone(),
                    fire_at: std::time::Instant::now() + parsed.delay,
                    wake_message: parsed.wake_message,
                },
            );
        }
    }
    // Run both parsers over the same final-text buffer. Per Step 2 spec, we
    // make NO ordering assumption between propose_plan and send_message
    // within a single turn — Step 3 owns the gate / latch question. Step 2's
    // job is just "both parsers see the buffer, both side-effects fire."
    let plan_results = parse_propose_plan_blocks(text);
    if !plan_results.is_empty() {
        handle_propose_plan_blocks(shared, &sender.id, plan_results).await;
    }
    let tags = parse_send_message_tags(text);

    // Step 3 approval-gate latch (per-emitter granularity, mitigation b
    // from Phase 3 risk #2): if the *sender* has any task in
    // `pending_approvals` whose `proposed_by_agent_id == sender.id`, drop
    // all their send_message dispatches from this turn and reply once with
    // a system note. Per Step 2's in-turn ordering, any same-turn
    // `<propose_plan>` ran above and has already populated the latch.
    //
    // The check is hot-path-cheap: a single lock + linear scan of an
    // already-small map. We do it once per turn (not once per tag) and
    // short-circuit the whole dispatch loop on `true`.
    if !tags.is_empty() && sender_has_pending_approvals(shared, &sender.id).await {
        let pending_count = shared
            .pending_approvals
            .lock()
            .await
            .values()
            .filter(|info| info.proposed_by_agent_id == sender.id)
            .count();
        let body = format!(
            "[From system]\nYour turn produced {} <send_message> tag(s) but you currently \
             have {} task(s) pending user approval. Those dispatches were dropped \
             — you may not hand off work to teammates until the user approves the \
             pending plan(s). Wait for the approval result; on approve, you'll \
             receive a system note naming each task and you can continue from \
             there.",
            tags.len(),
            pending_count,
        );
        let _ = dispatch_system_reply(shared, &sender.id, body).await;
        return;
    }

    for tag in tags {
        if !can_send(
            &shared.subscriptions,
            &shared.known_agent_ids,
            &sender.id,
            &tag.to,
            &tag.channel,
        ) {
            eprintln!(
                "[team-router] send_message ACL denied: {} → {} on channel \"{}\"",
                sender.id, tag.to, tag.channel
            );
            continue;
        }
        if tag.to == USER_PUBLISHER {
            // The user sees the tag as part of the sender's transcript stream;
            // no Codex thread to dispatch to. Routing succeeds silently.
            continue;
        }
        let Some(target_thread) = shared.by_agent_id.get(&tag.to).cloned() else {
            eprintln!(
                "[team-router] send_message target {} has no bound thread (provisioning gap?)",
                tag.to
            );
            continue;
        };
        let framed = format!("[From {}]\n{}", sender.name, tag.content);
        // Phase 6 — Block C (inherited from P5) — re-injection: if the target thread's
        // daily-memory prelude has gone stale (post-compaction, cross-day,
        // or first send into a resumed thread), prepend today's prelude
        // here so the target agent answers with current memory in its live
        // context. Safe no-op for non-team threads / non-team workspaces.
        let team_routers = {
            let state = shared.app_handle.state::<AppState>();
            state.team_routers.clone()
        };
        let framed = team_routers
            .prepend_prelude_if_stale(
                &shared.app_handle,
                &shared.workspace_id,
                &target_thread,
                framed,
            )
            .await;
        let state = shared.app_handle.state::<AppState>();
        let dispatch = send_user_message_core(
            &state.sessions,
            &state.workspaces,
            shared.workspace_id.clone(),
            target_thread.clone(),
            framed,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        drop(state);
        if let Err(err) = dispatch {
            eprintln!(
                "[team-router] dispatch to {} ({}) failed: {}",
                tag.to, target_thread, err
            );
        }
    }
}

// ===========================================================================
// Phase 7 S6-2 — structured tool-call path (DORMANT until S6-3).
//
// The counterpart to `process_final_text` (the text-tag path) for the
// structured `send_message` / `propose_plan` MCP tools. It graduates the eval
// harness's `scan_mcp_tool_calls` observer
// (`bin/opencrab-eval/assertions.rs`) to a production-reachable place, then
// reuses the EXISTING routing chain unchanged — `can_send` ACL → approval
// gate → `[From X]` framing → `send_user_message_core` for send_message, and
// `handle_propose_plan_blocks` for propose_plan — and records each routed
// message into the S6-1 `chats` read-DB.
//
// DORMANT: S6-2 does NOT register the `opencrab-team` MCP server and does NOT
// change the comm guide (both land atomically in S6-3). So the model never
// emits these tool calls, `parse_team_tool_call` returns `None` for every
// real event, the consumer's tool buffer stays empty, and
// `process_team_tool_calls` is a no-op. Zero production behaviour change.
//
// (The whole chain is wired into the live consumer, so it is reachable code —
// no `#[allow(dead_code)]` needed; the dormancy is runtime — empty input —
// not compile-time unreachability.)
// ===========================================================================

/// The MCP server name the team tools are hosted under (the stub binary
/// `opencrab-team-mcp`; registered in S6-3). Only tool calls from this server
/// are routed here.
const TEAM_MCP_SERVER: &str = "opencrab-team";

/// One observed `opencrab-team` MCP tool call lifted off the per-thread tap.
struct TeamToolCall {
    tool: String,
    arguments: Value,
}

/// `send_message` tool arguments. Note `body` — the structured counterpart of
/// the text-tag's inner `content` (the rename the S6-2 mapping bridges).
#[derive(Debug, Deserialize)]
struct SendMessageArgs {
    to: String,
    channel: String,
    body: String,
}

/// `propose_plan` tool arguments — the structured counterpart of a
/// `<propose_plan>` block.
#[derive(Debug, Deserialize)]
struct ProposePlanArgs {
    tasks: Vec<ProposePlanTaskArg>,
}

#[derive(Debug, Deserialize)]
struct ProposePlanTaskArg {
    title: String,
    body: String,
    #[serde(default)]
    assignee: Option<String>,
}

/// Plain-ref routing context for `decide_send_message`, so the decision is
/// unit-testable without constructing a `SharedRouterState` (which needs a
/// live Tauri `AppHandle`). Mirrors the fields `process_final_text` reads.
struct RouteCtx<'a> {
    subscriptions: &'a [Subscription],
    known_agent_ids: &'a HashSet<String>,
    by_agent_id: &'a HashMap<String, String>,
    workspace_id: &'a str,
    team_id: &'a str,
}

/// What to do with one observed `send_message` tool call. The async caller
/// (`process_team_tool_calls`) executes the side effects; this decision is
/// pure so the ACL / approval-gate / target-resolution / chats-row logic is
/// testable with synthetic input.
enum SendMessageOutcome {
    /// `can_send` denied — dropped, nothing recorded.
    AclDenied,
    /// Sender has a plan awaiting approval — dropped, system note sent.
    ApprovalGated,
    /// `to == "user"`: no Codex thread, but recorded so the PM↔user
    /// conversation renders from `chats` (the structured path does not leak
    /// the message into the visible transcript the way the text tag did).
    UserTarget { chat: crate::chats::store::NewChat },
    /// Route the framed body to a teammate's thread, and record it.
    Dispatch {
        target_thread: String,
        framed: String,
        chat: crate::chats::store::NewChat,
    },
    /// Recipient is a known teammate but has no bound thread — dropped.
    NoTargetThread,
}

/// Graduated from the eval harness's `scan_mcp_tool_calls`
/// (`bin/opencrab-eval/assertions.rs`): recognize ONE `item/completed`
/// notification carrying a `mcpToolCall` item from the `opencrab-team`
/// server, and lift `(tool, arguments)`. `None` for everything else —
/// non-completed items, non-MCP items, other servers (e.g. `opencrab-memory`)
/// — which is every event in production today, since the team MCP server is
/// unregistered until S6-3 (this path is dormant).
fn parse_team_tool_call(event: &Value) -> Option<TeamToolCall> {
    if event.get("method").and_then(Value::as_str) != Some("item/completed") {
        return None;
    }
    let item = event.get("params")?.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("mcpToolCall") {
        return None;
    }
    if item.get("server").and_then(Value::as_str) != Some(TEAM_MCP_SERVER) {
        return None;
    }
    let tool = item.get("tool").and_then(Value::as_str)?.to_string();
    let arguments = item.get("arguments").cloned().unwrap_or(Value::Null);
    Some(TeamToolCall { tool, arguments })
}

/// Split observed tool calls into (propose_plan args, send_message args),
/// preserving relative order within each bucket. Draining the plan bucket
/// fully before the message bucket is how the approve-before-dispatch
/// invariant is guaranteed at the turn boundary (a plan latched this turn
/// gates a message sent the same turn) — the structured analogue of
/// `process_final_text` running `parse_propose_plan_blocks` before
/// `parse_send_message_tags`, so the ordering does NOT depend on the order
/// the tool-call events happen to arrive. Unknown tools are logged + dropped.
fn partition_tool_calls(calls: Vec<TeamToolCall>) -> (Vec<Value>, Vec<Value>) {
    let mut plans = Vec::new();
    let mut msgs = Vec::new();
    for c in calls {
        match c.tool.as_str() {
            "propose_plan" => plans.push(c.arguments),
            "send_message" => msgs.push(c.arguments),
            other => eprintln!("[team-router] ignoring unknown opencrab-team tool: {other}"),
        }
    }
    (plans, msgs)
}

/// Map `propose_plan` tool args → the existing `ParsedPlan` shape so the
/// unchanged `handle_propose_plan_blocks` writer can consume it.
fn propose_plan_args_to_plan(args: ProposePlanArgs) -> ParsedPlan {
    ParsedPlan {
        tasks: args
            .tasks
            .into_iter()
            .map(|t| super::plan_parser::ParsedPlanTask {
                title: t.title,
                assignee: t.assignee,
                body: t.body,
            })
            .collect(),
    }
}

/// Decide what to do with one `send_message` tool call. Pure: mirrors the
/// per-tag body of `process_final_text` (approval gate → `can_send` → user
/// short-circuit → target resolution → framing) over structured args, and
/// emits the `chats` row for the routed cases. `has_pending_approval` is
/// computed by the async caller from `sender_has_pending_approvals`.
fn decide_send_message(
    ctx: &RouteCtx<'_>,
    sender: &AgentInfo,
    args: &SendMessageArgs,
    has_pending_approval: bool,
    ts: &str,
) -> SendMessageOutcome {
    // Approval gate first — mirrors process_final_text's pre-loop latch check:
    // a sender with a plan awaiting approval may not hand off work.
    if has_pending_approval {
        return SendMessageOutcome::ApprovalGated;
    }
    // Map tool args → the existing ParsedTag shape (note `body` → `content`).
    let tag = ParsedTag {
        to: args.to.clone(),
        channel: args.channel.clone(),
        content: args.body.clone(),
    };
    if !can_send(
        ctx.subscriptions,
        ctx.known_agent_ids,
        &sender.id,
        &tag.to,
        &tag.channel,
    ) {
        return SendMessageOutcome::AclDenied;
    }
    let chat = crate::chats::store::NewChat {
        workspace_id: ctx.workspace_id.to_string(),
        team_id: ctx.team_id.to_string(),
        thread_id: Some(sender.thread_id.clone()),
        sender: sender.id.clone(),
        recipient: Some(tag.to.clone()),
        role: None,
        kind: crate::chats::store::ChatKind::SendMessage,
        content: tag.content.clone(),
        ts: ts.to_string(),
    };
    if tag.to == USER_PUBLISHER {
        return SendMessageOutcome::UserTarget { chat };
    }
    let Some(target_thread) = ctx.by_agent_id.get(&tag.to).cloned() else {
        return SendMessageOutcome::NoTargetThread;
    };
    let framed = format!("[From {}]\n{}", sender.name, tag.content);
    SendMessageOutcome::Dispatch {
        target_thread,
        framed,
        chat,
    }
}

/// Append a chat row best-effort: `chats` is a render layer, never a source of
/// truth, so a write failure logs and is swallowed — it must never fail the
/// dispatch it accompanies. Runs the `!Send` rusqlite work in `spawn_blocking`.
async fn append_chat_best_effort(shared: &SharedRouterState, chat: crate::chats::store::NewChat) {
    let Some(root) = resolve_workspace_path(shared).await else {
        return;
    };
    match tokio::task::spawn_blocking(move || crate::chats::store::append_chat_at_path(&root, chat))
        .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => eprintln!("[team-router] chats append failed: {err}"),
        Err(err) => eprintln!("[team-router] chats append task panicked: {err}"),
    }
}

/// Structured-tool turn handler — the counterpart to `process_final_text`,
/// called at `turn/completed` with the tool calls buffered during the turn.
/// DORMANT: `calls` is always empty until S6-3 registers the team MCP server.
async fn process_team_tool_calls(
    shared: &SharedRouterState,
    sender_thread: &str,
    calls: Vec<TeamToolCall>,
) {
    if calls.is_empty() {
        return;
    }
    let Some(sender) = shared.by_thread.get(sender_thread).cloned() else {
        // Tap fired for an unknown thread — nothing we can do with it.
        return;
    };

    // Plans FIRST, then messages — guarantees approve-before-dispatch within a
    // turn (a plan latched here gates a same-turn message below).
    let (plans, msgs) = partition_tool_calls(calls);

    for args_val in plans {
        match serde_json::from_value::<ProposePlanArgs>(args_val) {
            Ok(args) => {
                let plan = propose_plan_args_to_plan(args);
                handle_propose_plan_blocks(shared, &sender.id, vec![Ok(plan)]).await;
            }
            Err(err) => eprintln!("[team-router] tool propose_plan: bad args: {err}"),
        }
    }

    // S6-3b (D) — compute the approval-gate state ONCE per turn (the plans
    // above already latched; nothing in the message loop un-latches), and merge
    // all gated drops into ONE system note after the loop — matching the
    // text-tag gate's single per-turn reply rather than one note per dropped
    // message (S6-2 carried this as a per-message divergence).
    let has_pending = sender_has_pending_approvals(shared, &sender.id).await;
    let mut gated_count = 0usize;

    for args_val in msgs {
        let args: SendMessageArgs = match serde_json::from_value(args_val) {
            Ok(a) => a,
            Err(err) => {
                eprintln!("[team-router] tool send_message: bad args: {err}");
                continue;
            }
        };
        let ts = chrono::Utc::now().to_rfc3339();
        let ctx = RouteCtx {
            subscriptions: &shared.subscriptions,
            known_agent_ids: &shared.known_agent_ids,
            by_agent_id: &shared.by_agent_id,
            workspace_id: &shared.workspace_id,
            team_id: &shared.team_id,
        };
        match decide_send_message(&ctx, &sender, &args, has_pending, &ts) {
            SendMessageOutcome::AclDenied => {
                eprintln!(
                    "[team-router] tool send_message ACL denied: {} → {} on channel \"{}\"",
                    sender.id, args.to, args.channel
                );
            }
            SendMessageOutcome::ApprovalGated => {
                // Per-turn merge: count now, send ONE reply after the loop (D).
                gated_count += 1;
            }
            SendMessageOutcome::NoTargetThread => {
                eprintln!(
                    "[team-router] tool send_message target {} has no bound thread \
                     (provisioning gap?)",
                    args.to
                );
            }
            SendMessageOutcome::UserTarget { chat } => {
                // No Codex thread to dispatch to; record for the chat UI only.
                append_chat_best_effort(shared, chat).await;
            }
            SendMessageOutcome::Dispatch {
                target_thread,
                framed,
                chat,
            } => {
                append_chat_best_effort(shared, chat).await;
                // Reuse the exact prelude-prepend + dispatch chain
                // process_final_text uses.
                let framed = {
                    let team_routers = {
                        let state = shared.app_handle.state::<AppState>();
                        state.team_routers.clone()
                    };
                    team_routers
                        .prepend_prelude_if_stale(
                            &shared.app_handle,
                            &shared.workspace_id,
                            &target_thread,
                            framed,
                        )
                        .await
                };
                let state = shared.app_handle.state::<AppState>();
                let dispatch = send_user_message_core(
                    &state.sessions,
                    &state.workspaces,
                    shared.workspace_id.clone(),
                    target_thread.clone(),
                    framed,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await;
                drop(state);
                if let Err(err) = dispatch {
                    eprintln!(
                        "[team-router] tool dispatch to {} ({}) failed: {}",
                        args.to, target_thread, err
                    );
                }
            }
        }
    }

    // S6-3b (D) — one merged approval-gate note for the whole turn.
    if let Some(body) = build_gated_reply(gated_count) {
        let _ = dispatch_system_reply(shared, &sender.id, body).await;
    }
}

/// Merged approval-gate reply for a turn: `None` when nothing was gated, else
/// ONE `[From system]` note covering all `gated` dropped messages — per-turn,
/// matching the text-tag gate's single reply rather than one note per dropped
/// message (S6-2 carried this as a per-message divergence; S6-3b converges it).
fn build_gated_reply(gated: usize) -> Option<String> {
    if gated == 0 {
        return None;
    }
    Some(format!(
        "[From system]\n{gated} message(s) you sent this turn were dropped — you have \
         task(s) pending user approval. Wait for the approval result before handing off \
         work to teammates.",
    ))
}

/// Wire shape of the `tasks-proposed` Tauri event payload. Stays here next
/// to its emission site so anyone evolving the event surface only has to
/// look in one place; the matching TS type lives in
/// `CodexMonitor/src/features/team-mode/types/tasks.ts`.
///
/// `schema_version` is the wire-stable contract pinned at Phase 3 closeout
/// (#19). See `tasks::tasks_proposed_event::CURRENT_SCHEMA_VERSION` for
/// the bump policy.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TasksProposedEvent {
    pub(crate) schema_version: u32,
    pub(crate) workspace_id: String,
    pub(crate) team_id: String,
    pub(crate) proposed_by_agent_id: String,
    pub(crate) tasks: Vec<crate::tasks::Task>,
}

async fn handle_propose_plan_blocks(
    shared: &SharedRouterState,
    sender_id: &str,
    blocks: Vec<Result<ParsedPlan, super::plan_parser::PlanParseError>>,
) {
    let workspace_path = {
        let state = shared.app_handle.state::<AppState>();
        let path = resolve_workspace_path_core(&state.workspaces, &shared.workspace_id).await;
        drop(state);
        match path {
            Ok(p) => std::path::PathBuf::from(p),
            Err(err) => {
                eprintln!(
                    "[team-router] propose_plan: cannot resolve workspace path \
                     for {}: {err}",
                    shared.workspace_id
                );
                return;
            }
        }
    };

    for (idx, block) in blocks.into_iter().enumerate() {
        let plan = match block {
            Ok(plan) => plan,
            Err(err) => {
                eprintln!(
                    "[team-router] propose_plan: block #{idx} from agent {sender_id} \
                     failed to parse ({err}); dropping whole block and notifying author"
                );
                send_plan_retry_hint(shared, sender_id, &err.to_string()).await;
                continue;
            }
        };
        if plan.tasks.is_empty() {
            // Empty plan is not strictly malformed, but it also has no
            // side-effect to perform. Surface a log line so a future debug
            // session can correlate "agent emitted plan tag but DB stays
            // empty" without hunting through Codex transcripts.
            eprintln!(
                "[team-router] propose_plan: block #{idx} from agent {sender_id} \
                 had 0 tasks; nothing written"
            );
            continue;
        }
        // Build the batch of NewTask rows. assignee-not-in-team => NULL + warn
        // (per spec: do NOT reject the plan; preserves the distinction between
        // "agent typed an unknown name" and "assignee not yet decided").
        let new_tasks: Vec<crate::tasks::NewTask> = plan
            .tasks
            .into_iter()
            .map(|t| {
                let assignee = match t.assignee {
                    None => None,
                    Some(a) if shared.known_agent_ids.contains(&a) => Some(a),
                    Some(a) => {
                        eprintln!(
                            "[team-router] propose_plan: assignee \"{a}\" not in team — \
                             writing NULL (Step 4 modal will collect)"
                        );
                        None
                    }
                };
                crate::tasks::NewTask {
                    id: uuid::Uuid::new_v4().to_string(),
                    workspace_id: shared.workspace_id.clone(),
                    team_id: shared.team_id.clone(),
                    assignee_agent_id: assignee,
                    proposed_by_agent_id: sender_id.to_string(),
                    title: t.title,
                    body: t.body,
                }
            })
            .collect();

        let workspace_id = shared.workspace_id.clone();
        let team_id = shared.team_id.clone();
        let sender_id_owned = sender_id.to_string();
        let workspace_path_clone = workspace_path.clone();
        // `rusqlite::Connection` is `!Send`, so all sqlite work runs inside
        // `spawn_blocking`. We hand the batch to a closure that opens the DB,
        // runs the transaction, and returns the persisted Tasks (already in
        // a `Send`-friendly shape).
        let persisted = tokio::task::spawn_blocking(move || {
            crate::tasks::insert_proposed_batch_at_path(&workspace_path_clone, new_tasks)
        })
        .await;
        let persisted = match persisted {
            Ok(Ok(v)) => v,
            Ok(Err(err)) => {
                eprintln!("[team-router] propose_plan: insert_proposed_batch failed: {err}");
                continue;
            }
            Err(join_err) => {
                eprintln!(
                    "[team-router] propose_plan: insert_proposed_batch task panicked: \
                     {join_err}"
                );
                continue;
            }
        };

        // Register the new tasks in the approval-gate latch BEFORE we emit
        // the Tauri event. Ordering matters: the event signals "these task
        // ids exist as proposed"; if the consumer (Step 4 modal) calls
        // back into `approve_task` after seeing the event, we want the
        // latch to already contain the id so the removal is meaningful.
        {
            let mut guard = shared.pending_approvals.lock().await;
            for task in &persisted {
                guard.insert(
                    task.id.clone(),
                    PendingTaskInfo {
                        proposed_by_agent_id: task.proposed_by_agent_id.clone(),
                    },
                );
            }
        }

        // Phase 4 Step 6 — append a `task_proposed` event per row to
        // `<cwd>/.opencrab/teams/<team_id>/events.jsonl`. Best-effort:
        // an open / write failure is logged but never aborts the
        // proposal (the row is already committed and the latch is
        // already populated). Step 1's `ensure_project_layer` is
        // responsible for the parent dirs + empty placeholder file.
        {
            let state = shared.app_handle.state::<AppState>();
            match crate::events::get_or_create_event_log(
                &state,
                &workspace_path,
                &shared.workspace_id,
                &shared.team_id,
            ) {
                Ok(log) => {
                    for task in &persisted {
                        if let Err(err) = log.emit(
                            Some(&task.id),
                            crate::events::TeamEventBody::TaskProposed {
                                agent_id: task.proposed_by_agent_id.clone(),
                                title: task.title.clone(),
                                description: task.body.clone(),
                            },
                        ) {
                            eprintln!(
                                "[events] task_proposed emit failed for task={}: {err}",
                                task.id
                            );
                        }
                    }
                }
                Err(err) => {
                    eprintln!(
                        "[events] cannot open log for team={}: {err}; \
                         skipping {} task_proposed events",
                        shared.team_id,
                        persisted.len()
                    );
                }
            }
            drop(state);
        }

        // Phase 4 Step 7 — re-render every agent's KANBAN.md so the
        // freshly-proposed tasks show up in the Proposed column on the
        // next prompt assembly. Best-effort: a render / write failure
        // logs to stderr; the rows are already committed and the
        // events are already appended.
        if let Err(err) = crate::kanban::regenerate_all_kanbans(
            &workspace_path,
            &shared.workspace_id,
            &shared.team_id,
        ) {
            eprintln!(
                "[kanban] regenerate skipped for workspace={} team={}: {err}",
                shared.workspace_id, shared.team_id
            );
        }

        // Emit a Tauri event so the (Step 4) plan-review modal can react.
        // The event NAME is the single Step-2 contract the frontend will
        // pin against; payload fields can grow additively.
        let payload = TasksProposedEvent {
            schema_version: tasks_proposed_event::CURRENT_SCHEMA_VERSION,
            workspace_id,
            team_id,
            proposed_by_agent_id: sender_id_owned,
            tasks: persisted,
        };
        if let Err(err) = shared.app_handle.emit(tasks_proposed_event::NAME, &payload) {
            eprintln!(
                "[team-router] propose_plan: emit `{}` failed: {err}",
                tasks_proposed_event::NAME
            );
        }
    }
}

async fn sender_has_pending_approvals(shared: &SharedRouterState, sender_id: &str) -> bool {
    let guard = shared.pending_approvals.lock().await;
    map_has_proposer(&guard, sender_id)
}

/// Pure predicate: does any entry in `map` name `sender_id` as its
/// `proposed_by_agent_id`? Extracted from `sender_has_pending_approvals`
/// for unit testing without an async / Mutex shell.
fn map_has_proposer(map: &HashMap<String, PendingTaskInfo>, sender_id: &str) -> bool {
    map.values()
        .any(|info| info.proposed_by_agent_id == sender_id)
}

/// Dispatch a one-shot system-style framed message back to one agent's
/// Codex thread. Uses the same `[From <name>]\n<body>` framing as
/// send_message but with sender_name "system" so the prose is visibly
/// distinct from a teammate message.
///
/// This is best-effort: if the target's thread can't accept a turn right
/// now (e.g. the user just sent it something else, or a previous turn is
/// still draining), the error is logged and we return `Err` — the caller
/// decides what to do. The Step 2 retry-hint path discards the error
/// (best-effort); the Step 3 approve/reject path surfaces it via the
/// `pm_notified=false` field on `ApprovalResult`.
/// Resolve a workspace_id to its filesystem root via AppState. Returns
/// `None` (with a stderr log) on lookup failure. Pulled out of the
/// notify_resolved path because the same lookup is needed for both the
/// "still in progress?" predicate query and the "list all rows" merged-
/// message build.
async fn resolve_workspace_path(shared: &SharedRouterState) -> Option<std::path::PathBuf> {
    let state = shared.app_handle.state::<AppState>();
    let path = resolve_workspace_path_core(&state.workspaces, &shared.workspace_id).await;
    drop(state);
    match path {
        Ok(p) => Some(std::path::PathBuf::from(p)),
        Err(err) => {
            eprintln!(
                "[team-router] cannot resolve workspace path for {}: {err}",
                shared.workspace_id
            );
            None
        }
    }
}

/// Build the plan-completion merged system message. Rebuilt from the DB
/// every time, so a restart mid-review followed by the final resolve
/// produces a complete message even if some resolves happened pre-restart.
///
/// Wire shape (Phase 3 bug fix, §E):
///   [From system]
///   Plan review complete. N task(s) approved, M rejected.
///
///   Approved:
///   - "<title>" (id=<id>) — now ready, assignee=<assignee or "unassigned">
///   - ...
///
///   Rejected:
///   - "<title>" (id=<id>): <feedback or "(no feedback provided)">
///   - ...
///
///   You may now dispatch the approved tasks via <send_message>.
///
/// Section pruning:
///   - all-approved → omit "Rejected:" section
///   - all-rejected → omit "Approved:" section + omit the trailing
///     "You may now dispatch..." sentence (nothing to dispatch)
///   - empty (defensive — shouldn't fire) → returns a one-line warning
fn build_plan_completion_message(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "[From system]\nPlan review complete, but no tasks were found in the plan. \
                This is a defensive log; please re-propose the plan if you expected tasks here."
            .to_string();
    }
    let approved: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Ready)
        .collect();
    let rejected: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Archived)
        .collect();
    let mut out = String::new();
    out.push_str("[From system]\n");
    out.push_str(&format!(
        "Plan review complete. {} task(s) approved, {} rejected.\n",
        approved.len(),
        rejected.len()
    ));
    if !approved.is_empty() {
        out.push_str("\nApproved:\n");
        for t in &approved {
            let assignee = t.assignee_agent_id.as_deref().unwrap_or("unassigned");
            out.push_str(&format!(
                "- \"{title}\" (id={id}) — now ready, assignee={assignee}\n",
                title = t.title,
                id = t.id,
            ));
        }
    }
    if !rejected.is_empty() {
        out.push_str("\nRejected:\n");
        for t in &rejected {
            let fb = t
                .feedback
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("(no feedback provided)");
            out.push_str(&format!(
                "- \"{title}\" (id={id}): {fb}\n",
                title = t.title,
                id = t.id,
            ));
        }
    }
    if !approved.is_empty() {
        out.push_str("\nYou may now dispatch the approved tasks via <send_message>.\n");
    }
    // Trim trailing newline so the body doesn't end with a blank line.
    out.trim_end().to_string()
}

async fn dispatch_system_reply(
    shared: &SharedRouterState,
    target_agent_id: &str,
    body: String,
) -> Result<(), String> {
    let Some(thread_id) = shared.by_agent_id.get(target_agent_id).cloned() else {
        return Err(format!(
            "agent {target_agent_id} has no bound thread (provisioning gap?)"
        ));
    };
    let state = shared.app_handle.state::<AppState>();
    let result = send_user_message_core(
        &state.sessions,
        &state.workspaces,
        shared.workspace_id.clone(),
        thread_id.clone(),
        body,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    drop(state);
    result.map(|_| ()).map_err(|err| {
        eprintln!("[team-router] system reply to {target_agent_id} ({thread_id}) failed: {err}");
        err
    })
}

async fn send_plan_retry_hint(shared: &SharedRouterState, sender_id: &str, reason: &str) {
    let body = format!(
        "[From system]\nYour <propose_plan> block failed to parse ({reason}). \
         The block was dropped and no tasks were recorded. Please retry — see \
         the team communication protocol in your developer instructions for \
         the expected `<propose_plan><task title=\"...\" assignee=\"...\">...\
         </task></propose_plan>` shape."
    );
    let _ = dispatch_system_reply(shared, sender_id, body).await;
}

// ---------------------------------------------------------------------------
// Step 3 + Phase 3 bug fix (2026-05-17): approve_task / reject_task
// router-side handlers with plan-level batching.
//
// `notify_approved` / `notify_rejected` run AFTER the state-machine
// transition has already succeeded at the DB layer. They:
//   1. Drop the resolved task from `pending_approvals`.
//   2. Check whether the task carries a `plan_id`.
//      - None (legacy row pre-fix): send a per-task system message
//        immediately. Strict back-compat.
//      - Some(plan_id): query the DB; if ANY task in `plan_id` still has
//        `status='proposed'`, send NO system message (the plan is still
//        being reviewed). If every task is resolved, build ONE merged
//        plan-completion message from all rows in the plan (approved +
//        rejected, with persisted feedback) and dispatch it.
//
// Restart-mid-review: status + feedback are durable. Hydration rebuilds
// `pending_approvals`. The merged message rebuilds from the DB on the
// final approve/reject — no in-memory tracking, no plan-progress map.
//
// pm_notified semantics (changed by this fix): for plan rows, returns
// `true` when the resolve is durable AND either (a) the plan is still
// in progress so no message was needed, or (b) the plan completed and
// the merged message dispatch succeeded. Returns `false` ONLY when the
// final merged-message dispatch fails (target Codex thread busy, etc).
// Per-intermediate-resolve `false` is no longer possible. See §7 of the
// closeout report. For legacy NULL-plan_id rows the old per-task
// semantics apply.
// ---------------------------------------------------------------------------

impl TeamRouters {
    /// Resolve a task as approved. See module-level doc comment above for
    /// the plan-level batching semantics.
    pub(crate) async fn notify_approved(&self, workspace_id: &str, task: &Task) -> bool {
        self.notify_resolved(workspace_id, task).await
    }

    /// Resolve a task as rejected. See module-level doc comment above.
    /// `feedback` is NOT used by this function directly — the reject
    /// command (`tasks::commands::reject_task`) is responsible for
    /// persisting it to the DB via `transition_task_with_feedback_at_path`
    /// before calling here. The parameter is preserved for API stability;
    /// dropping it would require a Step-4-modal contract change.
    pub(crate) async fn notify_rejected(
        &self,
        workspace_id: &str,
        task: &Task,
        _feedback: Option<&str>,
    ) -> bool {
        self.notify_resolved(workspace_id, task).await
    }

    /// Common path for approve / reject. The two are merged because once
    /// status + feedback are durable on disk, the resolve direction is
    /// indistinguishable from the perspective of "is the plan complete?"
    /// and "what does the merged message say?". The merged-message
    /// builder reads each row's terminal status from the DB.
    async fn notify_resolved(&self, workspace_id: &str, task: &Task) -> bool {
        let shared = {
            let guard = self.inner.lock().await;
            let Some(router) = guard.get(workspace_id) else {
                // No live router. DB transition is durable; we can't drop
                // the latch entry (there's no latch). Per the §7 contract,
                // a missing router means "no PM to notify yet"; return
                // false so the Step-4 modal can show a warning chip.
                return false;
            };
            router.shared.clone()
        };
        shared.pending_approvals.lock().await.remove(&task.id);

        let workspace_id_owned = workspace_id.to_string();
        let plan_id = match &task.plan_id {
            None => {
                // Legacy row (pre-fix) — per-task message, strict
                // back-compat. Reuses the original Step-3 wording so
                // existing transcripts stay consistent.
                let body = if task.status == TaskStatus::Archived {
                    // The legacy reject path didn't persist feedback to
                    // DB; we have nothing to embed here. The Step 4
                    // modal's reject feedback is lost for legacy rows.
                    format!(
                        "[From system]\nTask \"{title}\" (id={id}) was rejected by the user \
                         and is now `archived` — it will not run. Please reconsider \
                         the plan; you can propose a new <propose_plan> block based on \
                         this feedback.",
                        title = task.title,
                        id = task.id,
                    )
                } else {
                    format!(
                        "[From system]\nTask \"{title}\" (id={id}) was approved by the user. \
                         It is now `ready` for you to begin (status: ready). Continue the \
                         conversation; if it needs hand-off to a teammate, use \
                         <send_message> now that the latch is lifted.",
                        title = task.title,
                        id = task.id,
                    )
                };
                return dispatch_system_reply(&shared, &task.proposed_by_agent_id, body)
                    .await
                    .is_ok();
            }
            Some(p) => p.clone(),
        };

        // Plan-level path: is the plan still in progress?
        let workspace_path = match resolve_workspace_path(&shared).await {
            Some(p) => p,
            None => return false,
        };
        let still_proposed = {
            let plan_id_clone = plan_id.clone();
            let ws_path = workspace_path.clone();
            match tokio::task::spawn_blocking(move || {
                crate::tasks::plan_still_has_proposed_at_path(&ws_path, &plan_id_clone)
            })
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(err)) => {
                    eprintln!(
                        "[team-router] plan_still_has_proposed for plan {plan_id} \
                         failed: {err}; treating as still-in-progress (no message sent)"
                    );
                    // Conservative: if we can't tell, say nothing rather
                    // than send a premature merged message.
                    return true;
                }
                Err(join_err) => {
                    eprintln!(
                        "[team-router] plan_still_has_proposed spawn_blocking panicked: \
                         {join_err}"
                    );
                    return true;
                }
            }
        };
        if still_proposed {
            // Plan still under review. No message; pm_notified=true means
            // "durable resolve, no warning needed for this call."
            let _ = workspace_id_owned; // silence unused-binding lint
            return true;
        }

        // Plan complete — assemble the merged message from the DB.
        let all_tasks = {
            let plan_id_clone = plan_id.clone();
            match tokio::task::spawn_blocking(move || {
                crate::tasks::list_tasks_by_plan_at_path(&workspace_path, &plan_id_clone)
            })
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(err)) => {
                    eprintln!(
                        "[team-router] list_tasks_by_plan for plan {plan_id} failed: \
                         {err}; cannot build merged message"
                    );
                    return false;
                }
                Err(join_err) => {
                    eprintln!(
                        "[team-router] list_tasks_by_plan spawn_blocking panicked: \
                         {join_err}"
                    );
                    return false;
                }
            }
        };
        let body = build_plan_completion_message(&all_tasks);
        dispatch_system_reply(&shared, &task.proposed_by_agent_id, body)
            .await
            .is_ok()
    }

    /// Test-only helper: snapshot the pending_approvals task ids for one
    /// workspace. Returns empty vec if the workspace has no router.
    #[cfg(test)]
    pub(crate) async fn pending_approvals_snapshot(&self, workspace_id: &str) -> Vec<String> {
        let guard = self.inner.lock().await;
        let Some(router) = guard.get(workspace_id) else {
            return Vec::new();
        };
        let map = router.shared.pending_approvals.lock().await;
        map.keys().cloned().collect()
    }
}

/// Phase 7 S1 — `<send_message>` routing permit.
///
/// Peer messaging: ANY team member may message ANY other team member, so a
/// recipient that is a known team agent is permitted outright — topology- and
/// channel-independent (`channels` are metadata, never a routing gate). This
/// relaxes the pre-P7 rule (routing == subscription topology, which only wired
/// PM↔Dev). The `user` pseudo-node stays topology-gated: only an agent the team
/// wired to the user (the PM) may message the user, so "the user talks only to
/// the PM" still holds.
fn can_send(
    subs: &[Subscription],
    known_agent_ids: &HashSet<String>,
    from: &str,
    to: &str,
    channel: &str,
) -> bool {
    if known_agent_ids.contains(to) {
        return true;
    }
    subs.iter().any(|s| {
        s.publisher == from
            && s.subscribers.iter().any(|sub| sub == to)
            && s.channels.iter().any(|c| c == channel)
    })
}

// Hand-rolled scanner: locates each `<send_message ATTRS>BODY</send_message>`
// block, extracts `to` and `channel` attrs (double-quoted, any order). Nested
// `</send_message>` literals inside BODY are not supported (the comm guide
// tells agents not to do that); the first close wins and truncates BODY.
fn parse_send_message_tags(text: &str) -> Vec<ParsedTag> {
    const OPEN_TAG: &str = "<send_message";
    const CLOSE_TAG: &str = "</send_message>";

    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open_at) = rest.find(OPEN_TAG) {
        let after_name = &rest[open_at + OPEN_TAG.len()..];
        // Require a whitespace after `<send_message` so we don't match e.g.
        // `<send_messages>`. (No-op if attrs follow with `>` immediately —
        // that's a malformed tag and we skip it.)
        let after_first = after_name.chars().next();
        if !matches!(after_first, Some(c) if c.is_ascii_whitespace() || c == '>') {
            // Not actually our tag — advance past the false positive and
            // keep scanning.
            rest = after_name;
            continue;
        }
        let Some(open_close_rel) = after_name.find('>') else {
            break;
        };
        let attrs_str = &after_name[..open_close_rel];
        let body_start = &after_name[open_close_rel + 1..];
        let Some(close_rel) = body_start.find(CLOSE_TAG) else {
            // Unterminated tag — advance past the open token to avoid an
            // infinite loop and stop trying to extract this one.
            rest = body_start;
            continue;
        };
        let body = &body_start[..close_rel];
        let to = extract_attr(attrs_str, "to");
        let channel = extract_attr(attrs_str, "channel");
        if let (Some(to), Some(channel)) = (to, channel) {
            out.push(ParsedTag {
                to,
                channel,
                content: body.trim().to_string(),
            });
        } else {
            eprintln!("[team-router] malformed send_message tag (missing to/channel); ignoring");
        }
        rest = &body_start[close_rel + CLOSE_TAG.len()..];
    }
    out
}

fn extract_attr(attrs: &str, name: &str) -> Option<String> {
    // Look for `<name>="..."` allowing any whitespace around `=`. Rejects
    // single-quoted values (the comm guide spec uses double quotes).
    let mut search_from = 0;
    while let Some(rel) = attrs[search_from..].find(name) {
        let abs = search_from + rel;
        // Boundary check: char before `name` must be start-of-string or
        // whitespace (avoid matching `to` inside e.g. `auto`).
        let is_boundary = abs == 0
            || attrs[..abs]
                .chars()
                .last()
                .map(|c| c.is_ascii_whitespace())
                .unwrap_or(false);
        if !is_boundary {
            search_from = abs + name.len();
            continue;
        }
        let after_name = &attrs[abs + name.len()..];
        let trimmed = after_name.trim_start();
        let Some(eq_after) = trimmed.strip_prefix('=') else {
            search_from = abs + name.len();
            continue;
        };
        let after_eq = eq_after.trim_start();
        let Some(after_quote) = after_eq.strip_prefix('"') else {
            search_from = abs + name.len();
            continue;
        };
        let Some(close_rel) = after_quote.find('"') else {
            return None;
        };
        return Some(after_quote[..close_rel].to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subs(rules: &[(&str, &[&str], &[&str])]) -> Vec<Subscription> {
        rules
            .iter()
            .map(|(pub_, subs, chans)| Subscription {
                publisher: (*pub_).to_string(),
                subscribers: subs.iter().map(|s| (*s).to_string()).collect(),
                channels: chans.iter().map(|c| (*c).to_string()).collect(),
            })
            .collect()
    }

    #[test]
    fn parses_single_tag() {
        let text = r#"hi <send_message to="bob" channel="chat">hello bob</send_message> bye"#;
        let tags = parse_send_message_tags(text);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].to, "bob");
        assert_eq!(tags[0].channel, "chat");
        assert_eq!(tags[0].content, "hello bob");
    }

    #[test]
    fn parses_attrs_in_either_order_and_multiline_body() {
        let text = "<send_message channel=\"chat\" to=\"alice\">line one\nline two</send_message>";
        let tags = parse_send_message_tags(text);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].to, "alice");
        assert_eq!(tags[0].content, "line one\nline two");
    }

    #[test]
    fn parses_multiple_tags_in_order() {
        let text = r#"<send_message to="a" channel="chat">one</send_message>
between
<send_message to="b" channel="chat">two</send_message>"#;
        let tags = parse_send_message_tags(text);
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].to, "a");
        assert_eq!(tags[1].to, "b");
    }

    #[test]
    fn ignores_malformed_tag_missing_to() {
        let text = r#"<send_message channel="chat">no recipient</send_message>"#;
        let tags = parse_send_message_tags(text);
        assert!(tags.is_empty());
    }

    #[test]
    fn ignores_unterminated_tag() {
        let text = r#"<send_message to="a" channel="chat">never closes"#;
        let tags = parse_send_message_tags(text);
        assert!(tags.is_empty());
    }

    #[test]
    fn can_send_permits_any_peer_and_keeps_user_topology_gated() {
        // Topology wires only PM↔{user,dev}, but every agent is a known member.
        let s = subs(&[("pm", &["user", "dev"], &["chat"])]);
        let agents: HashSet<String> = ["pm", "dev", "qa"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();

        // Peer messaging: ANY agent → ANY agent, ignoring topology + channel.
        assert!(can_send(&s, &agents, "pm", "dev", "chat")); // PM→Dev (also had a sub)
        assert!(can_send(&s, &agents, "dev", "pm", "anything")); // Dev→PM, no sub, any channel
        assert!(can_send(&s, &agents, "dev", "qa", "x")); // Dev→QA peer, no sub
        assert!(can_send(&s, &agents, "qa", "dev", "")); // QA→Dev peer, no sub

        // An unknown recipient is still denied.
        assert!(!can_send(&s, &agents, "pm", "ghost", "chat"));

        // The `user` pseudo-node stays topology-gated — only the PM (wired to
        // the user) may message the user; channel still matters for that path.
        assert!(can_send(&s, &agents, "pm", "user", "chat")); // PM→user: has sub
        assert!(!can_send(&s, &agents, "dev", "user", "chat")); // Dev→user: no sub → denied
        assert!(!can_send(&s, &agents, "pm", "user", "secret")); // wrong channel → denied
    }

    // -- Step 3 latch predicate ---------------------------------------------

    fn pending_map(entries: &[(&str, &str)]) -> HashMap<String, PendingTaskInfo> {
        entries
            .iter()
            .map(|(task_id, proposer)| {
                (
                    (*task_id).to_string(),
                    PendingTaskInfo {
                        proposed_by_agent_id: (*proposer).to_string(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn map_has_proposer_true_when_emitter_has_any_pending() {
        let m = pending_map(&[("t1", "pm-alice"), ("t2", "pm-alice"), ("t3", "dev-bob")]);
        assert!(map_has_proposer(&m, "pm-alice"));
        assert!(map_has_proposer(&m, "dev-bob"));
    }

    #[test]
    fn map_has_proposer_false_for_non_proposer() {
        // Dev sends a message; only PM has pending — Dev must NOT be blocked.
        let m = pending_map(&[("t1", "pm-alice"), ("t2", "pm-alice")]);
        assert!(!map_has_proposer(&m, "dev-bob"));
        assert!(!map_has_proposer(&m, "qa-carol"));
    }

    #[test]
    fn map_has_proposer_false_when_empty() {
        let m = pending_map(&[]);
        assert!(!map_has_proposer(&m, "anyone"));
    }

    // -- S6-2 structured tool-call path ------------------------------------

    fn agent_info(id: &str, name: &str, thread: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            name: name.to_string(),
            thread_id: thread.to_string(),
            freshly_provisioned: false,
        }
    }

    fn send_args(to: &str, channel: &str, body: &str) -> SendMessageArgs {
        SendMessageArgs {
            to: to.to_string(),
            channel: channel.to_string(),
            body: body.to_string(),
        }
    }

    fn route_ctx<'a>(
        subscriptions: &'a [Subscription],
        known: &'a HashSet<String>,
        by_id: &'a HashMap<String, String>,
    ) -> RouteCtx<'a> {
        RouteCtx {
            subscriptions,
            known_agent_ids: known,
            by_agent_id: by_id,
            workspace_id: "ws-1",
            team_id: "team-1",
        }
    }

    fn known_set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn parse_team_tool_call_lifts_opencrab_team_calls() {
        let ev = serde_json::json!({
            "method": "item/completed",
            "params": { "item": {
                "type": "mcpToolCall",
                "server": "opencrab-team",
                "tool": "send_message",
                "arguments": { "to": "bob", "channel": "chat", "body": "hi" }
            }}
        });
        let tc = parse_team_tool_call(&ev).expect("should lift the opencrab-team call");
        assert_eq!(tc.tool, "send_message");
        assert_eq!(tc.arguments.get("to").and_then(Value::as_str), Some("bob"));
        assert_eq!(tc.arguments.get("body").and_then(Value::as_str), Some("hi"));
    }

    #[test]
    fn parse_team_tool_call_ignores_non_team_and_non_toolcalls() {
        // Wrong server (the memory MCP) — ignored.
        let other_server = serde_json::json!({
            "method": "item/completed",
            "params": { "item": {
                "type": "mcpToolCall", "server": "opencrab-memory",
                "tool": "memory_search", "arguments": {}
            }}
        });
        assert!(parse_team_tool_call(&other_server).is_none());

        // Not a tool call (an agent message item) — ignored.
        let msg_item = serde_json::json!({
            "method": "item/completed",
            "params": { "item": { "type": "agentMessage", "text": "hello" }}
        });
        assert!(parse_team_tool_call(&msg_item).is_none());

        // Not an item/completed (a delta) — ignored.
        let delta = serde_json::json!({
            "method": "item/agentMessage/delta",
            "params": { "delta": "hi" }
        });
        assert!(parse_team_tool_call(&delta).is_none());
    }

    #[test]
    fn decide_send_message_routes_permitted_peer_to_dispatch() {
        let s = subs(&[("pm", &["user", "dev"], &["chat"])]);
        let known = known_set(&["pm", "dev"]);
        let mut by_id = HashMap::new();
        by_id.insert("dev".to_string(), "thread-dev".to_string());
        let ctx = route_ctx(&s, &known, &by_id);
        let pm = agent_info("pm", "Alice", "thread-pm");
        let args = send_args("dev", "chat", "ship it");
        match decide_send_message(&ctx, &pm, &args, false, "2026-06-05T10:00:00Z") {
            SendMessageOutcome::Dispatch {
                target_thread,
                framed,
                chat,
            } => {
                assert_eq!(target_thread, "thread-dev");
                assert_eq!(framed, "[From Alice]\nship it");
                // body → content rename + chat row fields.
                assert_eq!(chat.content, "ship it");
                assert_eq!(chat.sender, "pm");
                assert_eq!(chat.recipient.as_deref(), Some("dev"));
                assert_eq!(chat.thread_id.as_deref(), Some("thread-pm"));
                assert_eq!(chat.kind.as_str(), "send_message");
            }
            _ => panic!("expected Dispatch"),
        }
    }

    #[test]
    fn decide_send_message_denies_unknown_recipient() {
        let s = subs(&[("pm", &["user"], &["chat"])]);
        let known = known_set(&["pm", "dev"]);
        let by_id = HashMap::new();
        let ctx = route_ctx(&s, &known, &by_id);
        let pm = agent_info("pm", "Alice", "thread-pm");
        let args = send_args("ghost", "chat", "hi");
        assert!(matches!(
            decide_send_message(&ctx, &pm, &args, false, "t"),
            SendMessageOutcome::AclDenied
        ));
    }

    #[test]
    fn decide_send_message_user_target_records_without_dispatch() {
        let s = subs(&[("pm", &["user"], &["chat"])]);
        let known = known_set(&["pm"]);
        let by_id = HashMap::new();
        let ctx = route_ctx(&s, &known, &by_id);
        let pm = agent_info("pm", "Alice", "thread-pm");
        let args = send_args("user", "chat", "done!");
        match decide_send_message(&ctx, &pm, &args, false, "t") {
            SendMessageOutcome::UserTarget { chat } => {
                assert_eq!(chat.recipient.as_deref(), Some("user"));
                assert_eq!(chat.content, "done!");
                assert_eq!(chat.sender, "pm");
            }
            _ => panic!("expected UserTarget"),
        }
    }

    #[test]
    fn decide_send_message_gates_when_pending_approval() {
        let s = subs(&[("pm", &["dev"], &["chat"])]);
        let known = known_set(&["pm", "dev"]);
        let mut by_id = HashMap::new();
        by_id.insert("dev".to_string(), "thread-dev".to_string());
        let ctx = route_ctx(&s, &known, &by_id);
        let pm = agent_info("pm", "Alice", "thread-pm");
        let args = send_args("dev", "chat", "go");
        // has_pending = true → gated even though ACL permits and a thread exists.
        assert!(matches!(
            decide_send_message(&ctx, &pm, &args, true, "t"),
            SendMessageOutcome::ApprovalGated
        ));
    }

    #[test]
    fn decide_send_message_no_thread_for_known_agent() {
        let s = subs(&[]);
        let known = known_set(&["pm", "dev"]);
        let by_id = HashMap::new(); // dev is a known member but has no bound thread
        let ctx = route_ctx(&s, &known, &by_id);
        let pm = agent_info("pm", "Alice", "thread-pm");
        let args = send_args("dev", "chat", "hi");
        assert!(matches!(
            decide_send_message(&ctx, &pm, &args, false, "t"),
            SendMessageOutcome::NoTargetThread
        ));
    }

    #[test]
    fn partition_tool_calls_orders_plans_before_messages() {
        let calls = vec![
            TeamToolCall {
                tool: "send_message".into(),
                arguments: serde_json::json!({"to":"dev","channel":"chat","body":"m1"}),
            },
            TeamToolCall {
                tool: "propose_plan".into(),
                arguments: serde_json::json!({"tasks":[]}),
            },
            TeamToolCall {
                tool: "send_message".into(),
                arguments: serde_json::json!({"to":"dev","channel":"chat","body":"m2"}),
            },
        ];
        let (plans, msgs) = partition_tool_calls(calls);
        // The split is what guarantees plans are handled before any message
        // (the orchestrator drains `plans` fully before `msgs`).
        assert_eq!(plans.len(), 1);
        assert_eq!(msgs.len(), 2);
        // Relative order within the message bucket is preserved.
        assert_eq!(msgs[0].get("body").and_then(Value::as_str), Some("m1"));
        assert_eq!(msgs[1].get("body").and_then(Value::as_str), Some("m2"));
    }

    #[test]
    fn propose_plan_args_map_to_parsed_plan() {
        let args = ProposePlanArgs {
            tasks: vec![
                ProposePlanTaskArg {
                    title: "A".into(),
                    body: "do a".into(),
                    assignee: Some("dev".into()),
                },
                ProposePlanTaskArg {
                    title: "B".into(),
                    body: "do b".into(),
                    assignee: None,
                },
            ],
        };
        let plan = propose_plan_args_to_plan(args);
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].title, "A");
        assert_eq!(plan.tasks[0].body, "do a");
        assert_eq!(plan.tasks[0].assignee.as_deref(), Some("dev"));
        assert!(plan.tasks[1].assignee.is_none());
    }

    #[test]
    fn build_user_input_chat_keys_row_under_target_agent() {
        // A human message to "pm" is recorded under pm's thread, addressed to
        // pm, as kind=user_input from sender "user".
        let chat = build_user_input_chat(
            "ws-1",
            "team-1",
            "thread-pm",
            "pm",
            "build me an app",
            "2026-06-05T10:00:00Z",
        );
        assert_eq!(chat.sender, "user");
        assert_eq!(chat.recipient.as_deref(), Some("pm"));
        assert_eq!(chat.thread_id.as_deref(), Some("thread-pm"));
        assert_eq!(chat.kind.as_str(), "user_input");
        assert_eq!(chat.content, "build me an app");
        assert_eq!(chat.workspace_id, "ws-1");
        assert_eq!(chat.team_id, "team-1");
    }

    #[test]
    fn build_gated_reply_merges_per_turn() {
        // Nothing gated → no reply.
        assert!(build_gated_reply(0).is_none());
        // N gated → ONE reply naming the count (not N separate notes).
        let body = build_gated_reply(3).expect("a reply when >0 gated");
        assert!(body.starts_with("[From system]"));
        assert!(body.contains("3 message(s)"));
        assert!(body.contains("pending user approval"));
    }

    // -- Step 3 §10.3: per-workspace latch isolation -----------------------
    //
    // Invariant: each workspace's `pending_approvals` is a separate map,
    // and the send_message block predicate (`map_has_proposer`) queries
    // only the map of the workspace it is given. So a workspace A
    // proposer's task id MUST NOT trip the predicate when queried against
    // workspace B's map, and vice versa.
    //
    // Why this lives inline (#[cfg(test)]) rather than in `tests/`: the
    // real `TeamRouters::start` couples to Tauri AppHandle + Codex
    // sessions; a true integration test would need a Tauri mock runtime
    // which is out of scope for Phase 3 closeout. The invariant being
    // tested here is the SAME shape (two independent latch maps; the
    // predicate reads only one), exercised against the same internal API
    // the production code uses. Phase 4's state-sync 专项 will revisit
    // whether to harden this with a full Tauri-mock end-to-end test.

    fn populated_pending(entries: &[(&str, &str)]) -> HashMap<String, PendingTaskInfo> {
        entries
            .iter()
            .map(|(task_id, proposer)| {
                (
                    (*task_id).to_string(),
                    PendingTaskInfo {
                        proposed_by_agent_id: (*proposer).to_string(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn pending_approvals_are_isolated_per_workspace() {
        // workspace A: PM-alice has tasks pending.
        let ws_a = populated_pending(&[("a-task-1", "pm-alice"), ("a-task-2", "pm-alice")]);
        // workspace B: PM-bob has unrelated tasks pending.
        let ws_b = populated_pending(&[("b-task-1", "pm-bob")]);

        // Sanity: each workspace's own map has its own proposer.
        assert!(map_has_proposer(&ws_a, "pm-alice"));
        assert!(map_has_proposer(&ws_b, "pm-bob"));

        // Crucial: workspace A's predicate does NOT flag pm-bob (the
        // other workspace's proposer), and workspace B's predicate does
        // NOT flag pm-alice. The send_message block in workspace A
        // therefore can't be tripped by workspace B's pending tasks.
        assert!(!map_has_proposer(&ws_a, "pm-bob"));
        assert!(!map_has_proposer(&ws_b, "pm-alice"));

        // And task ids are scoped: a id collision between workspaces (a
        // pathological case, but defensive) doesn't leak metadata. The
        // map lookup returns the proposer from the LOCAL workspace, not
        // some other workspace's, because we never query across maps.
        let mut ws_a_colliding = ws_a.clone();
        ws_a_colliding.insert(
            "b-task-1".to_string(),
            PendingTaskInfo {
                proposed_by_agent_id: "pm-alice".to_string(),
            },
        );
        // ws_a_colliding["b-task-1"].proposed_by_agent_id == "pm-alice"
        // ws_b["b-task-1"].proposed_by_agent_id == "pm-bob"
        // Each predicate query reads its own map's entry — no bleed.
        assert!(map_has_proposer(&ws_a_colliding, "pm-alice"));
        assert!(!map_has_proposer(&ws_a_colliding, "pm-bob"));
        assert!(map_has_proposer(&ws_b, "pm-bob"));
        assert!(!map_has_proposer(&ws_b, "pm-alice"));
    }

    #[test]
    fn pending_approvals_drain_in_one_workspace_does_not_affect_other() {
        // Approve flow: removing a task from workspace A's map MUST NOT
        // affect workspace B's map. The latch is a per-workspace
        // HashMap, not a global registry — verifying explicitly.
        let mut ws_a = populated_pending(&[("a-task-1", "pm-alice"), ("a-task-2", "pm-alice")]);
        let ws_b = populated_pending(&[("b-task-1", "pm-bob")]);

        ws_a.remove("a-task-1");
        assert_eq!(ws_a.len(), 1);
        assert!(map_has_proposer(&ws_a, "pm-alice")); // a-task-2 still there
                                                      // Workspace B untouched.
        assert_eq!(ws_b.len(), 1);
        assert!(map_has_proposer(&ws_b, "pm-bob"));

        ws_a.remove("a-task-2");
        assert!(ws_a.is_empty());
        assert!(!map_has_proposer(&ws_a, "pm-alice"));
        // Workspace B STILL untouched after A drained.
        assert_eq!(ws_b.len(), 1);
        assert!(map_has_proposer(&ws_b, "pm-bob"));
    }

    #[test]
    fn tasks_proposed_event_serializes_schema_version_field() {
        // Phase 3 closeout (#19) pin: the wire-shape carries a numeric
        // schemaVersion field. The frontend type pins on `1` and would
        // type-error if the field went missing. Asserting the JSON
        // representation here catches a silent removal in a future PR.
        use crate::tasks::tasks_proposed_event::CURRENT_SCHEMA_VERSION;
        let payload = TasksProposedEvent {
            schema_version: CURRENT_SCHEMA_VERSION,
            workspace_id: "ws-1".to_string(),
            team_id: "team-1".to_string(),
            proposed_by_agent_id: "pm-alice".to_string(),
            tasks: vec![],
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["schemaVersion"], 1);
        assert!(json.get("schemaVersion").is_some());
    }

    // -- Phase 3 bug fix: build_plan_completion_message ---------------------

    fn task_fixture(
        id: &str,
        title: &str,
        status: TaskStatus,
        assignee: Option<&str>,
        feedback: Option<&str>,
    ) -> Task {
        Task {
            id: id.to_string(),
            workspace_id: "ws-1".to_string(),
            team_id: "team-1".to_string(),
            assignee_agent_id: assignee.map(str::to_string),
            proposed_by_agent_id: "pm-alice".to_string(),
            status,
            title: title.to_string(),
            body: String::new(),
            approved_at: None,
            completed_at: None,
            created_at: "2026-05-17T00:00:00Z".to_string(),
            updated_at: "2026-05-17T00:00:00Z".to_string(),
            plan_id: Some("plan_test".to_string()),
            feedback: feedback.map(str::to_string),
        }
    }

    #[test]
    fn merged_message_all_approved() {
        let tasks = vec![
            task_fixture(
                "t1",
                "set up scaffolding",
                TaskStatus::Ready,
                Some("dev-bob"),
                None,
            ),
            task_fixture(
                "t2",
                "write tests",
                TaskStatus::Ready,
                Some("dev-bob"),
                None,
            ),
        ];
        let msg = build_plan_completion_message(&tasks);
        assert!(msg.starts_with("[From system]\n"));
        assert!(msg.contains("2 task(s) approved, 0 rejected."));
        assert!(msg.contains("Approved:"));
        assert!(msg.contains("\"set up scaffolding\" (id=t1) — now ready, assignee=dev-bob"));
        assert!(msg.contains("\"write tests\" (id=t2) — now ready, assignee=dev-bob"));
        assert!(
            !msg.contains("Rejected:"),
            "no rejected section when all approved"
        );
        assert!(msg.contains("You may now dispatch the approved tasks via <send_message>"));
    }

    #[test]
    fn merged_message_all_rejected_omits_dispatch_sentence() {
        let tasks = vec![
            task_fixture(
                "t1",
                "rip out auth",
                TaskStatus::Archived,
                None,
                Some("scope too big"),
            ),
            task_fixture("t2", "rewrite db layer", TaskStatus::Archived, None, None),
        ];
        let msg = build_plan_completion_message(&tasks);
        assert!(msg.contains("0 task(s) approved, 2 rejected."));
        assert!(
            !msg.contains("Approved:"),
            "no approved section when all rejected"
        );
        assert!(msg.contains("Rejected:"));
        // With feedback (verbatim).
        assert!(msg.contains("\"rip out auth\" (id=t1): scope too big"));
        // Without feedback (placeholder).
        assert!(msg.contains("\"rewrite db layer\" (id=t2): (no feedback provided)"));
        assert!(
            !msg.contains("You may now dispatch"),
            "dispatch sentence is omitted when nothing was approved"
        );
    }

    #[test]
    fn merged_message_mixed_approve_and_reject() {
        let tasks = vec![
            task_fixture("t1", "step one", TaskStatus::Ready, Some("dev-bob"), None),
            task_fixture(
                "t2",
                "step two",
                TaskStatus::Archived,
                None,
                Some("not now"),
            ),
            task_fixture("t3", "step three", TaskStatus::Ready, None, None),
        ];
        let msg = build_plan_completion_message(&tasks);
        assert!(msg.contains("2 task(s) approved, 1 rejected."));
        assert!(msg.contains("Approved:"));
        assert!(msg.contains("Rejected:"));
        assert!(msg.contains("\"step one\" (id=t1) — now ready, assignee=dev-bob"));
        assert!(msg.contains("\"step three\" (id=t3) — now ready, assignee=unassigned"));
        assert!(msg.contains("\"step two\" (id=t2): not now"));
        assert!(msg.contains("You may now dispatch"));
        // Approved must come before Rejected in the body.
        let approved_pos = msg.find("Approved:").unwrap();
        let rejected_pos = msg.find("Rejected:").unwrap();
        assert!(approved_pos < rejected_pos);
    }

    #[test]
    fn merged_message_unassigned_renders_placeholder() {
        let tasks = vec![task_fixture(
            "t1",
            "do stuff",
            TaskStatus::Ready,
            None,
            None,
        )];
        let msg = build_plan_completion_message(&tasks);
        assert!(msg.contains("assignee=unassigned"));
    }

    #[test]
    fn merged_message_whitespace_only_feedback_renders_placeholder() {
        let tasks = vec![task_fixture(
            "t1",
            "do stuff",
            TaskStatus::Archived,
            None,
            Some("   "),
        )];
        let msg = build_plan_completion_message(&tasks);
        assert!(msg.contains("(no feedback provided)"));
    }

    #[test]
    fn merged_message_empty_tasks_returns_defensive_warning() {
        let msg = build_plan_completion_message(&[]);
        assert!(msg.contains("Plan review complete, but no tasks were found"));
    }

    // -- Phase 6 — Block C (inherited from P5): re-injection invariant ---------------------

    #[test]
    fn prelude_is_stale_covers_the_three_reinjection_triggers() {
        // Trigger 1 — post-compaction: `thread/compacted` cleared the entry.
        assert!(prelude_is_stale(None, "2026-05-23"));
        // Trigger 2 — long thread crossed a day.
        assert!(prelude_is_stale(Some("2026-05-22"), "2026-05-23"));
        // Trigger 3 — resumed thread (initial `None`).
        assert!(prelude_is_stale(None, "2026-05-23"));
        // Fresh same-day prelude → NOT stale, no re-injection needed.
        assert!(!prelude_is_stale(Some("2026-05-23"), "2026-05-23"));
    }

    // -- Phase 6 — Block C (inherited from P5): regression cover for the glue ----------------

    #[test]
    fn compose_with_prelude_is_pass_through_for_empty_and_prepends_otherwise() {
        // Empty prelude (legit "no log entries yet" case) → no-op.
        assert_eq!(compose_with_prelude("", "hello".to_string()), "hello");
        // Non-empty prelude → prelude + blank line + text.
        assert_eq!(
            compose_with_prelude("[Untrusted memory recall]\n…", "hello".to_string()),
            "[Untrusted memory recall]\n…\n\nhello",
        );
    }

    #[test]
    fn finalize_text_fresh_does_not_change_text_or_state() {
        // Live context already has today's prelude — no-op even when a
        // (notional) fetch result would have produced content.
        let (text, new_date) = finalize_text_with_prelude(
            "hello".to_string(),
            Some("2026-05-23"),
            "2026-05-23",
            Ok("PRELUDE".to_string()),
        );
        assert_eq!(text, "hello");
        assert_eq!(new_date, None);
    }

    #[test]
    fn finalize_text_stale_post_compaction_re_injects_and_marks_fresh() {
        // Trigger 1 — post-compaction (`current == None`): re-inject + state
        // updated.
        let (text, new_date) = finalize_text_with_prelude(
            "hello".to_string(),
            None,
            "2026-05-23",
            Ok("PRELUDE".to_string()),
        );
        assert_eq!(text, "PRELUDE\n\nhello");
        assert_eq!(new_date, Some("2026-05-23".to_string()));
    }

    #[test]
    fn finalize_text_stale_cross_day_re_injects_and_marks_fresh() {
        // Trigger 2 — long thread crossed a day (yesterday → today).
        let (text, new_date) = finalize_text_with_prelude(
            "hello".to_string(),
            Some("2026-05-22"),
            "2026-05-23",
            Ok("PRELUDE".to_string()),
        );
        assert_eq!(text, "PRELUDE\n\nhello");
        assert_eq!(new_date, Some("2026-05-23".to_string()));
    }

    #[test]
    fn finalize_text_stale_empty_prelude_marks_fresh_but_does_not_prepend() {
        // Legit "agent has no log entries yet" — fetch succeeds
        // with an empty string. Don't prepend, but DO mark the thread fresh
        // so we don't refetch the empty string on every subsequent message.
        let (text, new_date) = finalize_text_with_prelude(
            "hello".to_string(),
            None,
            "2026-05-23",
            Ok(String::new()),
        );
        assert_eq!(text, "hello");
        assert_eq!(new_date, Some("2026-05-23".to_string()));
    }

    #[test]
    fn finalize_text_stale_rpc_err_soft_fails_text_unchanged_state_unchanged() {
        // Phase 6 — Block C (inherited from P5) Part 6 contract: a prelude-fetch failure
        // (sidecar gone, NDJSON timeout, RPC error, whatever) MUST NOT
        // block the real user / agent message. Soft fail → text passes
        // through unchanged. State STAYS stale so the next outgoing
        // message retries the fetch.
        let (text, new_date) = finalize_text_with_prelude(
            "hello".to_string(),
            None,
            "2026-05-23",
            Err(()),
        );
        assert_eq!(text, "hello");
        assert_eq!(new_date, None);
    }

    #[test]
    fn initial_prelude_date_reflects_freshly_provisioned_signal() {
        // Sidecar just kicked this agent off → today's prelude is in live
        // context → start fresh (no re-injection on first message).
        assert_eq!(
            initial_prelude_date(true, "2026-05-23"),
            Some("2026-05-23".to_string()),
        );
        // Resumed thread (no kickoff this session) → unknown live context →
        // start `None` so the next outgoing message re-injects (trigger 3).
        assert_eq!(initial_prelude_date(false, "2026-05-23"), None);
    }

    // -- P6 Step 4 — Block C link-level chain --------------------------------

    /// Walks the three Block C pieces sequentially — the real
    /// `mark_prelude_stale_on_compaction` (which is the `thread/compacted`
    /// arm's body), predicate flips stale, `finalize_text_with_prelude`
    /// prepends + marks fresh — as one narrative the predicate-level tests
    /// above leave at unit granularity.
    ///
    /// Binds to the **production function**, not a literal copy of the
    /// state op — if a future change to the arm's body diverges from the
    /// extracted fn, the real arm goes with it and this test catches no
    /// drift. `spawn_consumer`'s full event loop and the Tauri RPC inside
    /// `prepend_prelude_if_stale` aren't in scope here; everything else
    /// on the chain is.
    #[tokio::test]
    async fn thread_compacted_clears_prelude_dates_then_next_message_reinjects() {
        use std::collections::HashMap;
        use tokio::sync::Mutex;

        let prelude_dates: Mutex<HashMap<String, Option<String>>> =
            Mutex::new(HashMap::new());
        let thread = "thread-A".to_string();
        let today = "2026-05-26";

        // Initial state: kickoff put today's prelude in this thread's live
        // context (the freshly-provisioned path).
        prelude_dates
            .lock()
            .await
            .insert(thread.clone(), Some(today.to_string()));

        // Stretch 1 — fresh state, finalize is a no-op.
        {
            let current = prelude_dates
                .lock()
                .await
                .get(&thread)
                .cloned()
                .flatten();
            assert!(
                !prelude_is_stale(current.as_deref(), today),
                "fresh — no re-inject"
            );
            let (text, new_date) = finalize_text_with_prelude(
                "first message".to_string(),
                current.as_deref(),
                today,
                Ok("PRELUDE".to_string()),
            );
            assert_eq!(text, "first message");
            assert_eq!(new_date, None);
        }

        // ★ Stretch 2 — `thread/compacted` arrives. Call the exact function
        //    the real arm calls. Any future drift in the arm's body must go
        //    through this function, and so the test stays bound to it.
        mark_prelude_stale_on_compaction(&prelude_dates, &thread).await;

        // Stretch 3 — next outgoing message: predicate sees stale, fetch
        // succeeds, finalize prepends + signals "mark fresh".
        let new_date_after_reinject = {
            let current = prelude_dates
                .lock()
                .await
                .get(&thread)
                .cloned()
                .flatten();
            assert!(
                prelude_is_stale(current.as_deref(), today),
                "post-compacted — must be stale"
            );
            let (composed, new_date) = finalize_text_with_prelude(
                "follow-up message".to_string(),
                current.as_deref(),
                today,
                Ok("[Untrusted memory recall]\n…\n[/Untrusted memory recall]".to_string()),
            );
            assert!(
                composed.starts_with("[Untrusted memory recall]"),
                "composed = {composed}"
            );
            assert!(composed.contains("follow-up message"));
            assert_eq!(new_date, Some(today.to_string()));
            new_date
        };

        // Apply `new_date` back the way `prepend_prelude_if_stale` does on
        // a successful re-injection.
        prelude_dates
            .lock()
            .await
            .insert(thread.clone(), new_date_after_reinject);

        // Stretch 4 — subsequent same-day message is fresh again.
        {
            let current = prelude_dates
                .lock()
                .await
                .get(&thread)
                .cloned()
                .flatten();
            assert!(
                !prelude_is_stale(current.as_deref(), today),
                "post-reinject — fresh until next compaction or day rollover"
            );
        }
    }

    #[test]
    fn map_has_proposer_disregards_task_id_match() {
        // The predicate ignores task_id entirely — sender_id vs
        // proposed_by_agent_id is what matters. (Sanity-check: a task with
        // an id matching the sender shouldn't trip the predicate just on
        // that.)
        let m = pending_map(&[("pm-alice", "dev-bob")]);
        assert!(!map_has_proposer(&m, "pm-alice"));
        assert!(map_has_proposer(&m, "dev-bob"));
    }
}
