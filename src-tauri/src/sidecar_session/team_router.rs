// Phase 2 pivot — Tauri-side team router.
//
// On `team_router_start` reverse-RPC from sidecar, we:
//   1. Replace any existing router for this workspace (drop old senders so
//      consumer tasks exit; remove old taps via SessionRouting helpers).
//   2. For each agent's bound Codex thread, register a permanent tap on
//      `SessionRouting::tap_thread_callbacks` and spawn a tokio consumer
//      task that loops over the tap's mpsc receiver.
//   3. The consumer accumulates `item/agentMessage/delta` text and on
//      `turn/completed` parses `<send_message to="..." channel="...">...
//      </send_message>` tags from the accumulated text. For each tag:
//         - ACL: subscriptions-driven publisher/subscribers/channels match
//           (USER_PUBLISHER `"user"` is treated as a regular topology node)
//         - target == "user": no Codex dispatch (user has no thread; the
//           tag remains visible in the sender's transcript stream)
//         - otherwise: framing `[From <sender_name>]\n<content>` and a
//           non-blocking `send_user_message_core` to the target's thread
//
// All routing data (agent roster, subscription topology) is supplied by the
// sidecar in the start RPC payload — Tauri does not re-read team.json.
//
// Hand-rolled string parser: `regex` is not in Cargo.toml and adding deps is
// out of scope. The grammar we need is trivially scannable in ~30 lines.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::shared::codex_core::{get_session_clone, send_user_message_core};
use crate::state::AppState;

const USER_PUBLISHER: &str = "user";

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct AgentInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(rename = "threadId")]
    pub(crate) thread_id: String,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct Subscription {
    pub(crate) publisher: String,
    pub(crate) subscribers: Vec<String>,
    pub(crate) channels: Vec<String>,
}

struct ParsedTag {
    to: String,
    channel: String,
    content: String,
}

#[derive(Default)]
pub(crate) struct TeamRouters {
    // workspace_id → handles to spawned consumer tasks; replaced wholesale on
    // every team_router_start. The mpsc senders inside SessionRouting are
    // dropped via `unregister_tap_callback`, which causes each consumer's
    // `rx.recv().await` to return None and the task to exit.
    inner: Mutex<HashMap<String, WorkspaceRouter>>,
}

struct WorkspaceRouter {
    // Senders we registered so we can `unregister_tap_callback` them on
    // restart. Same handles that drive the consumer tasks.
    taps: Vec<(String, mpsc::UnboundedSender<Value>)>,
    tasks: Vec<JoinHandle<()>>,
}

impl TeamRouters {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn start(
        &self,
        app_handle: AppHandle,
        workspace_id: String,
        agents: Vec<AgentInfo>,
        subscriptions: Vec<Subscription>,
    ) -> Result<(), String> {
        // Tear down any pre-existing router for this workspace first. Order
        // matters: unregister taps (so dispatch stops fanning to old senders)
        // before dropping senders (so tasks see the channel close).
        self.stop(&app_handle, &workspace_id).await;

        let state = app_handle.state::<AppState>();
        let session = get_session_clone(&state.sessions, &workspace_id).await?;
        let routing = session.routing.clone();
        drop(state);

        let by_thread: HashMap<String, AgentInfo> = agents
            .iter()
            .cloned()
            .map(|a| (a.thread_id.clone(), a))
            .collect();
        let by_agent_id: HashMap<String, String> = agents
            .iter()
            .map(|a| (a.id.clone(), a.thread_id.clone()))
            .collect();
        let shared = Arc::new(SharedRouterState {
            workspace_id: workspace_id.clone(),
            by_thread,
            by_agent_id,
            subscriptions,
            app_handle: app_handle.clone(),
        });

        let mut taps = Vec::with_capacity(agents.len());
        let mut tasks = Vec::with_capacity(agents.len());
        for agent in agents {
            let (tx, rx) = mpsc::unbounded_channel::<Value>();
            routing
                .register_tap_callback(agent.thread_id.clone(), tx.clone())
                .await;
            taps.push((agent.thread_id.clone(), tx));
            let task = spawn_consumer(rx, agent.thread_id.clone(), shared.clone());
            tasks.push(task);
        }
        self.inner
            .lock()
            .await
            .insert(workspace_id, WorkspaceRouter { taps, tasks });
        Ok(())
    }

