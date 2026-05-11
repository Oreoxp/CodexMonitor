use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::codex_solo::{run_codex_task, CodexRunTaskRequest};
use crate::shared::process_core::{kill_child_process_tree, tokio_command};
use crate::state::AppState;

const LANGGRAPH_EVENT: &str = "opencrab://langgraph-event";
const SIDECAR_ENV: &str = "OPENCRAB_LANGGRAPH_SIDECAR";
const PING_ID: &str = "ping-1";
const PING_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Serialize)]
pub(crate) struct LangGraphSidecarPingResult {
    ok: bool,
    pong: bool,
    source: String,
    command: String,
    cwd: Option<String>,
    pid: Option<u32>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LangGraphSidecarInvokeResult {
    thread_id: String,
    request_id: String,
    ok: bool,
    completed: bool,
    data: Value,
}

#[derive(Debug, Deserialize)]
struct SoloRunIndex {
    threads: Option<Vec<SoloRunIndexEntry>>,
}

#[derive(Debug, Deserialize)]
struct SoloRunIndexEntry {
    thread_id: String,
    goal: Option<String>,
    status: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SoloRunSummary {
    thread_id: String,
    goal: String,
    title: String,
    status: String,
    created_at: Option<String>,
    updated_at: Option<String>,
    current_node: Option<String>,
    phase: Option<String>,
    workspace_id: Option<String>,
    codex_thread_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SoloRunEventsResult {
    thread_id: String,
    mission: Option<Value>,
    events: Vec<Value>,
    artifacts: Vec<String>,
    final_report_exists: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct SoloStepDetailResult {
    thread_id: String,
    node: String,
    conversation: Option<Value>,
    artifacts: Vec<String>,
    logs: Vec<String>,
    events: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LangGraphSidecarResumeAction {
    Approve,
    Reject,
}

#[derive(Debug, Serialize)]
pub(crate) struct LangGraphSidecarResumeInput {
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    feedback: Option<String>,
}

#[derive(Debug)]
struct SidecarResponse {
    completed: bool,
    data: Value,
}

#[derive(Debug, Default)]
struct SidecarSpawnContext {
    workspace_cwd: Option<PathBuf>,
    workspace_id: Option<String>,
}

#[derive(Debug)]
struct SidecarLaunch {
    source: String,
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

pub(crate) struct LangGraphSidecarHost {
    launch: SidecarLaunch,
}

impl LangGraphSidecarHost {
    pub(crate) fn resolve() -> Result<Self, String> {
        Ok(Self {
            launch: resolve_launch()?,
        })
    }

    pub(crate) async fn ping(&self) -> Result<LangGraphSidecarPingResult, String> {
        let mut child = self.spawn_child(&SidecarSpawnContext::default())?;
        let pid = child.id();

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open LangGraph sidecar stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open LangGraph sidecar stdout".to_string())?;

        let request = json!({
            "type": "req",
            "id": PING_ID,
            "op": "ping",
        });
        let mut request_line =
            serde_json::to_string(&request).map_err(|err| format!("encode ping failed: {err}"))?;
        request_line.push('\n');

        if let Err(err) =
            tokio::time::timeout(PING_TIMEOUT, stdin.write_all(request_line.as_bytes()))
                .await
                .map_err(|_| "timed out writing LangGraph sidecar ping".to_string())?
                .map_err(|err| format!("failed to write LangGraph sidecar ping: {err}"))
        {
            cleanup_child(&mut child).await;
            return Err(err);
        }
        drop(stdin);

        let mut lines = BufReader::new(stdout).lines();
        let response_result = tokio::time::timeout(PING_TIMEOUT, async {
            loop {
                let Some(line) = lines
                    .next_line()
                    .await
                    .map_err(|err| format!("failed to read LangGraph sidecar stdout: {err}"))?
                else {
                    return Err("LangGraph sidecar exited before ping response".to_string());
                };

                let value: Value = serde_json::from_str(&line).map_err(|err| {
                    format!("failed to parse LangGraph sidecar JSON line '{line}': {err}")
                })?;

                if value.get("type").and_then(Value::as_str) == Some("res")
                    && value.get("id").and_then(Value::as_str) == Some(PING_ID)
                {
                    return Ok(value);
                }
            }
        })
        .await
        .map_err(|_| "timed out waiting for LangGraph sidecar ping response".to_string());

        cleanup_child(&mut child).await;

        let response = response_result??;

        let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let pong = response
            .get("data")
            .and_then(|data| data.get("pong"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !ok || !pong {
            return Err(format!(
                "LangGraph sidecar ping returned unexpected response: {response}"
            ));
        }

        Ok(LangGraphSidecarPingResult {
            ok,
            pong,
            source: self.launch.source.clone(),
            command: self.launch.display_command(),
            cwd: self
                .launch
                .cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            pid,
        })
    }

    pub(crate) async fn invoke<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        goal: String,
        thread_id: Option<String>,
        workspace_cwd: PathBuf,
        workspace_id: Option<String>,
    ) -> Result<LangGraphSidecarInvokeResult, String> {
        let thread_id = normalize_thread_id(thread_id);
        let request_id = format!("invoke-{}", Uuid::new_v4());
        let request = json!({
            "type": "req",
            "id": request_id,
            "op": "invoke",
            "thread_id": thread_id,
            "workspace_id": workspace_id.clone().unwrap_or_default(),
            "input": {
                "goal": goal,
            },
        });
        let response = self
            .send_request(
                app,
                &SidecarSpawnContext {
                    workspace_cwd: Some(workspace_cwd),
                    workspace_id,
                },
                &request_id,
                "invoke",
                request,
            )
            .await?;

        Ok(LangGraphSidecarInvokeResult {
            thread_id,
            request_id,
            ok: true,
            completed: response.completed,
            data: response.data,
        })
    }

    pub(crate) async fn get_state<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        thread_id: String,
        workspace_cwd: PathBuf,
        workspace_id: Option<String>,
    ) -> Result<Value, String> {
        let thread_id = normalize_required_thread_id(thread_id)?;
        let request_id = format!("get-state-{}", Uuid::new_v4());
        let request = json!({
            "type": "req",
            "id": request_id,
            "op": "get_state",
            "thread_id": thread_id,
        });
        let response = self
            .send_request(
                app,
                &SidecarSpawnContext {
                    workspace_cwd: Some(workspace_cwd),
                    workspace_id,
                },
                &request_id,
                "get_state",
                request,
            )
            .await?;
        Ok(response.data)
    }

    pub(crate) async fn resume<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        thread_id: String,
        input: LangGraphSidecarResumeInput,
        workspace_cwd: PathBuf,
        workspace_id: Option<String>,
    ) -> Result<Value, String> {
        let thread_id = normalize_required_thread_id(thread_id)?;
        let request_id = format!("resume-{}", Uuid::new_v4());
        let request = json!({
            "type": "req",
            "id": request_id,
            "op": "resume",
            "thread_id": thread_id,
            "workspace_id": workspace_id.clone().unwrap_or_default(),
            "input": input,
        });
        let response = self
            .send_request(
                app,
                &SidecarSpawnContext {
                    workspace_cwd: Some(workspace_cwd),
                    workspace_id,
                },
                &request_id,
                "resume",
                request,
            )
            .await?;
        Ok(response.data)
    }

    pub(crate) async fn continue_run<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        thread_id: String,
        workspace_cwd: PathBuf,
        workspace_id: Option<String>,
    ) -> Result<Value, String> {
        let thread_id = normalize_required_thread_id(thread_id)?;
        let request_id = format!("continue-{}", Uuid::new_v4());
        let request = json!({
            "type": "req",
            "id": request_id,
            "op": "continue",
            "thread_id": thread_id,
            "workspace_id": workspace_id.clone().unwrap_or_default(),
        });
        let response = self
            .send_request(
                app,
                &SidecarSpawnContext {
                    workspace_cwd: Some(workspace_cwd),
                    workspace_id,
                },
                &request_id,
                "continue",
                request,
            )
            .await?;
        Ok(response.data)
    }

    async fn send_request<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        context: &SidecarSpawnContext,
        request_id: &str,
        op_label: &str,
        request: Value,
    ) -> Result<SidecarResponse, String> {
        let mut child = self.spawn_child(context)?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open LangGraph sidecar stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open LangGraph sidecar stdout".to_string())?;

        let mut request_line = serde_json::to_string(&request)
            .map_err(|err| format!("encode LangGraph sidecar {op_label} request failed: {err}"))?;
        request_line.push('\n');

        // Spawn a writer task driven by an mpsc channel so reader-side
        // handlers can post follow-up messages (e.g. host_call_result)
        // without contending on stdin.
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = write_rx.recv().await {
                if let Err(err) = stdin.write_all(line.as_bytes()).await {
                    eprintln!("LangGraph sidecar stdin write failed: {err}");
                    break;
                }
                if let Err(err) = stdin.flush().await {
                    eprintln!("LangGraph sidecar stdin flush failed: {err}");
                    break;
                }
            }
            drop(stdin);
        });

        if let Err(err) = write_tx.send(request_line) {
            let _ = writer.await;
            cleanup_child(&mut child).await;
            return Err(format!(
                "failed to enqueue LangGraph sidecar {op_label} request: {err}"
            ));
        }

        let app_for_dispatch = app.clone();
        let mut lines = BufReader::new(stdout).lines();
        let mut completed = false;
        let response_result = tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                let Some(line) = lines
                    .next_line()
                    .await
                    .map_err(|err| format!("failed to read LangGraph sidecar stdout: {err}"))?
                else {
                    return Err(format!(
                        "LangGraph sidecar exited before {op_label} response"
                    ));
                };

                let value: Value = serde_json::from_str(&line).map_err(|err| {
                    format!("failed to parse LangGraph sidecar JSON line '{line}': {err}")
                })?;

                match value.get("type").and_then(Value::as_str) {
                    Some("evt") => {
                        let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
                        app.emit(LANGGRAPH_EVENT, value.clone()).map_err(|err| {
                            format!("failed to emit LangGraph sidecar event: {err}")
                        })?;
                        if kind == "done" {
                            completed = true;
                        }
                        if kind == "error" {
                            let message = value
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("LangGraph sidecar emitted error event");
                            return Err(message.to_string());
                        }
                    }
                    Some("host_call") => {
                        dispatch_host_call(&app_for_dispatch, &write_tx, value).await;
                    }
                    Some("res")
                        if value.get("id").and_then(Value::as_str) == Some(request_id) =>
                    {
                        return Ok(value);
                    }
                    Some("res") => {}
                    Some(other) => {
                        return Err(format!(
                            "LangGraph sidecar emitted unsupported message type: {other}"
                        ));
                    }
                    None => {
                        return Err(format!(
                            "LangGraph sidecar message missing type field: {value}"
                        ));
                    }
                }
            }
        })
        .await
        .map_err(|_| {
            format!("timed out waiting for LangGraph sidecar {op_label} response")
        });

        drop(write_tx);
        let _ = writer.await;
        cleanup_child(&mut child).await;

        let response = response_result??;
        let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !ok {
            return Err(format!(
                "LangGraph sidecar {op_label} returned error response: {response}"
            ));
        }
        let data = response.get("data").cloned().unwrap_or(Value::Null);

        Ok(SidecarResponse { completed, data })
    }

    fn spawn_child(&self, context: &SidecarSpawnContext) -> Result<Child, String> {
        let mut command = tokio_command(&self.launch.program);
        command.args(&self.launch.args);
        if let Some(cwd) = &self.launch.cwd {
            command.current_dir(cwd);
        }
        if let Some(workspace_cwd) = &context.workspace_cwd {
            command.env("OPENCRAB_WORKSPACE_CWD", workspace_cwd);
        }
        if let Some(workspace_id) = &context.workspace_id {
            if !workspace_id.is_empty() {
                command.env("OPENCRAB_WORKSPACE_ID", workspace_id);
            }
        }
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        command.spawn().map_err(|err| {
            format!(
                "failed to spawn LangGraph sidecar '{}': {err}",
                self.launch.display_command()
            )
        })
    }
}

