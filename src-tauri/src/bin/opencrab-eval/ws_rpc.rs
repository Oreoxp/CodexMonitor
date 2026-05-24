// P6 Step 9 — Minimal JSON-RPC 2.0 over WebSocket for opencrab-eval.
//
// Scope: one short-lived connection per fixture, serial request/response,
// drain notifications between requests. NOT a general client — we don't
// need streaming subscribers, multi-thread routing, or reconnection. The
// production stack already has all that in `codex_transport::rpc_client`;
// here we just need enough to drive a Codex thread end to end against a
// freshly-spawned app-server.
//
// Wire format (codex-app-server):
//   request       { jsonrpc:"2.0", id, method, params }
//   response      { jsonrpc:"2.0", id, result | error }
//   notification  { jsonrpc:"2.0", method, params }   (no id)

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct RpcClient {
    socket: Socket,
    next_id: u64,
    /// Notifications received while waiting for a response; drained by
    /// `drain_notifications_until`.
    pending_notifs: Vec<(String, Value)>,
}

impl RpcClient {
    /// Open a WebSocket and run the MCP-style initialize / initialized
    /// handshake. Times out if the server doesn't respond.
    pub async fn connect(url: &str, client_version: &str) -> Result<Self, String> {
        let (socket, _resp) = connect_async(url)
            .await
            .map_err(|err| format!("connect_async({url}) failed: {err}"))?;
        let mut client = Self {
            socket,
            next_id: 1,
            pending_notifs: Vec::new(),
        };
        // Mirror production [`Tauri rpc_client::initialize`] handshake bytes
        // exactly: `clientInfo.name="codex_monitor"` + `capabilities.
        // experimentalApi=true`. Step 18 spike used a custom name + empty
        // capabilities and codex never spawned our MCP server; step 19
        // narrows down whether this handshake difference is the cause.
        let init_params = json!({
            "clientInfo": {
                "name": "codex_monitor",
                "title": "Codex Monitor",
                "version": client_version,
            },
            "capabilities": {
                "experimentalApi": true,
            },
        });
        let _ = client
            .request_with_timeout("initialize", init_params, Duration::from_secs(15))
            .await?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let envelope = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.socket
            .send(Message::Text(envelope.to_string()))
            .await
            .map_err(|err| format!("ws send {method}: {err}"))?;
        Ok(())
    }

    /// Send a request and block until the matching response. Any
    /// notifications received in the meantime are buffered for the next
    /// `drain_notifications_until` call.
    pub async fn request_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        budget: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if std::env::var("OPENCRAB_EVAL_DUMP_NOTIFS").is_ok() {
            eprintln!("[ws-dump] >> {method} id={id} params={}", params);
        }
        self.socket
            .send(Message::Text(envelope.to_string()))
            .await
            .map_err(|err| format!("ws send {method}: {err}"))?;

        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("ws {method} timed out after {budget:?}"));
            }
            let frame = match timeout(remaining, self.socket.next()).await {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(err))) => return Err(format!("ws recv error: {err}")),
                Ok(None) => return Err("ws closed unexpectedly".to_string()),
                Err(_) => return Err(format!("ws {method} timed out after {budget:?}")),
            };
            let text = match frame {
                Message::Text(t) => t,
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => return Err("ws closed mid-request".to_string()),
                Message::Frame(_) => continue,
            };
            let value: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(err) => {
                    return Err(format!("ws frame is not JSON: {err}; body={text}"));
                }
            };
            // Response to our request?
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = value.get("error") {
                    return Err(format!("ws {method} error: {err}"));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            // Notification — buffer for later.
            if let Some(notif_method) =
                value.get("method").and_then(Value::as_str).map(str::to_owned)
            {
                if value.get("id").is_none() {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    self.pending_notifs.push((notif_method, params));
                    continue;
                }
            }
            // Response to some OTHER id (e.g. our own initialize result we
            // skipped past) — ignore. Shouldn't happen in serial usage.
        }
    }

    /// Receive notifications until `stop_method` arrives (matched on the
    /// JSON-RPC `method` field) or `budget` elapses. Returns every
    /// notification observed, including the stop one. Buffered notifications
    /// from earlier requests are flushed first.
    pub async fn drain_notifications_until(
        &mut self,
        stop_method: &str,
        budget: Duration,
    ) -> Result<Vec<(String, Value)>, String> {
        let mut collected = std::mem::take(&mut self.pending_notifs);
        // Flush buffered first — may already contain the stop method.
        if collected.iter().any(|(m, _)| m == stop_method) {
            return Ok(collected);
        }
        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                dump_partial_on_timeout(stop_method, &collected);
                return Err(format!(
                    "drain_until({stop_method}) timed out after {budget:?}; collected={} \
                     notifications",
                    collected.len()
                ));
            }
            let frame = match timeout(remaining, self.socket.next()).await {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(err))) => return Err(format!("ws recv error: {err}")),
                Ok(None) => return Err("ws closed unexpectedly".to_string()),
                Err(_) => {
                    dump_partial_on_timeout(stop_method, &collected);
                    return Err(format!(
                        "drain_until({stop_method}) timed out after {budget:?}"
                    ));
                }
            };
            let text = match frame {
                Message::Text(t) => t,
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => return Err("ws closed mid-drain".to_string()),
                Message::Frame(_) => continue,
            };
            let value: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(err) => return Err(format!("ws frame is not JSON: {err}")),
            };
            // Notification?
            if let Some(method) = value.get("method").and_then(Value::as_str).map(str::to_owned) {
                if value.get("id").is_none() {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    let is_stop = method == stop_method;
                    collected.push((method, params));
                    if is_stop {
                        return Ok(collected);
                    }
                    continue;
                }
            }
            // Stale response to a previous request — ignore.
        }
    }

    pub async fn close(mut self) {
        let _ = self.socket.close(None).await;
    }
}

/// Step 22 diagnostic — when `OPENCRAB_EVAL_DUMP_NOTIFS` is set, print
/// the partial notification collection that drain_until accumulated before
/// timing out. Lets us see what happened during a silent 180s timeout
/// (model never closed the turn — was it spinning on reasoning? streaming
/// agent message? tool calls that hung?).
fn dump_partial_on_timeout(stop_method: &str, collected: &[(String, serde_json::Value)]) {
    if std::env::var("OPENCRAB_EVAL_DUMP_NOTIFS").is_err() {
        return;
    }
    eprintln!(
        "[notif-dump-timeout] drain_until({stop_method}) timed out with {} buffered notifs:",
        collected.len()
    );
    let mut by_method: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    for (m, _) in collected {
        *by_method.entry(m.as_str()).or_insert(0) += 1;
    }
    for (m, n) in &by_method {
        eprintln!("[notif-dump-timeout]   {n:5}x  {m}");
    }
    // Also surface any item/completed payload types — useful to know if the
    // model emitted function_call / mcpToolCall items mid-turn but never
    // turned/completed.
    for (method, params) in collected {
        if method != "item/completed" {
            continue;
        }
        let Some(item) = params.get("item") else { continue };
        let t = item.get("type").and_then(serde_json::Value::as_str).unwrap_or("?");
        let nm = item.get("name").or_else(|| item.get("tool"))
            .and_then(serde_json::Value::as_str).unwrap_or("");
        eprintln!("[notif-dump-timeout]   item/completed type={t} name/tool={nm}");
    }
}
