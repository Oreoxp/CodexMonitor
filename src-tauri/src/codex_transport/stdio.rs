//! Stdio-based transport for communicating with a Codex child process.
//!
//! `StdioTransport` spawns `codex app-server` as a child process and
//! communicates via newline-delimited JSON over stdin/stdout. Stderr
//! is forwarded through a caller-provided callback.
//!
//! This replaces the ad-hoc stdin/stdout handling that was previously
//! spread across `app_server::spawn_workspace_session`.

use std::path::PathBuf;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, Mutex};

use crate::backend::app_server::{build_codex_command_with_bin, check_codex_installation};
use crate::shared::process_core::kill_child_process_tree;

use super::transport::{CodexTransport, CodexTransportKind, TransportError};

/// A transport that communicates with a `codex app-server` child process
/// via stdin (send) and stdout (recv).
///
/// Stderr output is forwarded through an `mpsc` channel so the caller
/// can process it without blocking the transport.
pub(crate) struct StdioTransport {
    /// Handle used to write JSON lines to the child's stdin.
    stdin: Mutex<ChildStdin>,
    /// Receiver for stdout lines (fed by a background reader task).
    stdout_rx: Mutex<mpsc::UnboundedReceiver<String>>,
    /// The child process handle (used for cleanup).
    child: Mutex<Child>,
}

/// Stderr line receiver — callers can consume these to forward to the UI.
pub(crate) type StderrReceiver = mpsc::UnboundedReceiver<String>;

impl StdioTransport {
    /// Spawn a `codex app-server` child process and return a transport + stderr channel.
    ///
    /// # Arguments
    /// * `codex_bin` — Optional path to the `codex` binary.
    /// * `codex_args` — Optional extra CLI arguments.
    /// * `cwd` — Working directory for the child process.
    /// * `codex_home` — Optional `CODEX_HOME` environment variable override
    ///   passed to the child process so that the backend uses the same home
    ///   directory (defaults to `~/.opencrab`).
    ///
    /// # Returns
    /// A tuple of `(StdioTransport, StderrReceiver)`. The stderr receiver
    /// yields one `String` per line of stderr output.
    pub(crate) async fn spawn(
        codex_bin: Option<String>,
        codex_args: Option<&str>,
        cwd: &str,
        codex_home: Option<&PathBuf>,
    ) -> Result<(Self, StderrReceiver), TransportError> {
        // Verify installation first.
        let _ = check_codex_installation(codex_bin.clone())
            .await
            .map_err(|e| TransportError::io(e))?;

        let mut command = build_codex_command_with_bin(
            codex_bin,
            codex_args,
            vec!["app-server".to_string()],
        )
        .map_err(|e| TransportError::io(e))?;

        command.current_dir(cwd);
        if let Some(path) = codex_home {
            // codex-rs 的 `find_codex_home()` 只认 `CODEX_HOME`(不认 `OPENCRAB_HOME`),
            // 默认会 fallback 到 `~/.codex`,导致读不到我们写在 `~/.opencrab/config.toml`
            // 里的 provider / bearer token。这里显式把子进程指向 OpenCrab 的 home。
            command.env("CODEX_HOME", path);
        }
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| TransportError::io(e.to_string()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::io("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::io("missing stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TransportError::io("missing stderr"))?;

        // Spawn a task that reads stdout lines into an unbounded channel.
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if stdout_tx.send(line).is_err() {
                    break;
                }
            }
        });

        // Spawn a task that reads stderr lines into an unbounded channel.
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if stderr_tx.send(line).is_err() {
                    break;
                }
            }
        });

        Ok((
            Self {
                stdin: Mutex::new(stdin),
                stdout_rx: Mutex::new(stdout_rx),
                child: Mutex::new(child),
            },
            stderr_rx,
        ))
    }

    /// Obtain a mutable reference to the child process handle.
    ///
    /// This is used by `WorkspaceSession` for process lifecycle management
    /// (e.g. `kill_child_process_tree`).
    pub(crate) async fn kill_child(&self) {
        let mut child = self.child.lock().await;
        kill_child_process_tree(&mut child).await;
    }

    /// Check whether the child process is still alive.
    pub(crate) async fn child_is_alive(&self) -> bool {
        let mut child = self.child.lock().await;
        matches!(child.try_wait(), Ok(None))
    }
}

#[async_trait]
impl CodexTransport for StdioTransport {
    fn kind(&self) -> CodexTransportKind {
        CodexTransportKind::Stdio
    }

    async fn send(&self, message: &str) -> Result<(), TransportError> {
        let mut stdin = self.stdin.lock().await;
        let mut line = message.to_string();
        if !line.ends_with('\n') {
            line.push('\n');
        }
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| TransportError::io(e.to_string()))
    }

    async fn recv(&self) -> Result<Option<String>, TransportError> {
        let mut rx = self.stdout_rx.lock().await;
        // `recv()` returns None when the sender is dropped (EOF).
        Ok(rx.recv().await)
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.kill_child().await;
        Ok(())
    }

    async fn is_alive(&self) -> bool {
        self.child_is_alive().await
    }
}