    pub(crate) async fn stop(&self, app_handle: &AppHandle, workspace_id: &str) {
        let removed = self.inner.lock().await.remove(workspace_id);
        let Some(router) = removed else { return };
        let state = app_handle.state::<AppState>();
        if let Ok(session) = get_session_clone(&state.sessions, workspace_id).await {
            for (thread_id, sender) in &router.taps {
                session
                    .routing
                    .unregister_tap_callback(thread_id, sender)
                    .await;
            }
        }
        drop(router);
    }
}

struct SharedRouterState {
    workspace_id: String,
    by_thread: HashMap<String, AgentInfo>,
    by_agent_id: HashMap<String, String>,
    subscriptions: Vec<Subscription>,
    app_handle: AppHandle,
}

fn spawn_consumer(
    mut rx: mpsc::UnboundedReceiver<Value>,
    thread_id: String,
    shared: Arc<SharedRouterState>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = String::new();
        loop {
            let Some(event) = rx.recv().await else {
                // Channel closed — sidecar restarted the router (or workspace
                // tore down). Just exit.
                return;
            };
            let method = event.get("method").and_then(|m| m.as_str()).unwrap_or("");
            match method {
                "item/agentMessage/delta" => {
                    if let Some(delta) = event
                        .get("params")
                        .and_then(|p| p.get("delta"))
                        .and_then(|d| d.as_str())
                    {
                        buf.push_str(delta);
                    }
                }
                "turn/completed" => {
                    let final_text = std::mem::take(&mut buf);
                    process_final_text(&shared, &thread_id, &final_text).await;
                }
                "turn/error" => {
                    // Discard accumulated text; nothing to route on a failed turn.
                    buf.clear();
                }
                _ => {}
            }
        }
    })
}

async fn process_final_text(shared: &SharedRouterState, sender_thread: &str, text: &str) {
    let Some(sender) = shared.by_thread.get(sender_thread) else {
        // Tap fired for an unknown thread — nothing we can do with it.
        return;
    };
    let tags = parse_send_message_tags(text);
    for tag in tags {
        if !can_send(&shared.subscriptions, &sender.id, &tag.to, &tag.channel) {
            eprintln!(
                "[team-router] send_message ACL denied: {} → {} on channel \"{}\"",
                sender.id, tag.to, tag.channel
            );
            continue;
        }
        if tag.to == USER_PUBLISHER {
            // The user sees the tag as part of the sender's transcript stream;
            // no Codex thread to dispatch to. Routing succeeds silently.
            continue;
        }
        let Some(target_thread) = shared.by_agent_id.get(&tag.to).cloned() else {
            eprintln!(
                "[team-router] send_message target {} has no bound thread (provisioning gap?)",
                tag.to
            );
            continue;
        };
        let framed = format!("[From {}]\n{}", sender.name, tag.content);
        let state = shared.app_handle.state::<AppState>();
        let dispatch = send_user_message_core(
            &state.sessions,
            &state.workspaces,
            shared.workspace_id.clone(),
            target_thread.clone(),
            framed,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        drop(state);
        if let Err(err) = dispatch {
            eprintln!(
                "[team-router] dispatch to {} ({}) failed: {}",
                tag.to, target_thread, err
            );
        }
    }
}

fn can_send(subs: &[Subscription], from: &str, to: &str, channel: &str) -> bool {
    subs.iter().any(|s| {
        s.publisher == from
            && s.subscribers.iter().any(|sub| sub == to)
            && s.channels.iter().any(|c| c == channel)
    })
}

// Hand-rolled scanner: locates each `<send_message ATTRS>BODY</send_message>`
// block, extracts `to` and `channel` attrs (double-quoted, any order). Nested
// `</send_message>` literals inside BODY are not supported (the comm guide
// tells agents not to do that); the first close wins and truncates BODY.
fn parse_send_message_tags(text: &str) -> Vec<ParsedTag> {
    const OPEN_TAG: &str = "<send_message";
    const CLOSE_TAG: &str = "</send_message>";

    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open_at) = rest.find(OPEN_TAG) {
        let after_name = &rest[open_at + OPEN_TAG.len()..];
        // Require a whitespace after `<send_message` so we don't match e.g.
        // `<send_messages>`. (No-op if attrs follow with `>` immediately —
        // that's a malformed tag and we skip it.)
        let after_first = after_name.chars().next();
        if !matches!(after_first, Some(c) if c.is_ascii_whitespace() || c == '>') {
            // Not actually our tag — advance past the false positive and
            // keep scanning.
            rest = after_name;
            continue;
        }
        let Some(open_close_rel) = after_name.find('>') else {
            break;
        };
        let attrs_str = &after_name[..open_close_rel];
        let body_start = &after_name[open_close_rel + 1..];
        let Some(close_rel) = body_start.find(CLOSE_TAG) else {
            // Unterminated tag — advance past the open token to avoid an
            // infinite loop and stop trying to extract this one.
            rest = body_start;
            continue;
        };
        let body = &body_start[..close_rel];
        let to = extract_attr(attrs_str, "to");
        let channel = extract_attr(attrs_str, "channel");
        if let (Some(to), Some(channel)) = (to, channel) {
            out.push(ParsedTag {
                to,
                channel,
                content: body.trim().to_string(),
            });
        } else {
            eprintln!(
                "[team-router] malformed send_message tag (missing to/channel); ignoring"
            );
        }
        rest = &body_start[close_rel + CLOSE_TAG.len()..];
    }
    out
}

