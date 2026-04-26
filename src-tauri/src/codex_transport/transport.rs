//! Transport trait and kind enum for Codex communication.
//!
//! `CodexTransport` defines the raw byte-level contract: send a line of JSON,
//! receive a line of JSON, and shut the channel down.
//!
//! `CodexTransportKind` is a non-exhaustive enum that tags which concrete
//! transport is in use, allowing higher layers to branch on capability.

use async_trait::async_trait;

/// Enumerates the concrete transport backends.
///
/// New variants can be added here as more backends are implemented.
/// The existing `Stdio` variant will be wired up when we migrate the
/// current `WorkspaceSession` stdio logic.  `WebSocket` is reserved
/// for the planned WebSocket integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexTransportKind {
    /// Communicate via stdin/stdout of a child process.
    Stdio,
    /// Communicate via a WebSocket connection (future).
    WebSocket,
}

impl std::fmt::Display for CodexTransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexTransportKind::Stdio => write!(f, "stdio"),
            CodexTransportKind::WebSocket => write!(f, "websocket"),
        }
    }
}

/// Core transport abstraction.
///
/// Implementations are responsible for framing: each `send` call transmits
/// exactly one JSON-RPC message, and each `recv` call returns exactly one
/// JSON-RPC message (or `None` when the stream ends).
///
/// All methods are `&self` — interior mutability (e.g. `Mutex<ChildStdin>`)
/// is the implementor's responsibility, matching the pattern used in the
/// existing `WorkspaceSession`.
#[async_trait]
pub(crate) trait CodexTransport: Send + Sync + 'static {
    /// Returns the kind of this transport.
    fn kind(&self) -> CodexTransportKind;

    /// Send a single JSON-RPC message (a complete JSON line).
    ///
    /// The implementation must ensure the message is newline-terminated
    /// and flushed.
    async fn send(&self, message: &str) -> Result<(), TransportError>;

    /// Receive the next JSON-RPC message.
    ///
    /// Returns `Ok(None)` when the underlying stream reaches EOF.
    async fn recv(&self) -> Result<Option<String>, TransportError>;

    /// Gracefully shut down the transport.
    ///
    /// After this call, further `send`/`recv` operations may return errors.
    async fn close(&self) -> Result<(), TransportError>;

    /// Check whether the underlying process/connection is still alive.
    async fn is_alive(&self) -> bool;
}

/// Errors that can occur at the transport layer.
#[derive(Debug, Clone)]
pub(crate) struct TransportError {
    pub(crate) kind: TransportErrorKind,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TransportErrorKind {
    /// The underlying I/O operation failed.
    Io,
    /// The transport has been closed or the remote end disconnected.
    Closed,
    /// A connection attempt failed (relevant for WebSocket).
    ConnectionFailed,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "transport error ({:?}): {}", self.kind, self.message)
    }
}

impl std::error::Error for TransportError {}

impl TransportError {
    pub(crate) fn io(message: impl Into<String>) -> Self {
        Self {
            kind: TransportErrorKind::Io,
            message: message.into(),
        }
    }

    pub(crate) fn closed(message: impl Into<String>) -> Self {
        Self {
            kind: TransportErrorKind::Closed,
            message: message.into(),
        }
    }

    pub(crate) fn connection_failed(message: impl Into<String>) -> Self {
        Self {
            kind: TransportErrorKind::ConnectionFailed,
            message: message.into(),
        }
    }
}
