//! Codex Transport Abstraction Layer
//!
//! This module provides a unified transport abstraction for communicating with
//! the Codex backend. It decouples the JSON-RPC protocol logic from the
//! underlying transport mechanism (Stdio, WebSocket, etc.).
//!
//! # Two transport models
//!
//! - **Stdio** spawns its own `codex app-server` child process per
//!   `WorkspaceSession` and pipes JSON over stdin/stdout. Each session owns
//!   its child. Requires `codex` on `$PATH`.
//! - **WebSocket** connects to an *externally* managed `codex app-server`
//!   over `ws://…` (default `ws://127.0.0.1:9000`, started by
//!   `start-backend.sh`). The Tauri app is just a client; it does **not**
//!   need `codex` installed locally.
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │   TransportFactory                           │
//! │   env XIAOPANGXIE_CODEX_TRANSPORT            │
//! │   env XIAOPANGXIE_CODEX_WS_URL               │
//! │   default: WebSocket → ws://127.0.0.1:9000   │
//! │   fallback: Stdio                            │
//! └──────────┬───────────────────────────────────┘
//!            │ creates
//! ┌──────────▼───────────────┐
//! │   CodexTransport trait   │   send / recv / close / is_alive
//! └──────────┬───────────────┘
//!            │ impl
//!    ┌───────┴────────┐
//!    │  Stdio  │  WebSocket  │
//!    └─────────────────────┘
//! ```

pub(crate) mod factory;
mod rpc_client;
pub(crate) mod stdio;
mod transport;
pub(crate) mod websocket;

pub(crate) use factory::{create_transport, TransportBundle};
#[allow(unused_imports)]
pub(crate) use rpc_client::CodexRpcClient;
#[allow(unused_imports)]
pub(crate) use stdio::StdioTransport;
pub(crate) use transport::{CodexTransport, CodexTransportKind, TransportError};
#[allow(unused_imports)]
pub(crate) use websocket::WebSocketTransport;

use crate::types::TransportMode;

impl From<TransportMode> for CodexTransportKind {
    fn from(mode: TransportMode) -> Self {
        match mode {
            TransportMode::Stdio => CodexTransportKind::Stdio,
            TransportMode::WebSocket => CodexTransportKind::WebSocket,
        }
    }
}

impl From<&TransportMode> for CodexTransportKind {
    fn from(mode: &TransportMode) -> Self {
        match mode {
            TransportMode::Stdio => CodexTransportKind::Stdio,
            TransportMode::WebSocket => CodexTransportKind::WebSocket,
        }
    }
}
