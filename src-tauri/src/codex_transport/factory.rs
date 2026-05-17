//! Transport factory: creates a connected transport with automatic fallback.
//!
//! `create_transport` is the single entry-point used by `app_server.rs`.
//! It reads the environment variable `XIAOPANGXIE_CODEX_TRANSPORT` to
//! determine the preferred transport (default: `websocket`).
//!
//! If the preferred transport fails to start or connect, it automatically
//! falls back to the other transport and emits a warning.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::stdio::StdioTransport;
use super::transport::{CodexTransport, CodexTransportKind};
use super::websocket::WebSocketTransport;

/// Environment variable to override the transport mode.
///
/// Values: `"websocket"` (default) or `"stdio"`.
const ENV_TRANSPORT: &str = "XIAOPANGXIE_CODEX_TRANSPORT";

/// Environment variable to override the WebSocket URL the transport connects
/// to. The default matches `start-backend.sh`.
const ENV_WS_URL: &str = "XIAOPANGXIE_CODEX_WS_URL";

/// Default WebSocket URL — matches `start-backend.sh`'s `CODEX_WS_PORT=9000`.
const DEFAULT_WS_URL: &str = "ws://127.0.0.1:9000";

/// Resolve the WebSocket URL the client should connect to.
fn resolve_ws_url() -> String {
    std::env::var(ENV_WS_URL).unwrap_or_else(|_| DEFAULT_WS_URL.to_string())
}

/// Result returned by the factory: the transport, a stderr channel,
/// and metadata about what was actually used.
pub(crate) struct TransportBundle {
    /// The connected transport.
    pub(crate) transport: Arc<dyn CodexTransport>,
    /// Stderr line receiver from the child process.
    pub(crate) stderr_rx: mpsc::UnboundedReceiver<String>,
    /// Which transport was actually used.
    pub(crate) kind: CodexTransportKind,
    /// If a fallback occurred, this contains the warning message.
    pub(crate) fallback_warning: Option<String>,
}

/// Resolve transport kind from a raw string value (env var, settings field, …).
///
/// Returns `None` if the value isn't a recognised transport name so the caller
/// can fall through to the next preference layer.
fn parse_transport_kind(value: Option<&str>) -> Option<CodexTransportKind> {
    match value {
        Some(v) if v.eq_ignore_ascii_case("stdio") => Some(CodexTransportKind::Stdio),
        Some(v) if v.eq_ignore_ascii_case("websocket") || v.eq_ignore_ascii_case("ws") => {
            Some(CodexTransportKind::WebSocket)
        }
        _ => None,
    }
}

/// Resolve the active transport kind, with a layered preference order:
/// 1. The `XIAOPANGXIE_CODEX_TRANSPORT` env var (overrides everything — useful
///    for ops/debugging without touching the persisted settings file).
/// 2. The caller-supplied `setting_value` (from `AppSettings.transport_mode`).
/// 3. Default: WebSocket.
fn resolve_transport_kind(
    env_val: Option<&str>,
    setting_value: Option<CodexTransportKind>,
) -> CodexTransportKind {
    if let Some(kind) = parse_transport_kind(env_val) {
        return kind;
    }
    setting_value.unwrap_or(CodexTransportKind::WebSocket)
}