#[tauri::command]
pub(crate) async fn langgraph_sidecar_ping() -> Result<LangGraphSidecarPingResult, String> {
    LangGraphSidecarHost::resolve()?.ping().await
}

#[tauri::command]
pub(crate) async fn langgraph_sidecar_invoke<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    goal: String,
    thread_id: Option<String>,
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
) -> Result<LangGraphSidecarInvokeResult, String> {
    let (workspace_cwd, workspace_id) =
        resolve_workspace_target(&state, workspace_id, workspace_cwd).await?;
    LangGraphSidecarHost::resolve()?
        .invoke(&app, goal, thread_id, workspace_cwd, workspace_id)
        .await
}

#[tauri::command]
pub(crate) async fn langgraph_sidecar_get_state<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    thread_id: String,
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
) -> Result<Value, String> {
    let (workspace_cwd, workspace_id) =
        resolve_workspace_target(&state, workspace_id, workspace_cwd).await?;
    LangGraphSidecarHost::resolve()?
        .get_state(&app, thread_id, workspace_cwd, workspace_id)
        .await
}

#[tauri::command]
pub(crate) async fn langgraph_sidecar_resume<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    thread_id: String,
    action: LangGraphSidecarResumeAction,
    feedback: Option<String>,
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
) -> Result<Value, String> {
    let (workspace_cwd, workspace_id) =
        resolve_workspace_target(&state, workspace_id, workspace_cwd).await?;
    let input = LangGraphSidecarResumeInput {
        action: action.as_str().to_string(),
        feedback: feedback
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    };
    LangGraphSidecarHost::resolve()?
        .resume(&app, thread_id, input, workspace_cwd, workspace_id)
        .await
}

