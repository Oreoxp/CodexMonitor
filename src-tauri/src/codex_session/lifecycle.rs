//! V2 step 2: Codex notification / event router.
//!
//! `start_router` consumes `CodexRpcClient::subscribe_notifications()` and
//! does the dispatch that used to live inside `app_server::setup_session_runtime`'s
//! reader loop.  All routing-state writes go through `SessionRouting`.
//! All UI-bound emits go through `EventSink::emit_app_server_event`.
//!
//! Response-side post-processing (the `thread/list` / related-thread mapping
//! that used to happen on the response branch of the old reader loop) lives
//! in `record_response`, called by `WorkspaceSession::send_request_for_workspace`
//! after `rpc.request` returns.
//!
//! `start_stderr_forwarder` mirrors the legacy stderr → `codex/stderr`
//! AppServerEvent forwarder.
//!
//! Reader-exit signalling: this module also wires `CodexRpcClient::take_exit_signal()`
//! to the V1 `ReaderExitNotifier`, so the manager can flip status to
//! `Disconnected` (clean EOF) or `Crashed` (transport-level error) the same
//! way the legacy reader loop did.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

use crate::backend::events::{AppServerEvent, EventSink, ReaderExitNotifier};
use crate::codex_transport::{CodexRpcClient, CodexTransport, CodexTransportKind};
use crate::types::WorkspaceEntry;

use super::routing::{normalize_root_path, SessionRouting};
use super::session::WorkspaceSession;

// ──────────────────────────────────────────────────────────────────────
// Session bring-up entrypoints (V2 step 3 — moved from `backend::app_server`)
// ──────────────────────────────────────────────────────────────────────

/// Bring up an RPC-driven Codex session.
///
/// Caller provides the `Arc<CodexRpcClient>` (so the manager can share the
/// same handle with `CodexSession.rpc`), the matching `Arc<dyn CodexTransport>`,
/// the routing, and an optional reader-exit notifier.  We:
///   1. Spawn the lifecycle router + stderr forwarder (subscribe BEFORE
///      `rpc.start()` so no notifications are lost).
///   2. Call `rpc.start()` so the rpc reader owns `transport.recv()`.
///   3. Run the `initialize` / `initialized` handshake via `rpc.initialize`.
///   4. Build a thin `WorkspaceSession` whose only job is RPC plumbing.
async fn setup_session_runtime<E: EventSink>(
    rpc: Arc<CodexRpcClient>,
    transport: Arc<dyn CodexTransport>,
    stderr_rx: mpsc::UnboundedReceiver<String>,
    entry: &WorkspaceEntry,
    codex_args: Option<String>,
    client_version: &str,
    event_sink: E,
    exit_notifier: Option<Arc<dyn ReaderExitNotifier>>,
    routing: Arc<SessionRouting>,
) -> Result<Arc<WorkspaceSession>, String> {
    routing
        .register_workspace_with_path(&entry.id, Some(&entry.path))
        .await;

    start_router(
        Arc::clone(&rpc),
        Arc::clone(&routing),
        entry.id.clone(),
        event_sink.clone(),
        exit_notifier,
    );
    start_stderr_forwarder(stderr_rx, entry.id.clone(), event_sink.clone());

    rpc.start().await;

    let init_response = match timeout(
        Duration::from_secs(15),
        rpc.initialize(client_version),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            let _ = rpc.shutdown().await;
            return Err(format!("initialize failed: {err}"));
        }
        Err(_) => {
            let _ = rpc.shutdown().await;
            return Err(
                "Codex app-server did not respond to initialize. Check that `codex app-server` works in Terminal."
                    .to_string(),
            );
        }
    };
    if let Some(error) = init_response.get("error") {
        let _ = rpc.shutdown().await;
        return Err(format!("initialize returned error: {error}"));
    }

    let session = Arc::new(WorkspaceSession {
        codex_args,
        child: None,
        stdin: None,
        transport: Some(transport),
        rpc: Some(rpc),
        routing,
    });

    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id: entry.id.clone(),
        message: json!({
            "method": "codex/connected",
            "params": { "workspaceId": entry.id.clone() }
        }),
    });

    Ok(session)
}

