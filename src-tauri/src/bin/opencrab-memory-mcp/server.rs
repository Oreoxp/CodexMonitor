// Phase 6 Step 1 — the rmcp stdio MCP server for `opencrab-memory-mcp`.
//
// Exposes three tools over ONE agent's `memory.db` SQLite log store:
//   * `log_progress` (write)  — record one progress note.
//   * `memory_search` (read)  — full-text search over past detail bodies.
//   * `memory_get`   (read)  — fetch one entry by id.
//
// The agent id is baked in at spawn time (CLI arg) and the db path is
// derived from it (`paths::user_agent_memory_db`), so this process can only
// ever touch its own agent's memory file: per-agent isolation by construction.
// The rmcp surface mirrors codex-rs `memories/mcp` (the same rmcp 0.15
// version); the tools, schemas, and backend are OpenCrab's own.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use serde_json::Value;
use serde_json::json;

use crate::backend;
use crate::backend::BackendError;

const LOG_TOOL: &str = "log_progress";
const SEARCH_TOOL: &str = "memory_search";
const GET_TOOL: &str = "memory_get";

// Tool descriptions are the ONLY thing that tells the model when to reach
// for these tools (Phase 6's prompt-section work is a later step). They
// must stand on their own in the tool list.
const LOG_DESCRIPTION: &str = "Record one progress note in your own personal log. `summary` is \
a single-sentence headline (required) — what just happened or what you decided. `detail` is the \
optional longer context that supports it; this is the field that becomes full-text searchable \
later. Call this whenever something would be worth remembering across conversations or across \
days — a decision, a finding, a state change, the resolution of a thread.";

const SEARCH_DESCRIPTION: &str = "Search your own past progress log — the entries you (and \
earlier instances of you) saved with `log_progress`. Useful whenever a question touches a past \
decision, a task you worked on before, or earlier context that is not in the current \
conversation. Returns the most relevant entries (id, timestamp, summary, snippet of detail), \
ranked by relevance. Follow up with `memory_get` to read one entry's full detail.";

const GET_DESCRIPTION: &str = "Read one full entry from your own past progress log. `id` is the \
integer id returned by `memory_search`. Returns the entry's timestamp, summary, and full \
untruncated detail. Use this after `memory_search` points you at an id whose snippet was cut \
short or whose context you want in full.";

#[derive(Clone)]
pub struct MemoryMcpServer {
    agent_id: String,
    memory_db: PathBuf,
    tools: Arc<Vec<Tool>>,
}

impl MemoryMcpServer {
    pub fn new(agent_id: String, memory_db: PathBuf) -> Self {
        Self {
            agent_id,
            memory_db,
            tools: Arc::new(vec![log_tool(), search_tool(), get_tool()]),
        }
    }
}

impl ServerHandler for MemoryMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(format!(
                "Log, search, and read agent {}'s progress notes (per-agent memory.db).",
                self.agent_id
            )),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..ServerInfo::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools = Arc::clone(&self.tools);
        async move {
            Ok(ListToolsResult {
                tools: (*tools).clone(),
                next_cursor: None,
                meta: None,
            })
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let result: Value = match request.name.as_ref() {
            LOG_TOOL => {
                let summary = arguments
                    .get("summary")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        McpError::invalid_params(
                            "log_progress requires a string `summary`".to_string(),
                            None,
                        )
                    })?;
                let detail = arguments.get("detail").and_then(Value::as_str);
                let id = backend::log_progress(&self.memory_db, summary, detail)
                    .map_err(backend_error_to_mcp)?;
                json!({ "id": id })
            }
            SEARCH_TOOL => {
                let query = arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        McpError::invalid_params(
                            "memory_search requires a string `query`".to_string(),
                            None,
                        )
                    })?;
                let limit =
                    backend::clamp_limit(arguments.get("limit").and_then(Value::as_u64));
                let hits = backend::search(&self.memory_db, query, limit)
                    .map_err(backend_error_to_mcp)?;
                let hits_json = serde_json::to_value(&hits).map_err(to_internal)?;
                json!({ "query": query, "count": hits.len(), "hits": hits_json })
            }
            GET_TOOL => {
                let id = arguments
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| {
                        McpError::invalid_params(
                            "memory_get requires an integer `id`".to_string(),
                            None,
                        )
                    })?;
                let entry =
                    backend::get(&self.memory_db, id).map_err(backend_error_to_mcp)?;
                serde_json::to_value(&entry).map_err(to_internal)?
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown tool: {other}"),
                    None,
                ));
            }
        };

        Ok(CallToolResult {
            content: vec![Content::text(result.to_string())],
            structured_content: Some(result),
            is_error: Some(false),
            meta: None,
        })
    }
}

/// Serve the three memory tools over stdio until the client disconnects.
pub async fn run_stdio(
    agent_id: String,
    memory_db: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "[opencrab-memory-mcp] agent={agent_id} memory_db={}",
        memory_db.display()
    );
    let server = MemoryMcpServer::new(agent_id, memory_db);
    let service = server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

fn log_tool() -> Tool {
    // No `read_only` annotation — this is the one write tool.
    Tool::new(
        Cow::Borrowed(LOG_TOOL),
        Cow::Borrowed(LOG_DESCRIPTION),
        object_schema(json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "One-sentence headline of what to remember (required)."
                },
                "detail": {
                    "type": "string",
                    "description": "Optional longer context — the searchable body."
                }
            },
            "required": ["summary"],
            "additionalProperties": false
        })),
    )
}

fn search_tool() -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(SEARCH_TOOL),
        Cow::Borrowed(SEARCH_DESCRIPTION),
        object_schema(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search terms to look for across your past progress entries."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of entries to return (default 6)."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })),
    );
    tool.annotations = Some(ToolAnnotations::new().read_only(true));
    tool
}

fn get_tool() -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(GET_TOOL),
        Cow::Borrowed(GET_DESCRIPTION),
        object_schema(json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "The entry id to read (as returned by memory_search)."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })),
    );
    tool.annotations = Some(ToolAnnotations::new().read_only(true));
    tool
}

fn object_schema(value: Value) -> Arc<JsonObject> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => unreachable!("tool schema literal must be a JSON object"),
    }
}

fn backend_error_to_mcp(err: BackendError) -> McpError {
    match err {
        BackendError::EmptySummary => McpError::invalid_params(err.to_string(), None),
        BackendError::Io(_) | BackendError::Sqlite(_) => {
            McpError::internal_error(err.to_string(), None)
        }
    }
}

fn to_internal(err: serde_json::Error) -> McpError {
    McpError::internal_error(err.to_string(), None)
}