#[tauri::command]
pub(crate) async fn langgraph_sidecar_continue<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    thread_id: String,
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
) -> Result<Value, String> {
    let (workspace_cwd, workspace_id) =
        resolve_workspace_target(&state, workspace_id, workspace_cwd).await?;
    LangGraphSidecarHost::resolve()?
        .continue_run(&app, thread_id, workspace_cwd, workspace_id)
        .await
}

#[tauri::command]
pub(crate) async fn langgraph_sidecar_list_runs(
    state: State<'_, AppState>,
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
) -> Result<Vec<SoloRunSummary>, String> {
    let (workspace_cwd, workspace_id) =
        resolve_workspace_target(&state, workspace_id, workspace_cwd).await?;
    read_solo_run_index(&workspace_cwd, workspace_id).await
}

#[tauri::command]
pub(crate) async fn langgraph_sidecar_read_run_events(
    state: State<'_, AppState>,
    thread_id: String,
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
) -> Result<SoloRunEventsResult, String> {
    let thread_id = normalize_safe_thread_id(thread_id)?;
    let (workspace_cwd, _workspace_id) =
        resolve_workspace_target(&state, workspace_id, workspace_cwd).await?;
    read_solo_run_events(&workspace_cwd, &thread_id).await
}

#[tauri::command]
pub(crate) async fn langgraph_sidecar_read_step_detail(
    state: State<'_, AppState>,
    thread_id: String,
    node: String,
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
) -> Result<SoloStepDetailResult, String> {
    let thread_id = normalize_safe_thread_id(thread_id)?;
    let node = normalize_safe_node_name(node)?;
    let (workspace_cwd, _workspace_id) =
        resolve_workspace_target(&state, workspace_id, workspace_cwd).await?;
    read_solo_step_detail(&workspace_cwd, &thread_id, &node).await
}