/// Daemon-style spawn entrypoint that owns its own transport bring-up.
///
/// Used by `bin/codex_monitor_daemon` and any caller that does NOT go
/// through `CodexSessionManager`.  Selects the transport via
/// `codex_transport::create_transport` (env var → caller setting → ws),
/// emits the `codex/transportSelected` / `codex/transportFallback`
/// AppServerEvents the UI expects, then runs `setup_session_runtime`.
pub(crate) async fn spawn_workspace_session<E: EventSink>(
    entry: WorkspaceEntry,
    default_codex_bin: Option<String>,
    codex_args: Option<String>,
    codex_home: Option<PathBuf>,
    client_version: String,
    event_sink: E,
    transport_kind: Option<CodexTransportKind>,
) -> Result<Arc<WorkspaceSession>, String> {
    let bundle = crate::codex_transport::create_transport(
        default_codex_bin,
        codex_args.as_deref(),
        &entry.path,
        codex_home.as_ref(),
        transport_kind,
    )
    .await?;

    let transport_kind_str = bundle.kind.to_string();
    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id: entry.id.clone(),
        message: json!({
            "method": "codex/transportSelected",
            "params": {
                "transport": transport_kind_str,
                "fallback": bundle.fallback_warning.is_some(),
            }
        }),
    });
    if let Some(ref warning) = bundle.fallback_warning {
        event_sink.emit_app_server_event(AppServerEvent {
            workspace_id: entry.id.clone(),
            message: json!({
                "method": "codex/transportFallback",
                "params": {
                    "warning": warning,
                    "activeTransport": transport_kind_str,
                }
            }),
        });
    }

    let routing = SessionRouting::new(entry.id.clone());
    let rpc = Arc::new(CodexRpcClient::new_from_arc(Arc::clone(&bundle.transport)));
    setup_session_runtime(
        rpc,
        bundle.transport,
        bundle.stderr_rx,
        &entry,
        codex_args,
        &client_version,
        event_sink,
        None,
        routing,
    )
    .await
}

/// Manager-aware spawn entrypoint used by `CodexSessionManager::connect`.
///
/// The manager creates the `Arc<CodexRpcClient>` (so the same handle is
/// shared with `CodexSession.rpc`) and passes it in here.  Status events
/// (`Starting` / `Connected`) have already been emitted by the manager;
/// this function just runs the rpc startup + initialize handshake + router
/// spawn and returns the bound `WorkspaceSession`.
pub(crate) async fn spawn_workspace_session_with_manager_inner<E: EventSink>(
    entry: WorkspaceEntry,
    rpc: Arc<CodexRpcClient>,
    transport: Arc<dyn CodexTransport>,
    stderr_rx: mpsc::UnboundedReceiver<String>,
    codex_args: Option<String>,
    client_version: String,
    event_sink: E,
    exit_notifier: Arc<dyn ReaderExitNotifier>,
    routing: Arc<SessionRouting>,
) -> Result<Arc<WorkspaceSession>, String> {
    setup_session_runtime(
        rpc,
        transport,
        stderr_rx,
        &entry,
        codex_args,
        &client_version,
        event_sink,
        Some(exit_notifier),
        routing,
    )
    .await
}

// ──────────────────────────────────────────────────────────────────────
// Notification router (subscribes to rpc.subscribe_notifications())
// ──────────────────────────────────────────────────────────────────────

/// Spawn the notification router task plus the reader-exit watcher.
///
/// `fallback_workspace_id` is the owner workspace id, used when a
/// notification has no thread → workspace mapping yet (matches the legacy
/// reader loop's `fallback_workspace_id` semantics).
pub(crate) fn start_router<E: EventSink>(
    rpc: Arc<CodexRpcClient>,
    routing: Arc<SessionRouting>,
    fallback_workspace_id: String,
    event_sink: E,
    exit_notifier: Option<Arc<dyn ReaderExitNotifier>>,
) {
    // Dispatch loop on the rpc's notification broadcast.
    {
        let routing = Arc::clone(&routing);
        let fallback_workspace_id = fallback_workspace_id.clone();
        let event_sink = event_sink.clone();
        let mut notification_rx = rpc.subscribe_notifications();
        tokio::spawn(async move {
            loop {
                match notification_rx.recv().await {
                    Ok(value) => {
                        dispatch_notification(
                            value,
                            &routing,
                            &fallback_workspace_id,
                            &event_sink,
                        )
                        .await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Drop the missed batch; keep going.
                        continue;
                    }
                }
            }
        });
    }

    // Reader-exit watcher: when the rpc's reader task ends, route the
    // reason into the V1 ReaderExitNotifier (which the manager turns into
    // `Disconnected` / `Crashed`).
    if let Some(notifier) = exit_notifier {
        let workspace_id = fallback_workspace_id;
        let rpc_for_exit = Arc::clone(&rpc);
        tokio::spawn(async move {
            let Some(rx) = rpc_for_exit.take_exit_signal().await else {
                return;
            };
            let reason = rx.await.ok().flatten();
            notifier.notify_exit(workspace_id, reason);
        });
    }
}

