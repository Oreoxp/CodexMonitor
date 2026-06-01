use serde_json::Value;

// ---------------------------------------------------------------------------
// Phase 6 Step 3a — transcript rendering + compaction-segment splitting
// ---------------------------------------------------------------------------
//
// Pure functions that take raw_event JSONL lines (in `(line_no, payload)` form,
// already ordered by line_no) and produce a transcript string ready for an
// LLM distiller, segmented at top-level `"type":"compacted"` boundaries.
//
// CRITICAL: we navigate the JSON via `serde_json::Value` + string keys only.
// **No `codex_protocol` / `ResponseItem` / `RolloutItem` type import here.**
// The raw_event table is codex-cli-agnostic (§五 raw 表抽象边界); locking
// these structs in would couple OpenCrab's distiller to a moving upstream.

/// Classification of one rollout JSONL line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedLine {
    /// Goes into the transcript.
    Kept(RenderedItem),
    /// Top-level `type == "compacted"` — a segment boundary. The `summary`
    /// is the marker line's `payload.message` (empty string if missing).
    Boundary { summary: String },
    /// Filtered out by the memory policy, an unknown shape, or invalid JSON.
    Dropped,
}

/// One transcript-worthy item, in a structured form before rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedItem {
    Message {
        /// Lowercased role string ("user", "assistant", ...). "developer"
        /// is never produced here — that variant lands in `Dropped`.
        role: String,
        text: String,
    },
    ToolCall {
        name: String,
        arguments: String,
        call_id: String,
    },
    ToolResult {
        call_id: Option<String>,
        output: String,
    },
    /// Any of the policy-kept-but-shape-varied tool variants: local_shell_call,
    /// tool_search_call/output, custom_tool_call/output, web_search_call. If
    /// best-effort text extraction (`text`/`input`/`query`/`execution`/`content`)
    /// yielded a non-empty string, it's `Some`; otherwise `None` (rendered as
    /// a `[tool: <kind>]` placeholder).
    ToolMisc {
        kind: String,
        text: Option<String>,
    },
}

/// One contiguous range of `(line_no, payload)` pairs bounded by either a
/// top-level `"compacted"` marker or the start/end of the input. `transcript`
/// already has its lines joined with `\n` and is ready to feed an LLM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start_line_no: i64,
    pub end_line_no: i64,
    /// `Some(summary)` iff this segment is preceded by a compaction marker
    /// (the marker's `payload.message`). `None` for the first segment of a
    /// thread that hasn't been compacted at the start.
    pub prior_summary: Option<String>,
    pub transcript: String,
}

/// Classify one raw_event payload string. Never panics — any error path
/// (invalid JSON, missing field, unknown shape) returns `Dropped`.
pub fn parse_line(payload: &str) -> ParsedLine {
    let v: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return ParsedLine::Dropped,
    };
    let top_type = match v.get("type").and_then(|x| x.as_str()) {
        Some(t) => t,
        None => return ParsedLine::Dropped,
    };
    match top_type {
        // Top-level RolloutItem::Compacted — codex's segment marker.
        // Distinct from the *inner* `response_item/compaction` (which is
        // an encrypted-internal marker and gets Dropped below).
        "compacted" => {
            let summary = v
                .get("payload")
                .and_then(|p| p.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            ParsedLine::Boundary { summary }
        }
        "response_item" => parse_response_item(v.get("payload")),
        // session_meta / turn_context / event_msg / unknown → not transcript.
        _ => ParsedLine::Dropped,
    }
}

fn parse_response_item(payload: Option<&Value>) -> ParsedLine {
    let p = match payload {
        Some(p) => p,
        None => return ParsedLine::Dropped,
    };
    let inner = match p.get("type").and_then(|x| x.as_str()) {
        Some(t) => t,
        None => return ParsedLine::Dropped,
    };
    match inner {
        "message" => parse_message(p),
        "function_call" => parse_function_call(p),
        "function_call_output" => parse_function_call_output(p),
        // Policy-kept tool variants whose shapes vary — best-effort text
        // extraction; placeholder if none yields anything useful.
        "local_shell_call"
        | "tool_search_call"
        | "tool_search_output"
        | "custom_tool_call"
        | "custom_tool_call_output"
        | "web_search_call" => parse_tool_misc(p, inner),
        // Dropped per `should_persist_response_item_for_memories`
        // (codex-cli rollout/src/policy.rs:46).
        "reasoning" | "compaction" | "context_compaction" | "image_generation_call" => {
            ParsedLine::Dropped
        }
        _ => ParsedLine::Dropped,
    }
}

fn parse_message(p: &Value) -> ParsedLine {
    let role = match p.get("role").and_then(|x| x.as_str()) {
        Some(r) => r,
        None => return ParsedLine::Dropped,
    };
    if role == "developer" {
        return ParsedLine::Dropped;
    }
    let mut buf = String::new();
    if let Some(arr) = p.get("content").and_then(|v| v.as_array()) {
        for item in arr {
            let kind = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if matches!(kind, "input_text" | "output_text") {
                if let Some(text) = item.get("text").and_then(|x| x.as_str()) {
                    buf.push_str(text);
                }
            }
        }
    }
    if buf.trim().is_empty() {
        return ParsedLine::Dropped;
    }
    ParsedLine::Kept(RenderedItem::Message {
        role: role.to_lowercase(),
        text: buf,
    })
}

