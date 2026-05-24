// Phase 5 Step 2 — the rmcp stdio MCP server for `opencrab-memory-mcp`.
//
// Exposes two read-only tools — `memory_search` and `memory_get` — over the
// daily-memory archive of ONE agent. The agent id + the agent's
// `project-memory/` directory are baked in at spawn time (CLI args), so this
// process can only ever see its own agent's memory: per-agent isolation by
// construction. The rmcp surface mirrors codex-rs `memories/mcp` (the same
// rmcp 0.15 version); the tools, schemas, and backend are OpenCrab's own.

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

const SEARCH_TOOL: &str = "memory_search";
const GET_TOOL: &str = "memory_get";

// Tool descriptions are the ONLY thing that tells the model when to reach for
// these tools (Phase 5 Step 2 deliberately ships no prompt section — that is
// Phase 6's job). They must stand on their own in the tool list.
const SEARCH_DESCRIPTION: &str = "Search your own past daily-memory journal — your dated \
`project-memory/<date>.md` working notes from earlier days, including days far enough back that \
they are no longer auto-loaded into your context. Use this whenever a question touches a past \
decision, a task you worked on before, or earlier project context that is not in the current \
conversation. Returns the most relevant note paragraphs, each tagged with its date and ranked by \
relevance. Follow up with `memory_get` to read a full day.";

const GET_DESCRIPTION: &str = "Read one full day of your past daily-memory journal. `date` is a \
`YYYY-MM-DD` stamp (for example a date returned by `memory_search`). Returns the entire \
`project-memory/<date>.md` note file for that day. Use it after `memory_search` points you at a \
date, or when you recall a specific day you want to re-read in full.";

#[derive(Clone)]
pub struct MemoryMcpServer {
    agent_id: String,
    memory_dir: PathBuf,
    tools: Arc<Vec<Tool>>,
}

impl MemoryMcpServer {
    pub fn new(agent_id: String, memory_dir: PathBuf) -> Self {
        Self {
            agent_id,
            memory_dir,
            tools: Arc::new(vec![search_tool(), get_tool()]),
        }
    }
}

impl ServerHandler for MemoryMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(format!(
                "Search and read agent {}'s daily-memory journal (project-memory notes).",
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
                let hits = backend::search(&self.memory_dir, query, limit)
                    .map_err(backend_error_to_mcp)?;
                let hits_json = serde_json::to_value(&hits).map_err(to_internal)?;
                json!({ "query": query, "count": hits.len(), "hits": hits_json })
            }
            GET_TOOL => {
                let date = arguments
                    .get("date")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        McpError::invalid_params(
                            "memory_get requires a string `date` (YYYY-MM-DD)".to_string(),
                            None,
                        )
                    })?;
                let doc = backend::get(&self.memory_dir, date)
                    .map_err(backend_error_to_mcp)?;
                serde_json::to_value(&doc).map_err(to_internal)?
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

/// Serve the two memory tools over stdio until the client disconnects.
pub async fn run_stdio(
    agent_id: String,
    memory_dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "[opencrab-memory-mcp] agent={agent_id} memory_dir={}",
        memory_dir.display()
    );
    let server = MemoryMcpServer::new(agent_id, memory_dir);
    let service = server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
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
                    "description": "Search terms to look for across your daily-memory notes."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of note paragraphs to return (default 6)."
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
                "date": {
                    "type": "string",
                    "description": "The day to read, as a YYYY-MM-DD stamp."
                }
            },
            "required": ["date"],
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
        BackendError::InvalidDate(_) => McpError::invalid_params(err.to_string(), None),
        BackendError::Io(_) | BackendError::Sqlite(_) => {
            McpError::internal_error(err.to_string(), None)
        }
    }
}

fn to_internal(err: serde_json::Error) -> McpError {
    McpError::internal_error(err.to_string(), None)
}