/// Spawn the stderr forwarder task — mirrors the legacy
/// `tokio::spawn` block in `setup_session_runtime` that emits
/// `codex/stderr` AppServerEvents from a child process.
pub(crate) fn start_stderr_forwarder<E: EventSink>(
    mut stderr_rx: mpsc::UnboundedReceiver<String>,
    workspace_id: String,
    event_sink: E,
) {
    tokio::spawn(async move {
        while let Some(line) = stderr_rx.recv().await {
            let payload = AppServerEvent {
                workspace_id: workspace_id.clone(),
                message: json!({
                    "method": "codex/stderr",
                    "params": { "message": line },
                }),
            };
            event_sink.emit_app_server_event(payload);
        }
    });
}

/// Post-process a response that just came back from `rpc.request`.
///
/// Replicates the response-side branches of the legacy reader loop:
/// - Map `extract_related_thread_ids` (or `extract_thread_id`) on the
///   response value to the requesting workspace.
/// - For `thread/list` responses, parse the entries and update routing
///   (workspace mapping, hidden threads).
pub(crate) async fn record_response(
    routing: &Arc<SessionRouting>,
    workspace_id: &str,
    method: &str,
    response: &Value,
) {
    let related_thread_ids = extract_related_thread_ids(response);
    if !related_thread_ids.is_empty() {
        let mut tw = routing.thread_workspace.lock().await;
        for tid in related_thread_ids {
            tw.insert(tid, workspace_id.to_string());
        }
    } else if let Some(thread_id) = extract_thread_id(response) {
        routing
            .map_thread_to_workspace(&thread_id, workspace_id)
            .await;
    }

    if method == "thread/list" {
        let thread_entries = extract_thread_entries_from_thread_list_result(response);
        if !thread_entries.is_empty() {
            let workspace_roots = routing.workspace_roots.lock().await.clone();
            let mut hidden_thread_ids: Vec<String> = Vec::new();
            let mut tw = routing.thread_workspace.lock().await;
            for entry in thread_entries {
                if entry.is_memory_consolidation {
                    tw.remove(&entry.thread_id);
                    hidden_thread_ids.push(entry.thread_id);
                    continue;
                }
                let mapped_workspace = entry
                    .cwd
                    .as_deref()
                    .and_then(|cwd| resolve_workspace_for_cwd(cwd, &workspace_roots));
                if let Some(workspace_id) = mapped_workspace {
                    tw.insert(entry.thread_id, workspace_id);
                }
            }
            drop(tw);
            if !hidden_thread_ids.is_empty() {
                let mut hidden = routing.hidden_thread_ids.lock().await;
                for thread_id in hidden_thread_ids {
                    hidden.insert(thread_id);
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Notification dispatcher
// ──────────────────────────────────────────────────────────────────────

async fn dispatch_notification<E: EventSink>(
    value: Value,
    routing: &Arc<SessionRouting>,
    fallback_workspace_id: &str,
    event_sink: &E,
) {
    // `codex/parseError` notifications are minted by `CodexRpcClient::dispatch_message`
    // when the underlying line failed to parse.  We forward them with the
    // owner workspace_id so the UI's existing handler keeps working.
    let method_name = value.get("method").and_then(|m| m.as_str());
    let thread_id = extract_thread_id(&value);

    let mapped_thread_workspace = if let Some(ref tid) = thread_id {
        routing.resolve_workspace_for_thread(tid).await
    } else {
        None
    };
    let routed_workspace_id = mapped_thread_workspace
        .clone()
        .unwrap_or_else(|| fallback_workspace_id.to_string());

    if let Some(ref tid) = thread_id {
        if method_name == Some("codex/backgroundThread") {
            let action = value
                .get("params")
                .and_then(|params| params.get("action"))
                .and_then(Value::as_str)
                .unwrap_or("hide");
            if action.eq_ignore_ascii_case("hide") {
                routing.mark_hidden_thread(tid).await;
            }
        } else if method_name == Some("thread/started")
            && thread_started_is_memory_consolidation(&value)
        {
            routing.mark_hidden_thread(tid).await;
            event_sink.emit_app_server_event(AppServerEvent {
                workspace_id: routed_workspace_id.clone(),
                message: json!({
                    "method": "codex/backgroundThread",
                    "params": {
                        "threadId": tid,
                        "action": "hide"
                    }
                }),
            });
            return;
        }

        // Notifications never carry `result`/`error`, so the second arg to
        // `should_suppress_hidden_thread_event` is always `false`.
        //
        // For hidden threads we suppress the UI event emit, but we must still
        // run the background-callback dispatch below — callbacks are exactly
        // how solo-agent / background-codex tasks consume hidden-thread events.
        if routing.is_hidden_thread(tid).await
            && should_suppress_hidden_thread_event(method_name, false)
        {
            // Hidden-thread notifications must still reach background
            // callbacks — solo / team agents consume hidden-thread events via
            // exactly this path. Only UI emission is suppressed.
            let maybe_tx = routing
                .background_thread_callbacks
                .lock()
                .await
                .get(tid)
                .cloned();
            if let Some(tx) = maybe_tx {
                let _ = tx.send(value.clone());
            }
            return;
        }
    }

    if matches!(method_name, Some("item/started") | Some("item/completed")) {
        let related_thread_ids = extract_related_thread_ids(&value);
        if !related_thread_ids.is_empty() {
            let mut thread_workspace = routing.thread_workspace.lock().await;
            for related_id in related_thread_ids {
                thread_workspace
                    .entry(related_id)
                    .or_insert_with(|| routed_workspace_id.clone());
            }
        }
    }

    if method_name == Some("thread/archived") {
        if let Some(ref tid) = thread_id {
            routing.forget_thread_workspace_mapping(tid).await;
            routing.forget_hidden_thread(tid).await;
        }
    }

    // Background-thread callback dispatch.  Clone the sender out under the
    // lock so we can drop the lock before `send` (the callback channel is
    // unbounded so this is just defensive).
    let mut sent_to_background = false;
    if let Some(ref tid) = thread_id {
        let maybe_tx = routing
            .background_thread_callbacks
            .lock()
            .await
            .get(tid)
            .cloned();
        if let Some(tx) = maybe_tx {
            let _ = tx.send(value.clone());
            sent_to_background = true;
        }
    }

    if !sent_to_background {
        if should_broadcast_global_workspace_notification(method_name, thread_id.as_ref(), None) {
            let workspace_ids = routing.workspace_ids_snapshot().await;
            if workspace_ids.is_empty() {
                event_sink.emit_app_server_event(AppServerEvent {
                    workspace_id: routed_workspace_id,
                    message: value,
                });
            } else {
                for workspace_id in workspace_ids {
                    event_sink.emit_app_server_event(AppServerEvent {
                        workspace_id,
                        message: value.clone(),
                    });
                }
            }
        } else {
            event_sink.emit_app_server_event(AppServerEvent {
                workspace_id: routed_workspace_id,
                message: value,
            });
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Parse helpers (moved verbatim from `backend::app_server`)
// ──────────────────────────────────────────────────────────────────────

/// Convenience for the request side: pulls a thread id directly from a
/// raw `params` value (not wrapped in `{"params": …}`).  Used by
/// `WorkspaceSession::send_request_for_workspace` to record the
/// `thread_id → workspace_id` mapping before the outgoing request hits
/// the wire.
pub(crate) fn extract_thread_id_from_params(params: &Value) -> Option<String> {
    extract_thread_id(&json!({ "params": params.clone() }))
}

pub(crate) fn extract_thread_id(value: &Value) -> Option<String> {
    fn extract_from_container(container: Option<&Value>) -> Option<String> {
        let container = container?;
        container
            .get("threadId")
            .or_else(|| container.get("thread_id"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                container
                    .get("thread")
                    .and_then(|thread| thread.get("id"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
    }

    extract_from_container(value.get("params"))
        .or_else(|| extract_from_container(value.get("result")))
}

fn push_thread_id(out: &mut Vec<String>, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    if let Some(thread_id) = value.as_str().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        out.push(thread_id.to_string());
        return;
    }
    if let Some(values) = value.as_array() {
        for entry in values {
            push_thread_id(out, Some(entry));
        }
    }
}

pub(crate) fn extract_related_thread_ids(value: &Value) -> Vec<String> {
    fn collect_agent_thread_ids(value: Option<&Value>, out: &mut Vec<String>) {
        let Some(value) = value else {
            return;
        };
        if let Some(values) = value.as_array() {
            for entry in values {
                collect_agent_thread_ids(Some(entry), out);
            }
            return;
        }
        let Some(record) = value.as_object() else {
            return;
        };
        push_thread_id(
            out,
            record.get("threadId").or_else(|| record.get("thread_id")),
        );
        push_thread_id(out, record.get("id"));
        push_thread_id(
            out,
            record.get("thread").and_then(|thread| {
                thread
                    .get("id")
                    .or_else(|| thread.get("threadId"))
                    .or_else(|| thread.get("thread_id"))
            }),
        );
    }

    fn collect_from_container(container: Option<&Value>, out: &mut Vec<String>) {
        let Some(container) = container.and_then(|value| value.as_object()) else {
            return;
        };
        push_thread_id(
            out,
            container.get("threadId").or_else(|| container.get("thread_id")),
        );
        push_thread_id(
            out,
            container.get("thread").and_then(|thread| thread.get("id")),
        );
        push_thread_id(
            out,
            container
                .get("params")
                .and_then(|params| params.get("threadId").or_else(|| params.get("thread_id"))),
        );
        push_thread_id(
            out,
            container
                .get("result")
                .and_then(|result| result.get("threadId").or_else(|| result.get("thread_id"))),
        );
        push_thread_id(
            out,
            container
                .get("newThreadId")
                .or_else(|| container.get("new_thread_id")),
        );
        push_thread_id(
            out,
            container
                .get("receiverThreadId")
                .or_else(|| container.get("receiver_thread_id")),
        );
        push_thread_id(
            out,
            container
                .get("receiverThreadIds")
                .or_else(|| container.get("receiver_thread_ids")),
        );
        collect_agent_thread_ids(
            container
                .get("receiverAgents")
                .or_else(|| container.get("receiver_agents")),
            out,
        );
        collect_agent_thread_ids(
            container
                .get("receiverAgent")
                .or_else(|| container.get("receiver_agent")),
            out,
        );
        collect_agent_thread_ids(
            container
                .get("agentStatuses")
                .or_else(|| container.get("agent_statuses")),
            out,
        );
        if let Some(status_map) = container.get("statuses").and_then(|value| value.as_object()) {
            out.extend(
                status_map
                    .keys()
                    .map(|key| key.trim().to_string())
                    .filter(|key| !key.is_empty()),
            );
        }
        if let Some(item) = container.get("item") {
            collect_from_container(Some(item), out);
        }
    }

    let mut out = Vec::new();
    collect_from_container(value.get("params"), &mut out);
    collect_from_container(value.get("result"), &mut out);
    collect_from_container(Some(value), &mut out);

    let mut seen = HashSet::new();
    out.into_iter()
        .filter(|thread_id| seen.insert(thread_id.clone()))
        .collect()
}

#[derive(Debug, Clone)]
pub(crate) struct ThreadListEntry {
    pub(crate) thread_id: String,
    pub(crate) cwd: Option<String>,
    pub(crate) is_memory_consolidation: bool,
}

pub(crate) fn extract_thread_entries_from_thread_list_result(value: &Value) -> Vec<ThreadListEntry> {
    fn collect_entries(input: &Value, out: &mut Vec<ThreadListEntry>) {
        if let Some(values) = input.as_array() {
            for value in values {
                collect_entries(value, out);
            }
            return;
        }
        let Some(object) = input.as_object() else {
            return;
        };

        let cwd = object
            .get("cwd")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .or_else(|| {
                object
                    .get("thread")
                    .and_then(|thread| thread.get("cwd"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string())
            });

        let thread_id = object
            .get("threadId")
            .or_else(|| object.get("thread_id"))
            .or_else(|| object.get("id"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .or_else(|| {
                object
                    .get("thread")
                    .and_then(|thread| thread.get("id"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string())
            });
        if let Some(thread_id) = thread_id {
            let source = object
                .get("source")
                .or_else(|| object.get("thread").and_then(|thread| thread.get("source")));
            let is_memory_consolidation = source
                .and_then(source_subagent_kind)
                .is_some_and(|kind| kind == "memory_consolidation");
            out.push(ThreadListEntry {
                thread_id,
                cwd,
                is_memory_consolidation,
            });
        }

        for key in ["threads", "items", "results", "data"] {
            if let Some(values) = object.get(key).and_then(|value| value.as_array()) {
                for value in values {
                    collect_entries(value, out);
                }
            }
        }
    }

    let mut out = Vec::new();
    if let Some(result) = value.get("result") {
        collect_entries(result, &mut out);
    }
    out
}

pub(crate) fn resolve_workspace_for_cwd(
    cwd: &str,
    workspace_roots: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let normalized_cwd = normalize_root_path(cwd);
    if normalized_cwd.is_empty() {
        return None;
    }
    workspace_roots
        .iter()
        .filter_map(|(workspace_id, root)| {
            if root.is_empty() {
                return None;
            }
            let is_exact_match = root == &normalized_cwd;
            let is_nested_match = normalized_cwd.len() > root.len()
                && normalized_cwd.starts_with(root)
                && normalized_cwd.as_bytes().get(root.len()) == Some(&b'/');
            if is_exact_match || is_nested_match {
                Some((workspace_id, root.len()))
            } else {
                None
            }
        })
        .max_by_key(|(_, root_len)| *root_len)
        .map(|(workspace_id, _)| workspace_id.clone())
}

fn normalize_subagent_kind(value: &str) -> String {
    let mut normalized = value.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    if let Some(stripped) = normalized.strip_prefix("subagent_") {
        normalized = stripped.to_string();
    } else if let Some(stripped) = normalized.strip_prefix("sub_agent_") {
        normalized = stripped.to_string();
    }
    normalized
}

pub(crate) fn source_subagent_kind(source: &Value) -> Option<String> {
    if let Some(raw) = source.as_str() {
        let normalized = normalize_subagent_kind(raw);
        return if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
    }
    let source_obj = source.as_object()?;
    let sub_agent = source_obj
        .get("subAgent")
        .or_else(|| source_obj.get("sub_agent"))
        .or_else(|| source_obj.get("subagent"))?;

    if let Some(raw) = sub_agent.as_str() {
        let normalized = normalize_subagent_kind(raw);
        return if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
    }
    let sub_agent_obj = sub_agent.as_object()?;
    if let Some(explicit) = sub_agent_obj
        .get("kind")
        .or_else(|| sub_agent_obj.get("type"))
        .or_else(|| sub_agent_obj.get("name"))
        .or_else(|| sub_agent_obj.get("id"))
        .and_then(Value::as_str)
    {
        let normalized = normalize_subagent_kind(explicit);
        return if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
    }

    let candidate_keys: Vec<&String> = sub_agent_obj
        .keys()
        .filter(|key| key.as_str() != "thread_spawn" && key.as_str() != "threadSpawn")
        .collect();
    if candidate_keys.len() != 1 {
        return None;
    }
    let normalized = normalize_subagent_kind(candidate_keys[0]);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub(crate) fn thread_started_is_memory_consolidation(value: &Value) -> bool {
    value
        .get("params")
        .and_then(|params| {
            params
                .get("thread")
                .and_then(|thread| thread.get("source"))
                .or_else(|| params.get("source"))
        })
        .and_then(source_subagent_kind)
        .is_some_and(|kind| kind == "memory_consolidation")
}

pub(crate) fn should_suppress_hidden_thread_event(
    method_name: Option<&str>,
    has_result_or_error: bool,
) -> bool {
    !has_result_or_error
        && !matches!(
            method_name,
            Some("thread/archived") | Some("codex/backgroundThread")
        )
}

fn is_global_workspace_notification(method: &str) -> bool {
    matches!(
        method,
        "account/updated" | "account/rateLimits/updated" | "account/login/completed"
    )
}

pub(crate) fn should_broadcast_global_workspace_notification(
    method_name: Option<&str>,
    thread_id: Option<&String>,
    request_workspace: Option<&str>,
) -> bool {
    method_name.is_some_and(is_global_workspace_notification)
        && thread_id.is_none()
        && request_workspace.is_none()
}

// ──────────────────────────────────────────────────────────────────────
// Tests (moved from `backend::app_server::tests`)
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn extract_thread_id_reads_camel_case() {
        let value = json!({ "params": { "threadId": "thread-123" } });
        assert_eq!(extract_thread_id(&value), Some("thread-123".to_string()));
    }

    #[test]
    fn extract_thread_id_reads_snake_case() {
        let value = json!({ "params": { "thread_id": "thread-456" } });
        assert_eq!(extract_thread_id(&value), Some("thread-456".to_string()));
    }

    #[test]
    fn extract_thread_id_reads_hook_notification_thread_id() {
        let value = json!({
            "method": "hook/started",
            "params": {
                "threadId": "thread-hook-1",
                "run": { "id": "hook-1" }
            }
        });
        assert_eq!(extract_thread_id(&value), Some("thread-hook-1".to_string()));
    }

    #[test]
    fn extract_thread_id_returns_none_when_missing() {
        let value = json!({ "params": {} });
        assert_eq!(extract_thread_id(&value), None);
    }

    #[test]
    fn extract_thread_entries_reads_result_data_items() {
        let value = json!({
            "result": {
                "data": [
                    { "id": "thread-a", "cwd": "/tmp/a" },
                    {
                        "threadId": "thread-b",
                        "cwd": "/tmp/b",
                        "source": { "subAgent": "memory_consolidation" }
                    }
                ]
            }
        });
        let entries = extract_thread_entries_from_thread_list_result(&value);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].thread_id, "thread-a");
        assert_eq!(entries[0].cwd.as_deref(), Some("/tmp/a"));
        assert!(!entries[0].is_memory_consolidation);
        assert_eq!(entries[1].thread_id, "thread-b");
        assert_eq!(entries[1].cwd.as_deref(), Some("/tmp/b"));
        assert!(entries[1].is_memory_consolidation);
    }

    #[test]
    fn extract_related_thread_ids_reads_spawn_hints_from_item_payloads() {
        let value = json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-parent",
                "item": {
                    "type": "mcpToolCall",
                    "new_thread_id": "thread-child"
                }
            }
        });
        let ids = extract_related_thread_ids(&value);
        assert!(ids.contains(&"thread-parent".to_string()));
        assert!(ids.contains(&"thread-child".to_string()));
    }

    #[test]
    fn extract_related_thread_ids_reads_receiver_agent_references() {
        let value = json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-parent",
                "item": {
                    "type": "collabToolCall",
                    "receiver_agents": [
                        { "thread_id": "thread-child-a" },
                        { "thread": { "id": "thread-child-b" } }
                    ],
                    "statuses": {
                        "thread-child-c": { "status": "running" }
                    }
                }
            }
        });
        let ids = extract_related_thread_ids(&value);
        assert!(ids.contains(&"thread-parent".to_string()));
        assert!(ids.contains(&"thread-child-a".to_string()));
        assert!(ids.contains(&"thread-child-b".to_string()));
        assert!(ids.contains(&"thread-child-c".to_string()));
    }

    #[test]
    fn extract_related_thread_ids_reads_singular_receiver_agent_reference() {
        let value = json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-parent",
                "item": {
                    "type": "mcpToolCall",
                    "receiver_agent": { "thread_id": "thread-child-single" }
                }
            }
        });
        let ids = extract_related_thread_ids(&value);
        assert!(ids.contains(&"thread-parent".to_string()));
        assert!(ids.contains(&"thread-child-single".to_string()));
    }

    #[test]
    fn resolve_workspace_for_cwd_normalizes_windows_paths() {
        let mut roots = HashMap::new();
        roots.insert("ws-1".to_string(), normalize_root_path("C:\\Dev\\Codex"));
        assert_eq!(
            resolve_workspace_for_cwd("c:/dev/codex", &roots),
            Some("ws-1".to_string())
        );
    }

    #[test]
    fn resolve_workspace_for_cwd_normalizes_windows_namespace_paths() {
        let mut roots = HashMap::new();
        roots.insert("ws-1".to_string(), normalize_root_path("C:\\Dev\\Codex"));
        assert_eq!(
            resolve_workspace_for_cwd("\\\\?\\C:\\Dev\\Codex", &roots),
            Some("ws-1".to_string())
        );
    }

    #[test]
    fn resolve_workspace_for_cwd_matches_nested_paths() {
        let mut roots = HashMap::new();
        roots.insert("ws-1".to_string(), normalize_root_path("/tmp/codex"));
        assert_eq!(
            resolve_workspace_for_cwd("/tmp/codex/subdir/project", &roots),
            Some("ws-1".to_string())
        );
    }

    #[test]
    fn resolve_workspace_for_cwd_prefers_longest_matching_root() {
        let mut roots = HashMap::new();
        roots.insert("ws-parent".to_string(), normalize_root_path("/tmp/codex"));
        roots.insert(
            "ws-child".to_string(),
            normalize_root_path("/tmp/codex/subdir"),
        );
        assert_eq!(
            resolve_workspace_for_cwd("/tmp/codex/subdir/project", &roots),
            Some("ws-child".to_string())
        );
    }

    #[test]
    fn source_subagent_kind_reads_string_variants() {
        assert_eq!(
            source_subagent_kind(&json!("subagent-memory-consolidation")),
            Some("memory_consolidation".to_string())
        );
        assert_eq!(
            source_subagent_kind(&json!("sub_agent_memory_consolidation")),
            Some("memory_consolidation".to_string())
        );
    }

    #[test]
    fn source_subagent_kind_reads_nested_subagent_object_keys() {
        let source = json!({
            "subAgent": {
                "memory_consolidation": {
                    "thread_spawn": { "parent_thread_id": "thread-parent" }
                }
            }
        });
        assert_eq!(
            source_subagent_kind(&source),
            Some("memory_consolidation".to_string())
        );
    }

    #[test]
    fn thread_started_memory_consolidation_detects_thread_source() {
        let value = json!({
            "method": "thread/started",
            "params": {
                "thread": {
                    "id": "thread-1",
                    "source": { "subagent": "memory_consolidation" }
                }
            }
        });
        assert!(thread_started_is_memory_consolidation(&value));
    }

    #[test]
    fn thread_started_memory_consolidation_detects_params_source_fallback() {
        let value = json!({
            "method": "thread/started",
            "params": {
                "threadId": "thread-1",
                "source": { "subAgent": "memory_consolidation" }
            }
        });
        assert!(thread_started_is_memory_consolidation(&value));
    }

    #[test]
    fn thread_started_memory_consolidation_rejects_non_memory_subagent() {
        let value = json!({
            "method": "thread/started",
            "params": {
                "thread": {
                    "id": "thread-1",
                    "source": { "subAgent": "review" }
                }
            }
        });
        assert!(!thread_started_is_memory_consolidation(&value));
    }

    #[test]
    fn hidden_thread_suppression_allows_rpc_responses() {
        assert!(!should_suppress_hidden_thread_event(Some("thread/archived"), true));
        assert!(!should_suppress_hidden_thread_event(Some("thread/updated"), true));
        assert!(!should_suppress_hidden_thread_event(None, true));
    }

    #[test]
    fn hidden_thread_suppression_still_blocks_non_exempt_notifications() {
        assert!(should_suppress_hidden_thread_event(Some("thread/updated"), false));
        assert!(!should_suppress_hidden_thread_event(Some("thread/archived"), false));
        assert!(!should_suppress_hidden_thread_event(
            Some("codex/backgroundThread"),
            false
        ));
    }
}