fn parse_function_call(p: &Value) -> ParsedLine {
    let name = p
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let arguments = p
        .get("arguments")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let call_id = p
        .get("call_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() && arguments.is_empty() {
        return ParsedLine::Dropped;
    }
    ParsedLine::Kept(RenderedItem::ToolCall {
        name,
        arguments,
        call_id,
    })
}

fn parse_function_call_output(p: &Value) -> ParsedLine {
    let call_id = p
        .get("call_id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    // `output` may be either a bare string (legacy) or an object with
    // `content` (string) or `content_items` (array of `{type, text}`).
    let text = match p.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(out) => {
            if let Some(content) = out.get("content").and_then(|v| v.as_str()) {
                content.to_string()
            } else if let Some(arr) = out.get("content_items").and_then(|v| v.as_array()) {
                arr.iter()
                    .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                    .collect::<Vec<&str>>()
                    .join("")
            } else {
                String::new()
            }
        }
        None => String::new(),
    };
    ParsedLine::Kept(RenderedItem::ToolResult {
        call_id,
        output: text,
    })
}

fn parse_tool_misc(p: &Value, kind: &str) -> ParsedLine {
    // Best-effort: try the few keys that variants in this group actually
    // use to carry textual content. Anything missing → Some(None) →
    // placeholder render.
    let text = ["text", "input", "query", "execution", "content"]
        .iter()
        .find_map(|key| p.get(key).and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    ParsedLine::Kept(RenderedItem::ToolMisc {
        kind: kind.to_string(),
        text,
    })
}

/// Render one Kept item to a transcript line. Returns `None` when the
/// rendered text would be entirely whitespace (per the "空文本的 Kept 跳过"
/// rule).
fn render_item(item: &RenderedItem) -> Option<String> {
    match item {
        RenderedItem::Message { role, text } => {
            if text.trim().is_empty() {
                None
            } else {
                Some(format!("{}: {}", role.to_uppercase(), text))
            }
        }
        RenderedItem::ToolCall {
            name, arguments, ..
        } => {
            if name.trim().is_empty() && arguments.trim().is_empty() {
                None
            } else {
                let n = if name.is_empty() { "<unknown>" } else { name };
                Some(format!("TOOL CALL {}: {}", n, arguments))
            }
        }
        RenderedItem::ToolResult { output, .. } => {
            if output.trim().is_empty() {
                None
            } else {
                Some(format!("TOOL RESULT: {}", output))
            }
        }
        RenderedItem::ToolMisc { kind, text } => match text {
            Some(t) if !t.trim().is_empty() => {
                Some(format!("TOOL {}: {}", kind.to_uppercase(), t))
            }
            _ => Some(format!("[tool: {}]", kind)),
        },
    }
}

/// Segment a thread's raw_event lines on top-level `"type":"compacted"`
/// boundaries. Each segment has a closed `[start_line_no, end_line_no]`
/// range (line numbers are 1-based per the S2 ingester's contract) and a
/// transcript built by rendering every Kept line, joined by `\n`. Dropped
/// lines occupy line_no slots in the range but contribute no transcript.
pub fn segment_thread(lines: &[(i64, &str)]) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let mut start_line: Option<i64> = None;
    let mut pending_prior: Option<String> = None;

    for &(line_no, payload) in lines {
        if start_line.is_none() {
            start_line = Some(line_no);
        }
        match parse_line(payload) {
            ParsedLine::Kept(item) => {
                if let Some(s) = render_item(&item) {
                    buf.push(s);
                }
            }
            ParsedLine::Boundary { summary } => {
                // Close the current segment. `end_line_no` is the boundary
                // line itself — per spec: "Boundary 计入前段 end".
                segments.push(Segment {
                    start_line_no: start_line.expect("start_line set above"),
                    end_line_no: line_no,
                    prior_summary: pending_prior.take(),
                    transcript: buf.join("\n"),
                });
                buf.clear();
                start_line = None;
                pending_prior = Some(summary);
            }
            ParsedLine::Dropped => {
                // Stays inside the current segment's line range; just no
                // transcript contribution. Don't touch buf or start_line.
            }
        }
    }

    // Trailing segment: only if there's at least one line after the last
    // boundary. The "末行即 compacted → 无末尾空段" rule falls out of this
    // naturally because the boundary's iteration resets `start_line = None`.
    if let Some(start) = start_line {
        let end = lines.last().map(|(n, _)| *n).unwrap_or(start);
        segments.push(Segment {
            start_line_no: start,
            end_line_no: end,
            prior_summary: pending_prior,
            transcript: buf.join("\n"),
        });
    }

    segments
}
