//! JSON-RPC client built on top of `CodexTransport`.
//!
//! `CodexRpcClient` owns a transport and provides the standard JSON-RPC
//! request/response lifecycle:
//!
//! - **`request`**: Send a request and await its response via a oneshot channel.
//! - **`notify`**: Fire-and-forget notification (no `id` field).
//! - **`initialize`**: Perform the MCP-style `initialize` handshake.
//!
//! Incoming messages are dispatched by a background reader task:
//! - Responses (messages with `id` + `result`/`error`) are routed to the
//!   corresponding pending request.
//! - Notifications (messages with `method` but no `id`) are forwarded to
//!   a broadcast channel for subscribers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::time::timeout;

use super::transport::{CodexTransport, CodexTransportKind, TransportError};

/// Timeout for individual RPC requests.
const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Capacity of the notification broadcast channel.
const NOTIFICATION_CHANNEL_CAPACITY: usize = 256;

/// A JSON-RPC client that operates over any `CodexTransport`.
///
/// # Lifecycle
///
/// 1. Create with [`CodexRpcClient::new`], passing a boxed transport.
/// 2. Call [`CodexRpcClient::start`] to spawn the background reader task.
/// 3. Use [`request`], [`notify`], and [`initialize`] to communicate.
/// 4. Call [`shutdown`] when done.
pub(crate) struct CodexRpcClient {
    /// The underlying transport (send direction).
    transport: Arc<dyn CodexTransport>,
    /// Monotonically increasing request ID counter.
    next_id: AtomicU64,
    /// Map from request ID → oneshot sender for the response.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    /// Broadcast channel for server-initiated notifications.
    notification_tx: broadcast::Sender<Value>,
    /// Channel to signal the reader task to stop.
    shutdown_tx: mpsc::Sender<()>,
    /// Receiver end held to keep the channel alive; taken by `start`.
    shutdown_rx: Mutex<Option<mpsc::Receiver<()>>>,
}

impl CodexRpcClient {
    /// Create a new RPC client wrapping the given transport.
    ///
    /// The client is inert until [`start`] is called.
    pub(crate) fn new(transport: Box<dyn CodexTransport>) -> Self {
        let (notification_tx, _) = broadcast::channel(NOTIFICATION_CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        Self {
            transport: Arc::from(transport),
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            notification_tx,
            shutdown_tx,
            shutdown_rx: Mutex::new(Some(shutdown_rx)),
        }
    }

    /// Returns the transport kind for this client.
    pub(crate) fn transport_kind(&self) -> CodexTransportKind {
        self.transport.kind()
    }

    /// Subscribe to server-initiated notifications.
    ///
    /// Each subscriber gets its own receiver. Slow consumers will miss
    /// messages once the channel buffer is full (lagging).
    pub(crate) fn subscribe_notifications(&self) -> broadcast::Receiver<Value> {
        self.notification_tx.subscribe()
    }

    /// Spawn the background reader task that dispatches incoming messages.
    ///
    /// Must be called exactly once. Panics if called a second time.
    pub(crate) async fn start(&self) {
        let mut shutdown_rx = self
            .shutdown_rx
            .lock()
            .await
            .take()
            .expect("CodexRpcClient::start called more than once");

        let transport = Arc::clone(&self.transport);
        let pending = Arc::clone(&self.pending);
        let notification_tx = self.notification_tx.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = transport.recv() => {
                        match result {
                            Ok(Some(line)) => {
                                Self::dispatch_message(line, &pending, &notification_tx).await;
                            }
                            Ok(None) => {
                                // EOF — transport closed cleanly.
                                break;
                            }
                            Err(e) => {
                                eprintln!("[CodexRpcClient] reader error: {e}");
                                break;
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }

            // Drop all pending requests so callers get an error.
            let mut map = pending.lock().await;
            for (_, sender) in map.drain() {
                let _ = sender.send(json!({
                    "error": {
                        "code": -32000,
                        "message": "transport closed"
                    }
                }));
            }
        });
    }

    /// Send a JSON-RPC **request** and await the response.
    ///
    /// Returns the full response `Value` (which may contain `result` or `error`).
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();

        self.pending.lock().await.insert(id, tx);

        let message = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        let line = serde_json::to_string(&message).map_err(|e| TransportError::io(e.to_string()))?;
        if let Err(e) = self.transport.send(&line).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        match timeout(RPC_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(TransportError::closed("request canceled (sender dropped)")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(TransportError::io(format!(
                    "request '{method}' timed out after {}s",
                    RPC_REQUEST_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Send a JSON-RPC **notification** (fire-and-forget, no `id`).
    pub(crate) async fn notify(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), TransportError> {
        let message = if let Some(params) = params {
            json!({ "method": method, "params": params })
        } else {
            json!({ "method": method })
        };

        let line = serde_json::to_string(&message).map_err(|e| TransportError::io(e.to_string()))?;
        self.transport.send(&line).await
    }

    /// Perform the MCP-style `initialize` handshake.
    ///
    /// Sends an `initialize` request with client metadata and capability
    /// declarations, then follows up with an `initialized` notification.
    pub(crate) async fn initialize(
        &self,
        client_version: &str,
    ) -> Result<Value, TransportError> {
        let params = json!({
            "clientInfo": {
                "name": "codex_monitor",
                "title": "Codex Monitor",
                "version": client_version,
            },
            "capabilities": {
                "experimentalApi": true,
            },
        });

        let result = self.request("initialize", params).await?;
        self.notify("initialized", None).await?;
        Ok(result)
    }

    /// Gracefully shut down the client.
    ///
    /// Signals the reader task to stop and closes the underlying transport.
    pub(crate) async fn shutdown(&self) -> Result<(), TransportError> {
        let _ = self.shutdown_tx.send(()).await;
        self.transport.close().await
    }

    // ── internal ────────────────────────────────────────────────────────

    /// Dispatch a single incoming JSON line to the appropriate consumer.
    async fn dispatch_message(
        line: String,
        pending: &Mutex<HashMap<u64, oneshot::Sender<Value>>>,
        notification_tx: &broadcast::Sender<Value>,
    ) {
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[CodexRpcClient] failed to parse message: {e}");
                // Forward parse errors as a synthetic notification so the
                // upper layer can handle them (matches current behaviour).
                let error_notification = json!({
                    "method": "codex/parseError",
                    "params": { "error": e.to_string(), "raw": line },
                });
                let _ = notification_tx.send(error_notification);
                return;
            }
        };

        let has_id = value.get("id").and_then(|id| id.as_u64());
        let has_result_or_error = value.get("result").is_some() || value.get("error").is_some();

        // If this is a response to a pending request, resolve it.
        if let Some(id) = has_id {
            if has_result_or_error {
                if let Some(sender) = pending.lock().await.remove(&id) {
                    let _ = sender.send(value);
                    return;
                }
            }
        }

        // Everything else is a notification (server → client).
        let _ = notification_tx.send(value);
    }
}
