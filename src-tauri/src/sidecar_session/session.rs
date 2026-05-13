// Sidecar session — owns one `npx tsx sidecar/src/main.ts` child process and
// wraps it in a tiny JSON-RPC-ish request/response client.
//
// Phase 1 Spike A scope:
//   * single child per workspace
//   * ndjson request/response over stdin/stdout
//   * no push notifications, no reconnect, no health check, no graceful close
//   * stderr is forwarded to the host's stderr with a `[sidecar/<ws>]` tag
//
// Future phases will likely add a notification broadcast channel (mirrors
// `codex_transport::rpc_client`) and a typed protocol module shared with the
// sidecar TS source.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
struct SidecarErrorPayload {
    #[allow(dead_code)]
    code: Option<String>,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SidecarResponse {
    #[serde(rename = "type")]
    msg_type: String,
    id: String,
    ok: bool,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    error: Option<SidecarErrorPayload>,
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<SidecarResponse>>>>;

pub(crate) struct SidecarSession {
    pub(crate) workspace_id: String,
    pub(crate) workspace_path: String,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: PendingMap,
    next_id: AtomicU64,
}

impl SidecarSession {
    /// Spawn `npx tsx src/main.ts` inside the repo's `sidecar/` directory and
    /// wire up stdout/stderr reader tasks. Does NOT run the `init` op — caller
    /// is responsible for that (so the command layer can surface init errors
    /// distinctly from spawn errors).
    pub(crate) async fn spawn(
        workspace_id: String,
        workspace_path: String,
    ) -> Result<Arc<Self>, String> {
        let sidecar_dir = resolve_sidecar_dir()?;

        let mut command = tokio::process::Command::new("npx");
        command.args(["tsx", "src/main.ts"]);
        command.current_dir(&sidecar_dir);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn sidecar (npx tsx src/main.ts in {}): {}", sidecar_dir.display(), e))?;

        let stdin = child.stdin.take().ok_or_else(|| "sidecar stdin missing".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "sidecar stdout missing".to_string())?;
        let stderr = child.stderr.take().ok_or_else(|| "sidecar stderr missing".to_string())?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // stderr reader → host stderr with workspace tag
        {
            let ws = workspace_id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[sidecar/{}] {}", ws, line);
                }
            });
        }

        // stdout reader → demux to pending oneshots
        {
            let pending_for_stdout = pending.clone();
            let ws = workspace_id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<SidecarResponse>(trimmed) {
                        Ok(resp) if resp.msg_type == "res" => {
                            let tx_opt = {
                                let mut map = pending_for_stdout.lock().await;
                                map.remove(&resp.id)
                            };
                            if let Some(tx) = tx_opt {
                                let _ = tx.send(resp);
                            } else {
                                eprintln!(
                                    "[sidecar/{}] orphan response id={}",
                                    ws, resp.id
                                );
                            }
                        }
                        Ok(_) => eprintln!(
                            "[sidecar/{}] non-response message ignored: {}",
                            ws, trimmed
                        ),
                        Err(e) => eprintln!(
                            "[sidecar/{}] failed to parse response ({}): {}",
                            ws, e, trimmed
                        ),
                    }
                }
            });
        }

        Ok(Arc::new(Self {
            workspace_id,
            workspace_path,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
        }))
    }

    /// Send one request and await the matching response.
    pub(crate) async fn send_request(
        &self,
        op: &str,
        params: Option<Value>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst).to_string();

        let (tx, rx) = oneshot::channel::<SidecarResponse>();
        {
            let mut map = self.pending.lock().await;
            map.insert(id.clone(), tx);
        }

        let mut envelope = serde_json::json!({
            "type": "req",
            "id": id,
            "op": op,
        });
        if let Some(p) = params {
            envelope["params"] = p;
        }
        let mut body = envelope.to_string();
        body.push('\n');

        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(body.as_bytes())
                .await
                .map_err(|e| format!("sidecar stdin write failed: {}", e))?;
            stdin
                .flush()
                .await
                .map_err(|e| format!("sidecar stdin flush failed: {}", e))?;
        }

        let resp = match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                return Err("sidecar dropped response channel (child likely exited)".into());
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(format!("sidecar request `{}` timed out", op));
            }
        };

        if resp.ok {
            Ok(resp.data.unwrap_or(Value::Null))
        } else {
            Err(resp
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "unknown sidecar error (ok=false without error field)".into()))
        }
    }

    /// Hard-kill the child. Spike scope: no graceful shutdown protocol.
    pub(crate) async fn kill(&self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

fn resolve_sidecar_dir() -> Result<PathBuf, String> {
    // <repo>/CodexMonitor/src-tauri/../../sidecar  →  <repo>/sidecar
    let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sidecar");
    std::fs::canonicalize(&candidate).map_err(|e| {
        format!(
            "sidecar dir not found at {} ({}). Phase 1 Spike A expects the repo layout `<repo>/sidecar/`.",
            candidate.display(),
            e
        )
    })
}