async fn resolve_workspace_target(
    state: &State<'_, AppState>,
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
) -> Result<(PathBuf, Option<String>), String> {
    let workspace_id = workspace_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if let Some(cwd) = workspace_cwd
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok((PathBuf::from(cwd), workspace_id));
    }

    if let Some(id) = workspace_id.as_ref() {
        let workspaces = state.workspaces.lock().await;
        let entry = workspaces
            .get(id)
            .ok_or_else(|| format!("workspace not found: {id}"))?;
        return Ok((PathBuf::from(&entry.path), Some(id.clone())));
    }

    let cwd = env::current_dir()
        .map_err(|err| format!("failed to resolve current workspace cwd: {err}"))?;
    Ok((cwd, workspace_id))
}

async fn read_solo_run_index(
    workspace_cwd: &Path,
    workspace_id: Option<String>,
) -> Result<Vec<SoloRunSummary>, String> {
    let index_path = workspace_cwd.join(".opencrab").join("index.json");
    let raw = match fs::read_to_string(&index_path).await {
        Ok(value) => value,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to read Solo index: {err}")),
    };
    let parsed: SoloRunIndex =
        serde_json::from_str(&raw).map_err(|err| format!("failed to parse Solo index: {err}"))?;
    let mut runs = Vec::new();
    for entry in parsed.threads.unwrap_or_default() {
        let thread_id = match normalize_safe_thread_id(entry.thread_id) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let mission = read_mission_json(workspace_cwd, &thread_id).await?;
        let goal = mission
            .as_ref()
            .and_then(|value| value.get("goal"))
            .and_then(Value::as_str)
            .or(entry.goal.as_deref())
            .unwrap_or("")
            .to_string();
        let title = make_solo_title(&goal);
        let status = mission
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .or(entry.status.as_deref())
            .unwrap_or("running")
            .to_string();
        let created_at = mission
            .as_ref()
            .and_then(|value| value.get("created_at"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(entry.created_at);
        let updated_at = mission
            .as_ref()
            .and_then(|value| value.get("updated_at"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(entry.updated_at);
        let codex_thread_id = mission
            .as_ref()
            .and_then(|value| value.get("codex"))
            .and_then(|value| value.get("codex_thread_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        runs.push(SoloRunSummary {
            thread_id,
            goal,
            title,
            status,
            created_at,
            updated_at,
            current_node: None,
            phase: None,
            workspace_id: workspace_id.clone(),
            codex_thread_id,
        });
    }
    runs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(runs)
}

async fn read_solo_run_events(
    workspace_cwd: &Path,
    thread_id: &str,
) -> Result<SoloRunEventsResult, String> {
    let thread_dir = solo_thread_dir(workspace_cwd, thread_id)?;
    let mission = read_mission_json(workspace_cwd, thread_id).await?;
    let events_path = thread_dir.join("events.jsonl");
    let events_raw = match fs::read_to_string(&events_path).await {
        Ok(value) => value,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read Solo events: {err}")),
    };
    let mut events = Vec::new();
    for line in events_raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(trimmed)
            .map_err(|err| format!("failed to parse Solo event line: {err}"))?;
        events.push(event);
    }

    let artifacts_dir = thread_dir.join("artifacts");
    let mut artifacts = Vec::new();
    let mut final_report_exists = false;
    match fs::read_dir(&artifacts_dir).await {
        Ok(mut entries) => {
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|err| format!("failed to read Solo artifacts: {err}"))?
            {
                let file_type = entry
                    .file_type()
                    .await
                    .map_err(|err| format!("failed to inspect Solo artifact: {err}"))?;
                if !file_type.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "final_report.md" || name == "final_report.json" {
                    final_report_exists = true;
                }
                artifacts.push(name);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("failed to open Solo artifacts: {err}")),
    }
    artifacts.sort();

    Ok(SoloRunEventsResult {
        thread_id: thread_id.to_string(),
        mission,
        events,
        artifacts,
        final_report_exists,
    })
}

async fn read_solo_step_detail(
    workspace_cwd: &Path,
    thread_id: &str,
    node: &str,
) -> Result<SoloStepDetailResult, String> {
    let thread_dir = solo_thread_dir(workspace_cwd, thread_id)?;
    let conversation_path = thread_dir.join("conversations").join(format!("{node}.json"));
    let conversation = match fs::read_to_string(&conversation_path).await {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|err| format!("failed to parse Solo conversation: {err}"))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(format!("failed to read Solo conversation: {err}")),
    };

    let events_result = read_solo_run_events(workspace_cwd, thread_id).await?;
    let events = events_result
        .events
        .into_iter()
        .filter(|event| event.get("node").and_then(Value::as_str) == Some(node))
        .collect::<Vec<_>>();

    let artifacts = list_matching_files(&thread_dir.join("artifacts"), node).await?;
    let logs = list_matching_files(&thread_dir.join("logs"), node).await?;

    Ok(SoloStepDetailResult {
        thread_id: thread_id.to_string(),
        node: node.to_string(),
        conversation,
        artifacts,
        logs,
        events,
    })
}

async fn list_matching_files(dir: &Path, node: &str) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    match fs::read_dir(dir).await {
        Ok(mut entries) => {
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|err| format!("failed to read Solo step files: {err}"))?
            {
                let file_type = entry
                    .file_type()
                    .await
                    .map_err(|err| format!("failed to inspect Solo step file: {err}"))?;
                if !file_type.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(node) || name.contains(&format!("{node}.")) {
                    files.push(name);
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("failed to open Solo step files: {err}")),
    }
    files.sort();
    Ok(files)
}

async fn read_mission_json(workspace_cwd: &Path, thread_id: &str) -> Result<Option<Value>, String> {
    let mission_path = solo_thread_dir(workspace_cwd, thread_id)?.join("mission.json");
    match fs::read_to_string(&mission_path).await {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|err| format!("failed to parse Solo mission: {err}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("failed to read Solo mission: {err}")),
    }
}

fn solo_thread_dir(workspace_cwd: &Path, thread_id: &str) -> Result<PathBuf, String> {
    let safe_thread_id = normalize_safe_thread_id(thread_id.to_string())?;
    let root = workspace_cwd.join(".opencrab").join("threads");
    let thread_dir = root.join(safe_thread_id);
    let normalized_root = root
        .canonicalize()
        .unwrap_or(root.clone());
    let normalized_thread = thread_dir
        .canonicalize()
        .unwrap_or(thread_dir.clone());
    if normalized_thread.starts_with(&normalized_root) {
        Ok(thread_dir)
    } else {
        Err("invalid Solo thread path".to_string())
    }
}

fn normalize_safe_thread_id(thread_id: String) -> Result<String, String> {
    let thread_id = normalize_required_thread_id(thread_id)?;
    if thread_id == "." || thread_id == ".." {
        return Err("thread_id must not be a path segment".to_string());
    }
    if thread_id.contains('/') || thread_id.contains('\\') || thread_id.contains("..") {
        return Err("thread_id must not contain path separators".to_string());
    }
    Ok(thread_id)
}

fn normalize_safe_node_name(node: String) -> Result<String, String> {
    let node = node.trim().to_string();
    if node.is_empty() {
        return Err("node must be non-empty".to_string());
    }
    if node == "." || node == ".." || node.contains('/') || node.contains('\\') || node.contains("..") {
        return Err("node must not contain path separators".to_string());
    }
    if !node
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err("node contains unsupported characters".to_string());
    }
    Ok(node)
}

fn make_solo_title(goal: &str) -> String {
    let normalized = goal.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return "Untitled Solo".to_string();
    }
    let mut chars = normalized.chars();
    let title: String = chars.by_ref().take(56).collect();
    if chars.next().is_some() {
        let prefix: String = title.chars().take(53).collect();
        format!("{prefix}...")
    } else {
        title
    }
}

fn normalize_thread_id(thread_id: Option<String>) -> String {
    thread_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("solo-{}", Uuid::new_v4()))
}