fn extract_attr(attrs: &str, name: &str) -> Option<String> {
    // Look for `<name>="..."` allowing any whitespace around `=`. Rejects
    // single-quoted values (the comm guide spec uses double quotes).
    let mut search_from = 0;
    while let Some(rel) = attrs[search_from..].find(name) {
        let abs = search_from + rel;
        // Boundary check: char before `name` must be start-of-string or
        // whitespace (avoid matching `to` inside e.g. `auto`).
        let is_boundary = abs == 0
            || attrs[..abs]
                .chars()
                .last()
                .map(|c| c.is_ascii_whitespace())
                .unwrap_or(false);
        if !is_boundary {
            search_from = abs + name.len();
            continue;
        }
        let after_name = &attrs[abs + name.len()..];
        let trimmed = after_name.trim_start();
        let Some(eq_after) = trimmed.strip_prefix('=') else {
            search_from = abs + name.len();
            continue;
        };
        let after_eq = eq_after.trim_start();
        let Some(after_quote) = after_eq.strip_prefix('"') else {
            search_from = abs + name.len();
            continue;
        };
        let Some(close_rel) = after_quote.find('"') else {
            return None;
        };
        return Some(after_quote[..close_rel].to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subs(rules: &[(&str, &[&str], &[&str])]) -> Vec<Subscription> {
        rules
            .iter()
            .map(|(pub_, subs, chans)| Subscription {
                publisher: (*pub_).to_string(),
                subscribers: subs.iter().map(|s| (*s).to_string()).collect(),
                channels: chans.iter().map(|c| (*c).to_string()).collect(),
            })
            .collect()
    }

    #[test]
    fn parses_single_tag() {
        let text = r#"hi <send_message to="bob" channel="chat">hello bob</send_message> bye"#;
        let tags = parse_send_message_tags(text);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].to, "bob");
        assert_eq!(tags[0].channel, "chat");
        assert_eq!(tags[0].content, "hello bob");
    }

    #[test]
    fn parses_attrs_in_either_order_and_multiline_body() {
        let text =
            "<send_message channel=\"chat\" to=\"alice\">line one\nline two</send_message>";
        let tags = parse_send_message_tags(text);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].to, "alice");
        assert_eq!(tags[0].content, "line one\nline two");
    }

    #[test]
    fn parses_multiple_tags_in_order() {
        let text = r#"<send_message to="a" channel="chat">one</send_message>
between
<send_message to="b" channel="chat">two</send_message>"#;
        let tags = parse_send_message_tags(text);
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].to, "a");
        assert_eq!(tags[1].to, "b");
    }

    #[test]
    fn ignores_malformed_tag_missing_to() {
        let text = r#"<send_message channel="chat">no recipient</send_message>"#;
        let tags = parse_send_message_tags(text);
        assert!(tags.is_empty());
    }

    #[test]
    fn ignores_unterminated_tag() {
        let text = r#"<send_message to="a" channel="chat">never closes"#;
        let tags = parse_send_message_tags(text);
        assert!(tags.is_empty());
    }

    #[test]
    fn can_send_matches_publisher_subscriber_channel() {
        let s = subs(&[("pm", &["user", "dev"], &["chat"])]);
        assert!(can_send(&s, "pm", "user", "chat"));
        assert!(can_send(&s, "pm", "dev", "chat"));
        assert!(!can_send(&s, "pm", "qa", "chat"));
        assert!(!can_send(&s, "pm", "user", "secret"));
        assert!(!can_send(&s, "dev", "pm", "chat"));
    }
}
