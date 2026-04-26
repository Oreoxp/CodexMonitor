//! WebSocket-based transport for communicating with an externally-managed
//! `codex app-server`.
//!
//! Unlike the stdio transport, this transport does **not** spawn or own the
//! Codex process — it simply connects as a WebSocket client to a running
//! `codex app-server --listen ws://...`. In the OpenCrab3 architecture, that
//! server is started separately by `start-backend.sh` (default
//! `ws://127.0.0.1:9000`), and it can be shared by multiple workspace
//! sessions. This is also what makes the WebSocket path useful for our
//! product: the Tauri app does not need `codex` on the user's `$PATH`.
//!
//! JSON-RPC messages are exchanged as WebSocket Text frames, identical to
//! the framing the stdio transport uses for stdin/stdout.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use super::transport::{CodexTransport, CodexTransportKind, TransportError};

/// Stderr line receiver — kept for API parity with `StdioTransport`. The
/// WebSocket path doesn't own the child process, so this channel never
/// receives anything (its sender is dropped immediately).
pub(crate) type StderrReceiver = mpsc::UnboundedReceiver<String>;

/// A transport that talks to an externally-managed `codex app-server` via
/// WebSocket.
pub(crate) struct WebSocketTransport {
    /// Sender for outgoing WebSocket Text messages (consumed by the writer task).
    ws_tx: Mutex<mpsc::UnboundedSender<String>>,
    /// Receiver for incoming WebSocket Text messages (fed by the reader task).
    ws_rx: Mutex<mpsc::UnboundedReceiver<String>>,
    /// Liveness flag — set to `false` when either the reader or the writer
    /// task observes a closed/errored stream.
    alive: Arc<AtomicBool>,
    /// The URL we connected to (informational; useful for log/error output).
    #[allow(dead_code)]
    url: String,
}

impl WebSocketTransport {
    /// Connect to an externally-running `codex app-server` over WebSocket.
    ///
    /// Returns the transport plus a permanently-empty stderr receiver
    /// (the child process — if any — is not ours to manage).
    pub(crate) async fn connect(url: &str) -> Result<(Self, StderrReceiver), TransportError> {
        let (ws_stream, _response) = tokio_tungstenite::connect_async(url).await.map_err(|e| {
            TransportError::connection_failed(format!(
                "failed to connect to codex app-server at {url}: {e}. \
                 Make sure the backend is running (`./start-backend.sh`) \
                 or set XIAOPANGXIE_CODEX_TRANSPORT=stdio."
            ))
        })?;

        let alive = Arc::new(AtomicBool::new(true));
        let (mut ws_writer, mut ws_reader) = ws_stream.split();

        // Outgoing channel: callers `send` JSON strings into ws_out_tx, the
        // writer task forwards them as Text frames.
        let (ws_out_tx, mut ws_out_rx) = mpsc::unbounded_channel::<String>();
        {
            let alive = alive.clone();
            tokio::spawn(async move {
                while let Some(text) = ws_out_rx.recv().await {
                    if ws_writer.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                alive.store(false, Ordering::Relaxed);
                let _ = ws_writer.close().await;
            });
        }

        // Incoming channel: the reader task converts Text frames to lines.
        let (ws_in_tx, ws_in_rx) = mpsc::unbounded_channel::<String>();
        {
            let alive = alive.clone();
            tokio::spawn(async move {
                while let Some(msg) = ws_reader.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            let text_str: &str = &text;
                            if text_str.trim().is_empty() {
                                continue;
                            }
                            if ws_in_tx.send(text.to_string()).is_err() {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {
                            // Ignore binary, ping, pong frames; tungstenite
                            // already replies to pings automatically.
                        }
                        Err(_) => break,
                    }
                }
                alive.store(false, Ordering::Relaxed);
            });
        }

        Ok((
            Self {
                ws_tx: Mutex::new(ws_out_tx),
                ws_rx: Mutex::new(ws_in_rx),
                alive,
                url: url.to_string(),
            },
            // Empty stderr — sender is dropped here so recv() will return None.
            mpsc::unbounded_channel::<String>().1,
        ))
    }
}

#[async_trait]
impl CodexTransport for WebSocketTransport {
    fn kind(&self) -> CodexTransportKind {
        CodexTransportKind::WebSocket
    }

    async fn send(&self, message: &str) -> Result<(), TransportError> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(TransportError::closed("WebSocket transport is closed"));
        }
        let tx = self.ws_tx.lock().await;
        tx.send(message.to_string())
            .map_err(|_| TransportError::closed("WebSocket writer closed"))
    }

    async fn recv(&self) -> Result<Option<String>, TransportError> {
        let mut rx = self.ws_rx.lock().await;
        Ok(rx.recv().await)
    }

    async fn close(&self) -> Result<(), TransportError> {
        // Closing the outgoing channel triggers the writer task to send a
        // WebSocket Close frame and exit, which in turn terminates the
        // reader task at the remote end.
        self.alive.store(false, Ordering::Relaxed);
        // Replace the sender with a dropped one so `send` returns Closed
        // and the writer task observes channel closure.
        let mut guard = self.ws_tx.lock().await;
        let (dead_tx, dead_rx) = mpsc::unbounded_channel::<String>();
        drop(dead_rx);
        *guard = dead_tx;
        Ok(())
    }

    async fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}