fn normalize_required_thread_id(thread_id: String) -> Result<String, String> {
    let thread_id = thread_id.trim().to_string();
    if thread_id.is_empty() {
        return Err("thread_id must be non-empty".to_string());
    }
    Ok(thread_id)
}

impl LangGraphSidecarResumeAction {
    fn as_str(&self) -> &'static str {
        match self {
            LangGraphSidecarResumeAction::Approve => "approve",
            LangGraphSidecarResumeAction::Reject => "reject",
        }
    }
}

fn resolve_launch() -> Result<SidecarLaunch, String> {
    if let Ok(raw) = env::var(SIDECAR_ENV) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return resolve_env_launch(trimmed);
        }
    }

    let sidecar_dir = dev_sidecar_dir()?;
    if !sidecar_dir.is_dir() {
        return Err(format!(
            "LangGraph sidecar directory not found: {}",
            sidecar_dir.display()
        ));
    }

    Ok(SidecarLaunch {
        source: "dev-fallback".to_string(),
        program: "npm".to_string(),
        args: vec!["run".to_string(), "dev".to_string(), "--silent".to_string()],
        cwd: Some(sidecar_dir),
    })
}

fn resolve_env_launch(raw: &str) -> Result<SidecarLaunch, String> {
    let path = PathBuf::from(raw);
    if path.is_dir() {
        return Ok(SidecarLaunch {
            source: SIDECAR_ENV.to_string(),
            program: "npm".to_string(),
            args: vec!["run".to_string(), "dev".to_string(), "--silent".to_string()],
            cwd: Some(path),
        });
    }

    if path.is_file() {
        let extension = path.extension().and_then(|value| value.to_str());
        let (program, args) = match extension {
            Some("js") | Some("mjs") | Some("cjs") => (
                "node".to_string(),
                vec![path.to_string_lossy().into_owned()],
            ),
            Some("ts") | Some("tsx") => (
                "npx".to_string(),
                vec!["tsx".to_string(), path.to_string_lossy().into_owned()],
            ),
            _ => (path.to_string_lossy().into_owned(), Vec::new()),
        };
        return Ok(SidecarLaunch {
            source: SIDECAR_ENV.to_string(),
            program,
            args,
            cwd: path.parent().map(|parent| parent.to_path_buf()),
        });
    }

    let parts = shell_words::split(raw)
        .map_err(|err| format!("failed to parse {SIDECAR_ENV} command: {err}"))?;
    let Some((program, args)) = parts.split_first() else {
        return Err(format!("{SIDECAR_ENV} is empty"));
    };

    Ok(SidecarLaunch {
        source: SIDECAR_ENV.to_string(),
        program: program.clone(),
        args: args.to_vec(),
        cwd: None,
    })
}

