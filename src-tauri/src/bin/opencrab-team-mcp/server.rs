// rmcp stdio MCP server for the two team-coordination tools.
//
// Mirror of `opencrab-memory-mcp/server.rs` structure so the spike's
// production-readiness is obvious: same rmcp 0.15, same `ServerHandler`
// trait, same `tools/list` + `tools/call` surface. Differences:
//
//   1. NO `read_only` ToolAnnotation — `send_message` and `propose_plan`
//      are write-effect tools (the agent intends them as actions, not
//      reads), even though THIS server's body is no-op. The annotation
//      tracks intent for codex's UI / approval gates, not stub behavior.
//   2. The call body returns a no-op `{ok: true, tool, received: <args>}`.
//      The Tauri host watches `item/completed` for these calls and runs
//      the real side effect (route the message / persist the plan).
//
// Tool descriptions are the ONLY thing telling the model when to use
// these tools (Step 18's comm guide is principle-only). They must
// stand on their own in the model's tool list.

use std::borrow::Cow;
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
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use serde_json::Value;
use serde_json::json;

const SEND_MESSAGE_TOOL: &str = "send_message";
const PROPOSE_PLAN_TOOL: &str = "propose_plan";

const SEND_MESSAGE_DESCRIPTION: &str =
    "Send a message to another agent in the team. Use this whenever you need to communicate \
with a teammate or the user — it is the only way your message reaches anyone. Pass `to` \
(the recipient agent id, from the list of available recipients in your team-communication \
section), `channel` (the subscription channel for that recipient, typically `chat`), and \
`body` (the message text). The recipient sees your message wrapped as `[From <you>]` in \
their thread.";

const PROPOSE_PLAN_DESCRIPTION: &str =
    "Put a plan to the user for sign-off before any work begins. Use this whenever you want \
the user to approve a course of action that you would otherwise present as a list of \
steps. Pass a `tasks` array; each task has a short imperative `title`, an optional \
`assignee` (agent id — omit if undecided), and a longer `body` describing the task. The \
user reviews and approves, edits, or rejects each task. Approval results arrive in your \
thread as `[From system]` messages. Do not begin or delegate work on a task until its \
approval has arrived.";

#[derive(Clone)]
pub struct TeamMcpServer {
    tools: Arc<Vec<Tool>>,
}

impl TeamMcpServer {
    pub fn new() -> Self {
        Self {
            tools: Arc::new(vec![send_message_tool(), propose_plan_tool()]),
        }
    }
}

impl ServerHandler for TeamMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Team coordination tools: send_message (talk to a teammate or the user) \
                 and propose_plan (put a plan to the user for approval)."
                    .to_string(),
            ),
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
        // No-op ack — Tauri host observes `item/completed` notifications
        // to run the real side effect. We echo the args back in
        // structured_content so debug logs / a future watch tool can see
        // exactly what the agent intended.
        let args = request.arguments.unwrap_or_default();
        let result = json!({
            "ok": true,
            "tool": request.name.as_ref(),
            "received": args,
        });
        Ok(CallToolResult {
            content: vec![Content::text(result.to_string())],
            structured_content: Some(result),
            is_error: Some(false),
            meta: None,
        })
    }
}

pub async fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[opencrab-team-mcp] starting stdio server");
    let server = TeamMcpServer::new();
    let service = server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

fn send_message_tool() -> Tool {
    Tool::new(
        Cow::Borrowed(SEND_MESSAGE_TOOL),
        Cow::Borrowed(SEND_MESSAGE_DESCRIPTION),
        object_schema(json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient agent id. Must be one of the recipients listed in your team-communication section."
                },
                "channel": {
                    "type": "string",
                    "description": "Subscription channel for the recipient (typically `chat`). Must match the channel listed alongside the recipient."
                },
                "body": {
                    "type": "string",
                    "description": "The message text the recipient will see."
                }
            },
            "required": ["to", "channel", "body"],
            "additionalProperties": false
        })),
    )
}

fn propose_plan_tool() -> Tool {
    Tool::new(
        Cow::Borrowed(PROPOSE_PLAN_TOOL),
        Cow::Borrowed(PROPOSE_PLAN_DESCRIPTION),
        object_schema(json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "Ordered list of plan steps. Each task is one discrete step that the user reviews and can independently approve, edit, or reject.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "Short imperative title (e.g. 'Install django-auditlog')."
                            },
                            "assignee": {
                                "type": "string",
                                "description": "Agent id to assign this task to (optional — omit if undecided; the user picks at approval time)."
                            },
                            "body": {
                                "type": "string",
                                "description": "Longer description of what the task involves and any relevant context."
                            }
                        },
                        "required": ["title", "body"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["tasks"],
            "additionalProperties": false
        })),
    )
}

fn object_schema(value: Value) -> Arc<JsonObject> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => unreachable!("tool schema literal must be a JSON object"),
    }
}