/// Create a connected transport, with automatic fallback.
///
/// Preference order: env var → caller setting → WebSocket default. If the
/// preferred transport fails to start, falls back to the other transport and
/// records a warning in the returned [`TransportBundle`].
///
/// Both the transport and its stderr channel are returned so the caller
/// can wire them into the session runtime.
pub(crate) async fn create_transport(
    codex_bin: Option<String>,
    codex_args: Option<&str>,
    cwd: &str,
    codex_home: Option<&PathBuf>,
    setting_kind: Option<CodexTransportKind>,
) -> Result<TransportBundle, String> {
    let preferred =
        resolve_transport_kind(std::env::var(ENV_TRANSPORT).ok().as_deref(), setting_kind);

    let ws_url = resolve_ws_url();
    // `codex_bin`/`codex_args`/`codex_home`/`cwd` are only consumed by the
    // stdio transport. The WebSocket transport connects to an
    // externally-managed server (started via `start-backend.sh`) and ignores
    // them.

    match preferred {
        CodexTransportKind::WebSocket => {
            match WebSocketTransport::connect(&ws_url).await {
                Ok((transport, stderr_rx)) => {
                    eprintln!("[TransportFactory] using WebSocket transport ({ws_url})");
                    Ok(TransportBundle {
                        transport: Arc::new(transport),
                        stderr_rx,
                        kind: CodexTransportKind::WebSocket,
                        fallback_warning: None,
                    })
                }
                Err(ws_err) => {
                    let warning =
                        format!("WebSocket transport failed ({ws_err}), falling back to stdio");
                    eprintln!("[TransportFactory] {warning}");

                    // Fallback to Stdio so the app stays usable when the
                    // backend daemon hasn't been started yet.
                    let (transport, stderr_rx) =
                        StdioTransport::spawn(codex_bin, codex_args, cwd, codex_home)
                            .await
                            .map_err(|e| format!("stdio fallback also failed: {e}"))?;

                    Ok(TransportBundle {
                        transport: Arc::new(transport),
                        stderr_rx,
                        kind: CodexTransportKind::Stdio,
                        fallback_warning: Some(warning),
                    })
                }
            }
        }
        CodexTransportKind::Stdio => {
            match StdioTransport::spawn(codex_bin, codex_args, cwd, codex_home).await {
                Ok((transport, stderr_rx)) => {
                    eprintln!("[TransportFactory] using stdio transport");
                    Ok(TransportBundle {
                        transport: Arc::new(transport),
                        stderr_rx,
                        kind: CodexTransportKind::Stdio,
                        fallback_warning: None,
                    })
                }
                Err(stdio_err) => {
                    let warning =
                        format!("stdio transport failed ({stdio_err}), falling back to WebSocket");
                    eprintln!("[TransportFactory] {warning}");

                    let (transport, stderr_rx) = WebSocketTransport::connect(&ws_url)
                        .await
                        .map_err(|e| format!("WebSocket fallback also failed: {e}"))?;

                    Ok(TransportBundle {
                        transport: Arc::new(transport),
                        stderr_rx,
                        kind: CodexTransportKind::WebSocket,
                        fallback_warning: Some(warning),
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests use the pure `resolve_transport_kind` function to avoid
    // env-var race conditions when tests run in parallel.

    #[test]
    fn default_transport_is_websocket() {
        assert_eq!(
            resolve_transport_kind(None, None),
            CodexTransportKind::WebSocket
        );
    }

    #[test]
    fn empty_env_defers_to_setting_or_default() {
        assert_eq!(
            resolve_transport_kind(Some(""), None),
            CodexTransportKind::WebSocket
        );
        assert_eq!(
            resolve_transport_kind(Some(""), Some(CodexTransportKind::Stdio)),
            CodexTransportKind::Stdio
        );
    }

    #[test]
    fn env_stdio_selects_stdio() {
        assert_eq!(
            resolve_transport_kind(Some("stdio"), None),
            CodexTransportKind::Stdio
        );
    }

    #[test]
    fn env_websocket_selects_websocket() {
        assert_eq!(
            resolve_transport_kind(Some("websocket"), None),
            CodexTransportKind::WebSocket
        );
    }

    #[test]
    fn env_ws_alias_selects_websocket() {
        assert_eq!(
            resolve_transport_kind(Some("ws"), None),
            CodexTransportKind::WebSocket
        );
    }

    #[test]
    fn env_case_insensitive() {
        assert_eq!(
            resolve_transport_kind(Some("STDIO"), None),
            CodexTransportKind::Stdio
        );
        assert_eq!(
            resolve_transport_kind(Some("WebSocket"), None),
            CodexTransportKind::WebSocket
        );
        assert_eq!(
            resolve_transport_kind(Some("WS"), None),
            CodexTransportKind::WebSocket
        );
    }

    #[test]
    fn unknown_env_falls_through_to_setting() {
        assert_eq!(
            resolve_transport_kind(Some("grpc"), None),
            CodexTransportKind::WebSocket
        );
        assert_eq!(
            resolve_transport_kind(Some("grpc"), Some(CodexTransportKind::Stdio)),
            CodexTransportKind::Stdio
        );
    }

    #[test]
    fn env_overrides_setting() {
        assert_eq!(
            resolve_transport_kind(Some("stdio"), Some(CodexTransportKind::WebSocket)),
            CodexTransportKind::Stdio
        );
        assert_eq!(
            resolve_transport_kind(Some("websocket"), Some(CodexTransportKind::Stdio)),
            CodexTransportKind::WebSocket
        );
    }
}