fn dev_sidecar_dir() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            format!(
                "failed to resolve repository root from {}",
                manifest_dir.display()
            )
        })?;
    Ok(repo_root.join("sidecar"))
}

async fn cleanup_child(child: &mut tokio::process::Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }

    if matches!(
        tokio::time::timeout(Duration::from_millis(500), child.wait()).await,
        Ok(Ok(_))
    ) {
        return;
    }

    kill_child_process_tree(child).await;
}

impl SidecarLaunch {
    fn display_command(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.clone());
        parts.join(" ")
    }
}

async fn dispatch_host_call<R: Runtime>(
    app: &AppHandle<R>,
    write_tx: &mpsc::UnboundedSender<String>,
    message: Value,
) {
    let id = message
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let op = message.get("op").and_then(Value::as_str).unwrap_or("");

    let result = match op {
        "codex_run_task" => handle_codex_run_task(app, &message).await,
        other => Err(format!("unsupported host_call op: {other}")),
    };

    let payload = match result {
        Ok(data) => json!({
            "type": "host_call_result",
            "id": id,
            "ok": true,
            "data": data,
        }),
        Err(err) => json!({
            "type": "host_call_result",
            "id": id,
            "ok": false,
            "error": { "message": err },
        }),
    };

    let mut line = match serde_json::to_string(&payload) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("failed to encode host_call_result for {id}: {err}");
            return;
        }
    };
    line.push('\n');
    if let Err(err) = write_tx.send(line) {
        eprintln!("failed to enqueue host_call_result for {id}: {err}");
    }
}

async fn handle_codex_run_task<R: Runtime>(
    app: &AppHandle<R>,
    message: &Value,
) -> Result<Value, String> {
    let input_value = message.get("input").cloned().unwrap_or(Value::Null);
    let req = CodexRunTaskRequest {
        thread_id: message
            .get("thread_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        workspace_id: message
            .get("workspace_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        node: message
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        input: serde_json::from_value(input_value.clone()).unwrap_or_default(),
        codex_thread_id: input_value
            .get("codex_thread_id")
            .and_then(Value::as_str)
            .map(|value| value.to_string()),
    };

    let state = app.state::<AppState>();
    let result = run_codex_task(&*state, req).await?;
    Ok(json!({
        "codex_thread_id": result.codex_thread_id,
        "codex_thread_created": result.codex_thread_created,
        "summary": result.summary,
        "raw_text": result.raw_text,
        "phase": result.phase,
    }))
}
