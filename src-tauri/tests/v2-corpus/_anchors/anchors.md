# V2 Corpus Handoff — Anchors for the Corpus Author

This file packages everything Section 8 of the V2 corpus spec asked for. It is
self-contained — the corpus author can synthesize against this one file and the
spec, without reading `backend.rs` directly.

> **Rev 2 (author-reviewed).** Incorporates the corpus author's six decisions
> (§6) + three confirmed source findings: `parent_thread_id` is always NULL
> (§0.7), the exact scan-path template + attribution rules (§0.2), and
> ToolMisc is 0/108 synthesize-only (§5.1). De-sensitized hard (§5.3) — a
> full key-shaped-string sweep of all 108 real rollouts came back empty.

All source excerpts are verbatim copies from
`CodexMonitor/src-tauri/src/bin/opencrab-memory-mcp/backend.rs` at the line
ranges shown. If a discrepancy appears between this file and `backend.rs`,
`backend.rs` is canonical — flag it and we'll regenerate.

---

## 0. Critical findings before you start

### 0.1 Real rollouts on this machine contain ZERO `type:"compacted"` lines

Scanned all 108 rollout files under `~/.opencrab/` (archived_sessions + live
agent team_sessions). None has a `type:"compacted"` top-level line. The
`multi-segment-compaction` and `with-prior-summary` cases in the corpus
**must be hand-synthesized** — there is no real-world anchor to imitate.

The shape is unambiguous from `parse_line` (see §1, function body line 910-918):

```json
{"timestamp":"2026-05-15T10:59:58.096Z","type":"compacted","payload":{"message":"<prior summary text>"}}
```

Only `type` and `payload.message` are load-bearing for parsing. `timestamp`
is for human readability only — the parser ignores it. Anything else inside
`payload` is also ignored. Empty string for `payload.message` is legal and
results in `Boundary { summary: "" }` (then `prior_summary = Some("")` on the
next segment — not `None`).

### 0.2 Directory layout the harness builds + how attribution is parsed

The ingester (`ingest_once(conn, scan_root, agent_id)`) is handed a
`scan_root` and an `agent_id` string. It walks `scan_root/*/*/rollout-*.jsonl`
(exactly two directory levels below `scan_root`) and derives attribution per
file. **The corpus harness must lay rollouts out to match this template:**

```
<scan_root>/<team-id>/<project-hash>/rollout-<ISO-timestamp>-<thread-uuid>.jsonl
```

In production `scan_root` is the per-agent `team_sessions` dir, so the full
on-disk shape is:

```
~/.opencrab/agents/<agent-id>/team_sessions/<team-id>/<project-hash>/rollout-<ISO>-<thread-uuid>.jsonl
   └─ NOT parsed ──────────┘ └────────────── scan_root ───────────┘ └── two segments parsed ──┘ └─ filename ─┘
```

Attribution rules, verbatim from `parse_rollout_attribution`
(backend.rs:475-510) — every one is a hard gate, miss any and the file is
**silently skipped** (logged + ignored, ingest continues):

| Field | Source | Rule |
|---|---|---|
| `agent_id` | **the `--agent-id` arg**, NOT the path | The `agents/<agent-id>/` prefix above `team_sessions` is *not* parsed. The harness injects it. |
| `thread_id` | **last 36 chars of the file stem** | Must be canonical UUID 8-4-4-4-12 (`is_uuid_form`). The `rollout-<ISO>-` prefix is ignored — only the trailing 36 chars matter. |
| `team_id` | path segment immediately under `team_sessions`/`scan_root` | `<scan_root>/<team-id>/...` |
| `project_hash` | next path segment | `<scan_root>/<team-id>/<project-hash>/...` |

Hard gates that cause a **skip** (use these to author negative S2 cases too):
- filename does not end in `.jsonl`
- stem shorter than 36 chars, or last 36 chars aren't UUID-shaped
- the cut at `len-36` lands mid-codepoint (multibyte char straddling the
  boundary) — `is_char_boundary` guard, returns skip not panic
- the file is NOT exactly at `<...>/<team>/<hash>/<file>` depth: one level
  too shallow or too deep → skip (`idx+3 == file_name && idx+4 is None`)

Consequences for corpus authoring:

- A case dir `cases/C001-pg-decision/` holding
  `rollout-2026-05-15T10-59-57-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl`
  → `thread_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"`. The harness copies
  this file to `<tmp_scan_root>/<team-id>/<project-hash>/` before ingest.
- Two cases must use two **distinct** trailing UUIDs even if content is
  similar — same UUID = same thread = the `last_offset` cursor treats the
  second as a continuation of the first.
- A multi-thread case (e.g. a resume case, §5.4) drops 2+ files with
  different UUIDs under the same `<team>/<hash>/`.
- `<thread-uuid>` is a **synthetic UUID you mint**, not a real one from this
  machine. Use any valid UUIDv4-shaped string; they need not be globally
  unique, just distinct within the corpus.
- Real files use a `rollout-<ISO>-<uuid>` filename; a bare `rollout-<uuid>.jsonl`
  also parses (last-36-chars rule), but **use the ISO prefix for realism.**

### 0.3 Transcript truncation hard cap: 48,000 chars

```rust
// backend.rs:2044-2047
/// Per-segment transcript char budget fed to the LLM. Older content
/// (head of segment) is dropped first; the truncation point is marked
/// with `[...earlier content truncated...]`.
pub const DISTILL_MAX_TRANSCRIPT_CHARS: usize = 48_000;
```

**Truncation behavior (you don't need the source — this is the full
contract):** when a segment's rendered transcript exceeds 48,000 chars, the
distiller drops from the **HEAD** (oldest content) and keeps the **TAIL**
(most recent), inserting a `[...earlier content truncated...]` marker at the
cut. So the LLM sees: `[...earlier content truncated...]` + the last ~48k
chars.

So `long-truncation` / `long-thread` cases need a segment whose rendered
transcript exceeds 48k chars. Note this is **rendered transcript chars**, not
raw JSONL bytes. A 200 KB file of mostly Dropped lines may render to a 5 KB
transcript and not trip truncation; a 60 KB file of pure `output_text` will.
Compute against the Kept items × `render_item` format (§1.5) to predict.

**Ground-truth pattern for `long-truncation`:** plant a distinctive durable
fact in the **last** few Kept lines (the tail, which survives) and a decoy
"fact" in the **first** lines (the head, which gets dropped). Then assert:
- `expected_distill.knowledge_points` → `summary_must_contain_any` matches
  the TAIL fact;
- a `summary_must_not_contain` (or `detail_must_not_mention`) entry for the
  HEAD decoy — proving the head was truncated away and the LLM never saw it.

That's the whole point of the case: KP reflects tail content, not head.

### 0.4 `role: "developer"` is always Dropped

`parse_message` returns `ParsedLine::Dropped` for any `role == "developer"`,
regardless of content (backend.rs:960-962). Don't synthesize developer
messages and expect them in the transcript.

### 0.5 Inner `reasoning` is Dropped, top-level `compacted` is Boundary

Don't confuse the two: `response_item.payload.type == "reasoning"` is silent
chain-of-thought, dropped before the LLM ever sees it. `top.type == "compacted"`
is the segment marker. Also dropped inner types:
`compaction`, `context_compaction`, `image_generation_call`.

### 0.6 Outer dropped types

Anything where `top.type` is not `"response_item"` and not `"compacted"` is
Dropped: `session_meta`, `turn_context`, `event_msg` (all variants), and any
unknown future type. The corpus can use these as noise filler without worrying
about them affecting Kept/Boundary counts.

### 0.7 `parent_thread_id` is ALWAYS NULL after ingest — resume chains are NOT captured

**Confirmed against source** (this directly answers the corpus author's S5
question). The ingester deliberately never writes `parent_thread_id`:

```rust
// backend.rs:662-665, inside ingest_one_file_in_tx
// Upsert raw_thread. We deliberately do NOT touch source / parent_thread_id
// / cwd here — those fields are payload-derived (S3). On re-encounter of
// an existing thread the IGNORE keeps every column unchanged; only the
// cursor + last_ingest_ts move (via the UPDATE at the end).
```

The `INSERT OR IGNORE INTO raw_thread (...)` lists only `thread_id, agent_id,
team_id, project_hash, source_path, first_seen_ts, last_ingest_ts,
last_offset, last_line_no` — `parent_thread_id` is **not** in the column list,
so it defaults to NULL. backend.rs:4281 asserts `parent_thread_id.is_none()`.
The comment says it's "payload-derived (S3)", i.e. *intended* to be filled
later — but **no current code fills it** (the S3 distiller block we read
never touches `raw_thread.parent_thread_id`). As of Phase 6 today: **always
NULL.**

**Impact on the corpus — this changes S5 supersede design:**

- A resume case is physically **two rollout files → two thread_ids → two
  `raw_thread` rows, both with `parent_thread_id = NULL`.** The DB does not
  encode "thread B continues thread A." Do **not** write a ground_truth
  assertion that depends on `parent_thread_id` linkage — it will always fail.
- The two threads distill **independently**; their KPs land in `log`
  side-by-side with no structural link.
- Therefore the ONLY signals available to S5 consolidation to relate a
  thread-A KP and a thread-B KP are: (a) **KP content** (the LLM-judge
  compares summaries/details) and (b) **`log.ts`** (the timestamp the harness
  injects per the spec's `created_at_offset`). This *reinforces* the spec's
  existing §4.4 design — supersede ordering comes from harness-injected
  `created_at_offset`, never from a parent chain.
- See §5.4 for how to physically represent resume vs in-thread compaction.

---

## 1. `parse_line()` + types (backend.rs:830-1089)

This is the rollout-line classifier. **Read this first.** Every line you
synthesize needs to land in exactly one of `Kept | Boundary | Dropped`, and
the ground_truth.yaml must declare which.

### 1.1 `ParsedLine` enum and `RenderedItem` (backend.rs:830-893)

```rust
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
```

### 1.2 `parse_line()` entry point (backend.rs:895-923)

```rust
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
```

### 1.3 `parse_response_item` + sub-dispatchers (backend.rs:925-1007)

```rust
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
    let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let arguments = p.get("arguments").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let call_id = p.get("call_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if name.is_empty() && arguments.is_empty() {
        return ParsedLine::Dropped;
    }
    ParsedLine::Kept(RenderedItem::ToolCall { name, arguments, call_id })
}
```

### 1.4 `parse_function_call_output` + `parse_tool_misc` (backend.rs:1009-1051)

```rust
fn parse_function_call_output(p: &Value) -> ParsedLine {
    let call_id = p.get("call_id").and_then(|x| x.as_str()).map(|s| s.to_string());
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
    ParsedLine::Kept(RenderedItem::ToolResult { call_id, output: text })
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
    ParsedLine::Kept(RenderedItem::ToolMisc { kind: kind.to_string(), text })
}
```

### 1.5 `render_item` — Kept → transcript-line format (backend.rs:1053-1089)

This determines **what the LLM actually sees**. If you want a Kept line to
read a specific way in the transcript, generate it so it lands here as the
right shape.

```rust
/// Render one Kept item to a transcript line. Returns `None` when the
/// rendered text would be entirely whitespace (per the "空文本的 Kept 跳过"
/// rule).
fn render_item(item: &RenderedItem) -> Option<String> {
    match item {
        RenderedItem::Message { role, text } => {
            if text.trim().is_empty() { None }
            else { Some(format!("{}: {}", role.to_uppercase(), text)) }
        }
        RenderedItem::ToolCall { name, arguments, .. } => {
            if name.trim().is_empty() && arguments.trim().is_empty() { None }
            else {
                let n = if name.is_empty() { "<unknown>" } else { name };
                Some(format!("TOOL CALL {}: {}", n, arguments))
            }
        }
        RenderedItem::ToolResult { output, .. } => {
            if output.trim().is_empty() { None }
            else { Some(format!("TOOL RESULT: {}", output)) }
        }
        RenderedItem::ToolMisc { kind, text } => match text {
            Some(t) if !t.trim().is_empty() => Some(format!("TOOL {}: {}", kind.to_uppercase(), t)),
            _ => Some(format!("[tool: {}]", kind)),
        },
    }
}
```

**Render format reference (this is what the LLM sees):**

| Kept variant | Rendered line |
|---|---|
| `Message{role,text}` | `<UPPERCASED-ROLE>: <text>` — e.g. `USER: 我们用什么数据库?` |
| `ToolCall{name,args}` | `TOOL CALL <name>: <args>` — e.g. `TOOL CALL bash: {"cmd":"ls"}` |
| `ToolResult{output}` | `TOOL RESULT: <output>` (no `call_id` in render) |
| `ToolMisc{kind, Some(t)}` | `TOOL <KIND>: <t>` — e.g. `TOOL WEB_SEARCH_CALL: rust async` |
| `ToolMisc{kind, None}` | `[tool: <kind>]` (placeholder) |

Lines join with `\n`. **There is no blank line between lines, no role prefix
on continuation lines.** This is what `EXTRACTION_PROMPT` (§3) refers to as
`[TRANSCRIPT]`.

---

## 2. `segment_thread()` — split by Boundary (backend.rs:1091-1146)

```rust
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
```

### 2.1 Worked example — single segment (no boundary)

Input lines (line_no, classification):

```
(1, Dropped)      // session_meta
(2, Dropped)      // event_msg/task_started
(3, Kept Message) // user: "用什么数据库?"
(4, Dropped)      // event_msg/user_message (mirror)
(5, Kept Message) // assistant: "用 Postgres,因为..."
(6, Dropped)      // event_msg/task_complete
```

Output: **1 segment**:
```
Segment {
  start_line_no: 1,
  end_line_no: 6,
  prior_summary: None,
  transcript: "USER: 用什么数据库?\nASSISTANT: 用 Postgres,因为...",
}
```

### 2.2 Worked example — two segments, boundary at L4

Input:
```
(1, Dropped)
(2, Kept Message)              // "earlier user msg"
(3, Kept Message)              // "earlier assistant reply"
(4, Boundary{summary:"earlier we discussed X"})
(5, Kept Message)              // "later user msg"
(6, Kept Message)              // "later assistant reply"
```

Output: **2 segments**:
```
Segment {
  start_line_no: 1, end_line_no: 4,          // boundary line counts in PREV segment
  prior_summary: None,                        // first segment, no prior
  transcript: "USER: earlier user msg\nASSISTANT: earlier assistant reply",
}
Segment {
  start_line_no: 5, end_line_no: 6,
  prior_summary: Some("earlier we discussed X"),  // boundary's payload.message
  transcript: "USER: later user msg\nASSISTANT: later assistant reply",
}
```

Note: `prior_summary` of the **first** segment is `None` even if there was
no prior content; `prior_summary` of any subsequent segment is `Some(<the
preceding boundary's payload.message>)`, including `Some("")` when the
boundary's message field is empty.

---

## 3. `EXTRACTION_PROMPT` (backend.rs:1564-1647)

This is the full distillation system prompt. The `[TRANSCRIPT]` block the
LLM sees is exactly what `render_item` produces (§1.5), joined by `\n`. The
`[PRIOR CONTEXT SUMMARY]` block is the `Segment.prior_summary` when
`Some(_)`. **The No-Echo / No-Meta / Evidence-bound rules are what your
"reaction reverse cases" (`expected_kp_count: 0`) must trip.**

```text
# ROLE

You are a Memory Extractor for an AI software-engineering agent's work log. The agent has just spent a thread debugging, designing, and editing code — sometimes its own, sometimes another agent's. Your job is to extract DURABLE, REUSABLE knowledge from that thread: what should the next instance of this agent (or another agent picking up the same work) carry forward into future sessions?

# KINDS OF KNOWLEDGE (four)

- decision — a choice the agent or user made and the reason behind it. "Chose X over Y because Z."
- failure — something that broke, including the trigger condition and the fix. "X breaks under Y; resolved by Z."
- pattern — a reusable idiom inside THIS codebase / system. "The way to do K here is Z."
- fact — a non-obvious concrete fact about the code or system. "File P contains Q." "Service R requires S."

# INTEGRITY RULES (non-negotiable)

- No Echo. Don't restate the user's task assignment or the agent's question as a memory. The task itself is not a durable lesson.
- No Meta. Don't extract observations about the conversation ("the user asked", "the agent decided to..."). Extract the SUBSTANCE of what was learned, not the dialogue around it.
- Evidence-bound. Only extract claims directly supported by the [TRANSCRIPT]. Do not invent context, hypothesize causes, or paraphrase beyond what the messages and tool calls actually establish.
- Specific over generic. "TTL-based cache eviction with a 60s default in src/cache.rs" > "Cache invalidation is hard."
- Skip the trivial. Routine status pings, mid-decisions later revised, one-shot typos and their immediate corrections: not memories. A short, sharp list beats a long, padded one.

# PRIOR CONTEXT

If a [PRIOR CONTEXT SUMMARY] block is given, it summarises earlier work in the same thread that has since been compacted away by the model. Use it ONLY as background — to disambiguate references in the [TRANSCRIPT]. DO NOT extract knowledge points from it. Only extract from [TRANSCRIPT].

# OUTPUT FORMAT

Return a pure JSON array. Each element has exactly three fields:
- "summary": a single-sentence headline of the knowledge point.
- "detail": supporting context (exact error strings, file paths, code symbols, the workaround steps) as a string, OR null if the summary stands alone.
- "kind": one of "decision", "failure", "pattern", "fact".

Output ONLY the JSON array. No prose preamble, no closing remarks, no markdown code fences. If the transcript contains nothing durable, output [].

Soft cap: a typical segment yields 0–5 points. Favour quality over count — a short, sharp list beats a long, padded one. Never pad to hit a number.

# EXAMPLES

## Example 1 — failure

When the user block is:

[TRANSCRIPT]
USER: Run cargo test and see what happens.
ASSISTANT: I'll run the tests.
TOOL CALL bash: {"cmd":"cargo test"}
TOOL RESULT: thread 'tests::backend::s1_c1' panicked at 'SqliteFailure: duplicate column name: origin' at backend.rs:240
ASSISTANT: The v1 ALTER TABLE ran twice — that's not idempotent. I'll wrap the step in PRAGMA user_version gating and re-run.

Your output should be exactly:

[{"summary":"ALTER TABLE ADD COLUMN is not idempotent — rerunning the migration crashes with 'duplicate column name'","detail":"Gate schema changes on PRAGMA user_version; apply a step only when user_version < target and set user_version as the last statement inside the same IMMEDIATE transaction.","kind":"failure"}]

## Example 2 — decision

When the user block is:

[TRANSCRIPT]
USER: Should the ingester use spawn_blocking or switch the whole runtime to multi_thread?
ASSISTANT: We're on a current_thread tokio runtime to keep the binary small. Switching to multi_thread would touch the rest of the bin. spawn_blocking is the right escape hatch for the SQLite + file I/O pass — it offloads to the blocking thread pool without changing the runtime flavor.
ASSISTANT: I'll keep the runtime current_thread and wrap each ingest pass in spawn_blocking.

Your output should be exactly:

[{"summary":"Ingester runs in spawn_blocking on the current_thread runtime instead of switching to multi_thread","detail":"The memory-MCP process uses a current_thread tokio runtime; wrapping the blocking file+SQLite ingest pass in spawn_blocking keeps the MCP server responsive without changing the runtime flavor.","kind":"decision"}]

## Example 3 — nothing durable

When the user block is:

[TRANSCRIPT]
USER: thanks, that worked!
ASSISTANT: Glad it helped!

Your output should be exactly:

[]
```

### 3.1 Prompt-derived implications for your "reverse" cases (expected_kp_count: 0)

When you write `kind-reverse` cases, make sure each trips a **specific** rule
from the prompt, not just "I think this looks empty":

| Case tag | Trip what rule | Concrete shape |
|---|---|---|
| `greeting-only` | Example 3 / Skip the trivial | `USER: thanks, that worked!\nASSISTANT: Glad it helped!` |
| `task-complete-no-learning` | Skip the trivial | A finished task with no error and no surprising fact — just "done X" |
| `chitchat` | No Echo (the task itself isn't a lesson) | Help request with no substance: "can you help me?" "sure, what do you need?" |
| `aborted` | Skip the trivial / mid-decisions revised | Started X, midway changed mind, no concluded answer |
| `meta-only` | No Meta | "the user asked Y, then the agent did Z" without any actual learning |

The few-shot Example 3 in the prompt is **the single most load-bearing
counter-example**. If the corpus' reverse cases look LESS empty than that,
you're not actually exercising the rule.

---

## 4. The four `#[ignore]` live tests (context anchors)

These are the manual `cargo test -- --ignored` tests. They're "anchors"
because the corpus' ground_truth.yaml ultimately should be checkable by the
same plumbing: ingest → distill (s3val anchor), search (s4e anchor),
embed (s4e anchor).

### 4.1 `s3be_e2_live_extract_hits_real_endpoint` (backend.rs:5279-5307)

The simplest end-to-end "is the configured LLM endpoint reachable" test —
ground truth is just "we got at least 1 KP back".

```rust
// ---- S3be.E.2 — #[ignore]'d live test (manual `cargo test -- --ignored`) ----

/// Hits the real endpoint configured in `OPENCRAB_DISTILLER_*`. Not run
/// in CI; run manually with:
///   `cargo test --bin opencrab-memory-mcp -- --ignored s3be_e2`
/// after exporting the env vars. `OPENCRAB_DISTILLER_BASE_URL` must
/// include the provider's API version segment (e.g.
/// `https://api.openai.com/v1`, `https://dashscope.aliyuncs.com/compatible-mode/v1`);
/// the extractor appends `/chat/completions` and nothing else.
#[test]
#[ignore]
fn s3be_e2_live_extract_hits_real_endpoint() {
    let extractor = HttpExtractor::load()
        .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(extractor.extract(
        "USER: I keep crashing with 'duplicate column name: origin' when re-running cargo test.\n\
         ASSISTANT: ALTER TABLE ADD COLUMN is not idempotent — gate the step on PRAGMA user_version inside an IMMEDIATE transaction so a re-run is a no-op.",
        None,
    ));
    let kps = result.expect("live extract should succeed against the configured provider");
    eprintln!("[live] extracted {} knowledge point(s)", kps.len());
    for kp in &kps {
        eprintln!("[live]   kind={} summary={}", kp.kind, kp.summary);
    }
}
```

### 4.2 `s3w_c1_e2e_ingest_then_distill_writes_distill_origin_log_row` (backend.rs:6109-6187)

Closest to "synthesize a tiny rollout → ingest → distill → check log table"
flow. The corpus author can mirror its harness for per-case S3 verification.

```rust
// S3w.C.1 — end-to-end ingest→distill against the real configured
// provider. Not in CI; run manually with:
//   `cargo test --bin opencrab-memory-mcp -- --ignored s3w_c1`
// with `OPENCRAB_DISTILLER_{BASE_URL,MODEL,API_KEY}` exported.
#[test]
#[ignore]
fn s3w_c1_e2e_ingest_then_distill_writes_distill_origin_log_row() {
    let extractor = HttpExtractor::load()
        .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");

    let (_tmp, memory_db, scan_root) = ingest_test_env();
    let agent_id = "agent_s3w_c1";

    // A small but rich transcript: a real failure-with-fix the LLM
    // can almost certainly extract.
    let transcript_lines = vec![
        user_msg_str("My cargo test keeps panicking — what should I check?"),
        assistant_msg_str("I'll look at the failure."),
        assistant_msg_str(
            "The panic is 'SqliteFailure: duplicate column name: origin' — \
             ALTER TABLE ADD COLUMN ran twice. The fix is to wrap the step in \
             PRAGMA user_version gating inside an IMMEDIATE transaction so a \
             re-run is a no-op.",
        ),
    ];
    let content = transcript_lines.join("\n") + "\n";
    write_rollout(
        &scan_root,
        SAMPLE_TEAM_ID,
        SAMPLE_PROJECT_HASH,
        SAMPLE_THREAD_UUID,
        SAMPLE_ISO_TS,
        &content,
    );

    let conn = open(&memory_db).unwrap();
    let ingest_stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
    assert!(ingest_stats.events_inserted >= 3);

    // Force the idle trigger by handing distill_once a now_ms far
    // ahead of the just-set last_growth_ts.
    let now_ms = current_time_ms() + DISTILL_IDLE_MS + 60_000;
    let stats = block_on(distill_once(&conn, &extractor, now_ms)).unwrap();

    assert_eq!(stats.threads_seen, 1);
    assert_eq!(stats.threads_triggered, 1);
    eprintln!("[live] distill stats: {stats:?}");

    let distill_rows: i64 = count_rows(
        &conn,
        "SELECT count(*) FROM log WHERE origin = 'distill'",
    );
    assert!(
        distill_rows >= 1,
        "the live extractor should have produced at least one distilled row"
    );
    let mut stmt = conn
        .prepare(
            "SELECT summary, detail, kind FROM log \
             WHERE origin = 'distill' ORDER BY id",
        )
        .unwrap();
    for row in stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .unwrap()
    {
        let (summary, detail, kind) = row.unwrap();
        eprintln!(
            "[live] distilled: kind={:?} summary={} detail={:?}",
            kind, summary, detail
        );
    }
}
```

Note: `user_msg_str` / `assistant_msg_str` / `write_rollout` / `ingest_test_env`
/ `SAMPLE_*` constants are test helpers defined elsewhere in the file. Per §6.2
the corpus author does NOT need these — building dirs, running the pipeline,
and asserting are the corpus-runner harness's job, not the synthesis side's.
Shown here only so the test reads as complete.

### 4.3 `s3val_real_rollout_distill` (backend.rs:6342-6419) — priority anchor

This is the "drive ingest + distill against a real agent's directory" test —
it's what the **corpus harness will most closely resemble**. The agent it
points at (`agent_1e17677e-4a5b-40db-8113-3aeba71635d4`) was chosen for
content-richness; if it's gone, repoint at any agent dir under
`~/.opencrab/agents/`.

```rust
// -----------------------------------------------------------------
// Phase 6 Step 3-val — one-shot quality look at a REAL rollout
// -----------------------------------------------------------------

/// Drive the full S2 ingest → S3 distill chain against a *real*
/// agent's rollout files. Not a CI test — runs only with:
///   `cargo test --bin opencrab-memory-mcp -- --ignored \
///        s3val_real_rollout_distill --nocapture`
/// (and the distiller must be configured via env or
/// `~/.opencrab/distiller.json`).
///
/// The agent below was selected because its `team_sessions/`
/// holds rollouts with substantive content (a real DB-choice
/// decision the PM was asked to record), not just kickoff stubs.
/// Re-point at a different agent if this one is no longer present.
#[test]
#[ignore]
fn s3val_real_rollout_distill() {
    let extractor = HttpExtractor::load()
        .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");

    let home = std::env::var("HOME").expect("HOME");
    let scan_root: std::path::PathBuf = std::path::PathBuf::from(home).join(
        ".opencrab/agents/agent_1e17677e-4a5b-40db-8113-3aeba71635d4/team_sessions",
    );
    assert!(
        scan_root.exists(),
        "no scan_root at {} — re-point at a real agent dir or skip",
        scan_root.display()
    );

    let tmp = tempfile::tempdir().unwrap();
    let memory_db = tmp.path().join("memory.db");

    let conn = open(&memory_db).unwrap();
    let ingest = ingest_once(&conn, &scan_root, "agent_s3val").unwrap();
    eprintln!("[s3val] ingest stats: {ingest:?}");

    // Thread-scale glimpse: how big is each thread?
    let mut stmt = conn
        .prepare(
            "SELECT thread_id, last_line_no FROM raw_thread ORDER BY thread_id",
        )
        .unwrap();
    for row in stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
    {
        let (tid, lines) = row.unwrap();
        eprintln!("[s3val] thread {} = {} lines", tid, lines);
    }

    // Force the idle trigger so every thread distills this pass.
    let far_future = current_time_ms() + DISTILL_IDLE_MS + 60_000;
    let stats = block_on(distill_once(&conn, &extractor, far_future)).unwrap();
    eprintln!("[s3val] distill stats: {stats:?}");

    // Dump every distilled row verbatim.
    let mut stmt = conn
        .prepare(
            "SELECT id, summary, detail, kind FROM log \
             WHERE origin = 'distill' ORDER BY id",
        )
        .unwrap();
    let rows: Vec<(i64, String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    eprintln!("[s3val] total distilled rows = {}", rows.len());
    for (id, summary, detail, kind) in &rows {
        eprintln!("[s3val] -------- log id={} kind={:?}", id, kind);
        eprintln!("[s3val]   summary: {}", summary);
        eprintln!("[s3val]   detail:  {:?}", detail);
    }
}
```

### 4.4 `s4e_d1_live_embed_against_siliconflow` (backend.rs:7207-7294) — priority anchor

Embedding + KNN sanity. The corpus' S4 (retrieval) verification will follow
the same shape: seed N log rows, embed, do FTS+vector hybrid (or pure vec
MATCH), assert ordering.

```rust
// ==== D — #[ignore] live: real SiliconFlow /v1/embeddings ====

/// Manual: `cargo test --bin opencrab-memory-mcp -- --ignored \
///   s4e_d1_live_embed_against_siliconflow --nocapture`
#[test]
#[ignore]
fn s4e_d1_live_embed_against_siliconflow() {
    let embedder = HttpEmbedder::load()
        .expect("OPENCRAB_EMBED_MODEL env OR distiller.json embed_model required");

    let (_tmp, db) = db_path();
    let conn = open(&db).unwrap();
    // Two log rows with semantically different content + a third
    // closer to row A. KNN-against-A's embedding should rank
    // (A, A-related) ahead of (B).
    let id_migration = seed_log_row(
        &conn,
        "cargo test failed: duplicate column name origin",
        Some("ALTER TABLE ADD COLUMN ran twice; PRAGMA user_version gating fixes it"),
    );
    let id_lunch = seed_log_row(
        &conn,
        "lunch menu discussion",
        Some("we agreed on dumplings for the team lunch"),
    );
    let id_other_migration = seed_log_row(
        &conn,
        "SQLite ALTER TABLE migration",
        Some("idempotent schema upgrade via user_version PRAGMA"),
    );

    let stats = block_on(embed_pending_once(&conn, &embedder)).unwrap();
    eprintln!("[live-embed] stats: {stats:?}");
    eprintln!("[live-embed] dim: {}", embedder.dimensions());
    assert_eq!(stats.rows_embedded, 3);

    // Sanity: each log_vec row must hold a 4096-d vector.
    // sqlite-vec stores vectors as opaque; we infer from the
    // distance column behaving sensibly.
    let q_text = "ALTER TABLE migration not idempotent";
    let q_vec = block_on(embedder.embed(&[q_text.to_string()]))
        .expect("live embed of query");
    eprintln!("[live-embed] query vector length: {}", q_vec[0].len());
    assert_eq!(
        q_vec[0].len(),
        4096,
        "embedding dimension must match log_vec's float[4096]"
    );

    let q_json = vec_to_match_json(&q_vec[0]);
    let mut stmt = conn
        .prepare(
            "SELECT log.id, log.summary, log_vec.distance \
             FROM log_vec JOIN log ON log.id = log_vec.rowid \
             WHERE log_vec.embedding MATCH ?1 AND log_vec.k = ?2 \
             ORDER BY log_vec.distance",
        )
        .unwrap();
    let rows: Vec<(i64, String, f64)> = stmt
        .query_map(params![&q_json, 3_i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    eprintln!("[live-embed] KNN rows (id, summary, distance):");
    for (id, summary, dist) in &rows {
        eprintln!("  id={id} dist={dist:.4} summary={summary}");
    }
    assert_eq!(rows.len(), 3);

    // The two migration-related rows should outrank the lunch row.
    let lunch_pos = rows
        .iter()
        .position(|(id, _, _)| *id == id_lunch)
        .expect("lunch row in results");
    let migration_positions: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, (id, _, _))| *id == id_migration || *id == id_other_migration)
        .map(|(i, _)| i)
        .collect();
    assert!(
        migration_positions.iter().all(|&pos| pos < lunch_pos),
        "both migration rows must rank above the lunch row; got order: {:?}",
        rows.iter().map(|r| r.0).collect::<Vec<_>>()
    );
}
```

---

## 5. Real rollout sample anchors (de-sensitized)

I'm not pasting full rollout files here — they're 30 KB to 130 KB each, and
most of that bulk is the codex `session_meta` line carrying the whole base
instructions verbatim (always Dropped). What follows is what you actually
need to anchor synthesis:

### 5.1 Line-type skeleton — what we see in 108 real rollouts

Distribution of `(top.type, payload.type)` pairs, with the `parse_line`
classification we derived in §1. Counts are from a representative sweep
across small (12-line) and large (50-line) rollouts.

| `top.type` | `payload.type` | parse_line | Notes |
|---|---|---|---|
| `session_meta` | n/a | **Dropped** | Always L1. Carries 30+ KB of base instructions. Always present. |
| `turn_context` | n/a | **Dropped** | Usually 1 per turn. |
| `event_msg` | `task_started` | **Dropped** | Usually 1 per turn. |
| `event_msg` | `task_complete` | **Dropped** | Usually 1 per turn. |
| `event_msg` | `user_message` | **Dropped** | A *mirror* of the response_item user message — same text appears twice. |
| `event_msg` | `agent_message` | **Dropped** | Mirror of assistant message. |
| `event_msg` | `agent_reasoning` | **Dropped** | Mirror of reasoning. |
| `event_msg` | `token_count` | **Dropped** | Usage stats. |
| `response_item` | `message` (role=user) | **Kept** | The actual user prompt. |
| `response_item` | `message` (role=assistant) | **Kept** | The actual assistant reply. |
| `response_item` | `message` (role=developer) | **Dropped** | Sandbox / skills boilerplate. |
| `response_item` | `function_call` | **Kept** | Tool call (name + args + call_id). |
| `response_item` | `function_call_output` | **Kept** | Tool result (call_id + output). |
| `response_item` | `reasoning` | **Dropped** | Silent chain-of-thought. |
| `response_item` | `compaction` / `context_compaction` | **Dropped** | Encrypted-internal markers. |
| `compacted` (top-level) | (uses payload.message) | **Boundary** | **NEVER seen in 108 real rollouts** — synthesize. |

**Mirror caveat**: real rollouts duplicate every user message as both a
`response_item.message` (Kept) and an `event_msg.user_message` (Dropped).
Same for assistant. **Your synthetic rollouts must also include the mirror
or the corpus will look unrealistic and the ingester's behavior may differ**
(the mirror itself is Dropped, but the dual-write pattern is what the
ingester's offset arithmetic was tuned against).

**ToolMisc caveat**: the `local_shell_call` / `tool_search_call` /
`tool_search_output` / `custom_tool_call` / `custom_tool_call_output` /
`web_search_call` variants (the `ToolMisc` group, §1.4) appear in **0 of 108**
real rollouts on this machine — every tool call we saw was a plain
`function_call` (`exec_command`). Treat ToolMisc as synthesize-only and
edge-tier: if a case needs one, build it from the `parse_tool_misc` shape
(§1.4) + the render format (§1.5 table); there is no real anchor to imitate
and the corpus doesn't need to prioritize these.

### 5.2 Sample A — minimal trivial thread (12 lines, real, de-sensitized)

Source: `~/.opencrab/archived_sessions/rollout-2026-05-15T18-59-57-019e2b4b-078d-7ce0-87f3-aedb785f6695.jsonl`.

For brevity I show each line as `<L#> <classification> <one-line description
of the payload>` — the corpus author should regenerate full JSON via the
shapes in §1.

```
L1  Dropped  type=session_meta — cwd, originator, base_instructions blob (~30KB)
L2  Dropped  type=event_msg payload.type=task_started — turn_id, started_at
L3  Dropped  type=response_item payload.type=message role=developer
           — permissions + skills instructions wrapped in XML tags
L4  Kept    type=response_item payload.type=message role=user
           — text="<environment_context><cwd>...</cwd>...</environment_context>"
           → renders to "USER: <environment_context>...</environment_context>"
L5  Dropped  type=turn_context — turn_id, model, sandbox_policy
L6  Kept    type=response_item payload.type=message role=user
           — text="You create concise run metadata...Task:\n你好你是谁"
           → renders to "USER: You create concise run metadata...\n你好你是谁"
L7  Dropped  type=event_msg payload.type=user_message — mirror of L6 (same text)
L8  Dropped  type=event_msg payload.type=token_count — no info yet
L9  Dropped  type=event_msg payload.type=agent_message — mirror of L10
L10 Kept    type=response_item payload.type=message role=assistant
           — text='{"title":"Identify Agent Role","worktreeName":"chore/identify-agent-role"}'
           → renders to "ASSISTANT: {\"title\":\"Identify Agent Role\",...}"
L11 Dropped  type=event_msg payload.type=token_count — full usage stats
L12 Dropped  type=event_msg payload.type=task_complete — turn_id, completed_at
```

After `segment_thread`:
```
Segment {
  start_line_no: 1, end_line_no: 12,
  prior_summary: None,
  transcript: "USER: <environment_context>...\nUSER: You create concise...\n你好你是谁\nASSISTANT: {...}"
}
```

This is **a `task-complete-no-learning` reverse case** in the corpus
taxonomy — the agent received a task, did it, no failure, no decision, no
durable knowledge. Expected `kp_count: 0`.

### 5.3 Sample B — `function_call` + `function_call_output` pair (structure only, fully de-sensitized)

Derived from a real pair, then scrubbed hard: all real paths → generic
placeholders, all real project filenames → generic names. **Only the
envelope shape and field layout are real; every string is replaceable.**

```json
{"timestamp":"2026-05-07T03:01:49.512Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\": \"find /Users/USER/code/PROJECT -maxdepth 3 -type f | head -80\"}","call_id":"call_PLACEHOLDER01"}}
```

renders to (via render_item):
```
TOOL CALL exec_command: {"cmd": "find /Users/USER/code/PROJECT -maxdepth 3 -type f | head -80"}
```

Paired output (same `call_id` links them — though render drops the id):
```json
{"timestamp":"2026-05-07T03:01:49.864Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_PLACEHOLDER01","output":"Chunk ID: 6d2dd0\nWall time: 0.0000 seconds\nProcess exited with code 0\nOriginal token count: 472\nOutput:\n/Users/USER/code/PROJECT/service_main\n/Users/USER/code/PROJECT/service_main.cc\n/Users/USER/code/PROJECT/README.md\n..."}}
```

renders to:
```
TOOL RESULT: Chunk ID: 6d2dd0
Wall time: 0.0000 seconds
Process exited with code 0
Original token count: 472
Output:
/Users/USER/code/PROJECT/service_main
/Users/USER/code/PROJECT/service_main.cc
...
```

What's load-bearing (keep) vs replaceable (swap freely):
- **Keep**: the `Chunk ID:` / `Wall time:` / `Process exited with code N` /
  `Original token count: N` / `Output:\n` framing — this is the **real codex
  `exec_command` output envelope**. Synthesize tool outputs inside this
  envelope for realism. The actual command output starts after `Output:\n`.
- **Replace**: paths (`/Users/USER/code/PROJECT/...`), filenames
  (`service_main` etc. are already generic stand-ins), `call_id` (any opaque
  `call_*` string), the chunk hash. None carry meaning to the parser.

**De-sensitization standard applied across this whole doc (per author's
"go hard" directive):** no real keys/tokens/credentials (a full sweep of all
108 rollouts for key-shaped strings came back **empty** — none were present
to begin with); real abs paths → `/Users/USER/code/PROJECT`; real project /
company names → generic (`PROJECT`, or `AcmeCorp`-style in synthesized
content). The anchors exist for **shape + language style only**; the corpus
author replaces all content anyway.

### 5.4 Resume vs Compaction — two DIFFERENT mechanisms, don't conflate

These are independent and the corpus needs both. One is in-file; one is
cross-file.

| | **Compaction** (in-thread) | **Resume** (cross-session) |
|---|---|---|
| What it is | codex compressed earlier context mid-session | agent stopped, later started a NEW codex session |
| Physical shape | **1 file**, one `type:"compacted"` line mid-file | **2+ files**, each its own `rollout-<ISO>-<uuid>.jsonl` |
| thread_id | **same** throughout (1 file = 1 thread) | **different** per file (one thread per file) |
| Captured by | `segment_thread` → splits into segments; boundary's `payload.message` → next segment's `prior_summary` | nothing links them — both are independent `raw_thread` rows |
| `parent_thread_id` | n/a (one thread) | **NULL** — the resume link is NOT recorded (§0.7) |
| Corpus tags | `multi-segment-compaction`, `with-prior-summary` | resume / multi-thread end-to-end (§4.5) |

#### 5.4a Compaction — synthesized `type:"compacted"` boundary (NO real anchor)

0 of 108 real rollouts have one, so this is pure synthesis. Canonical shape,
derived from `parse_line` lines 910-918:

```json
{"timestamp":"2026-05-15T11:30:00.000Z","type":"compacted","payload":{"message":"Earlier in this thread the user and agent agreed to use Postgres for the orders table. The agent reviewed three options (Postgres, MySQL, MongoDB) and the user picked Postgres for ACID + existing ops familiarity. Implementation was deferred to a follow-up turn."}}
```

The synthesizer only needs `type` and `payload.message`. `timestamp` is
preserved for human inspection (real lines have it). Everything else in
`payload` is fine to omit or add as noise — `parse_line` does
`v.get("payload").and_then(|p| p.get("message"))` and ignores siblings.

**Authoring tip**: write `payload.message` as a real mid-thread codex
compaction would read — a 2-4 sentence factual recap. The **post-boundary
segment** is the continuation with NEW work. `EXTRACTION_PROMPT` instructs
the LLM to use `[PRIOR CONTEXT SUMMARY]` only for disambiguation and **NOT**
extract KPs from it — so KPs come only from the post-boundary transcript, and
the boundary text is context, never a source. A clean in-file supersede looks
like:

- pre-compacted segment: agent works toward Redis cache
- compacted boundary message: "We had decided on Redis for the cache layer."
- post-compacted segment (NEW work): "After profiling, Postgres LISTEN/NOTIFY
  beat Redis here." → yields a `decision` KP about switching to Postgres;
  ground_truth marks a `supersede` relation, older (Redis) `superseded_by`
  newer (Postgres). Within one file, segment order gives the temporal arrow.

#### 5.4b Resume — multiple files, distinct UUIDs, NULL parent link

A resume case is two (or more) physical rollout files under the **same**
`<team>/<hash>/`, each with a **distinct** thread UUID:

```
<scan_root>/<team>/<hash>/rollout-2026-05-20T09-00-00-<uuid-A>.jsonl   # session 1: chose Redis
<scan_root>/<team>/<hash>/rollout-2026-05-22T14-00-00-<uuid-B>.jsonl   # session 2: switched to Postgres
```

Both ingest as independent threads (`parent_thread_id = NULL` on both — §0.7),
distill independently, and their KPs land in `log` side by side. There is **no
structural link** for the consolidation step to follow. So a resume-based
supersede case must encode the relationship two ways in ground_truth:

- **temporal**: `created_at_offset` (session-1 file is N hours earlier than
  session-2). The harness injects this as `log.ts` — NOT derivable from
  `parent_thread_id`, which is dead.
- **semantic**: the two KPs' content must be close enough that the S5
  LLM-judge recognizes "same question (cache backend), later answer overrides
  earlier" → `supersede`.

Do **not** add a ground_truth assertion like `parent_thread_id == <uuid-A>` —
it will always read NULL and the case will fail for the wrong reason.

The spec's §4.5 end-to-end composite cases ("10-20 cases as one agent's real
workflow") are the natural home for resume chains: several sessions over
simulated days, KPs accumulating and occasionally superseding across files.

---

## 6. Resolutions (author's calls, locked) + the one finding that changes design

All six reverse-asks from the first draft are now answered by the corpus
author. Recorded here so nobody re-litigates them.

1. **Truncation behavior** — resolved: source not needed. Behavior is stated
   in full at §0.3 (keep tail, drop head, 48k cap) and the `long-truncation`
   ground-truth pattern (tail fact asserted present, head decoy asserted
   absent) is spelled out there. No `truncate_transcript()` source dump.

2. **Test-helper fixtures** — resolved: NOT needed. The synthesis side
   produces only static data + annotations. Building agent dirs, running the
   pipeline, and asserting are the corpus-runner harness's job (author's
   side). Clean responsibility split: `user_msg_str` / `write_rollout` /
   `SAMPLE_*` stay out of this handoff.

3. **ToolMisc in real data** — resolved: **0 of 108 files** contain any
   ToolMisc variant (audited). Edge-tier, synthesize-only; see the ToolMisc
   caveat under §5.1. No real sample provided (none exists).

4. **`raw_event` ingester internals** — resolved: NOT needed. The ingester is
   a black box to the synthesis side; their interface is "produce
   real-shaped rollout files + annotations." Offset / torn-line / shrink
   logic is covered by the author's existing unit tests. (The one ingester
   fact that DOES leak into authoring — the scan-path template + attribution
   rules — is now in §0.2, because the harness has to lay files out to match
   it.)

5. **De-sensitization** — resolved: **go hard.** Priority: (1) any
   key/token/credential — a full sweep of all 108 rollouts for key-shaped
   strings came back empty, so none are present, but the bar stands; (2) real
   paths → `/Users/USER/code/PROJECT`; (3) real project/company names →
   generic (`AcmeCorp`-style). Anchors are for shape + language style only.
   §5.3 hardened accordingly (real project filenames generalized).

6. **Resume vs Compaction** — resolved: they are **two different
   mechanisms**, both needed, never conflated. Compaction = single file +
   `type:"compacted"` boundary. Resume = multiple files + distinct thread
   UUIDs. Full treatment + table in §5.4. See the finding below for why the
   resume link can't be asserted structurally.

### 6.1 Finding that changes S5 supersede design — `parent_thread_id` is dead

The corpus author flagged a risk: "if the ingester never fills
`parent_thread_id`, the resume parent chain isn't captured and resume linkage
can't be tested." **Confirmed true** (see §0.7 for the source): the ingester
deliberately leaves `parent_thread_id = NULL`, and no current code fills it.

Net for S5: supersede across a resume (two files) cannot rely on a parent
chain — it must be encoded via the spec's `created_at_offset` (harness-injected
`log.ts`, the temporal arrow) + KP content (what the LLM-judge compares). This
**confirms** the spec's §4.4 design rather than breaking it; just don't write
a ground_truth assertion that reads `parent_thread_id`. In-file compaction
supersede (single file, §5.4a) gets its temporal arrow for free from segment
order.

---

## 7. Sanity check before bulk authoring

Recommend the corpus author do this for **case C001** (the first case)
before writing anything else:

1. Drop the rollout into a temp dir matching the expected directory shape
   (per spec §3).
2. Run `cargo test --bin opencrab-memory-mcp -- --ignored s3val_real_rollout_distill --nocapture` against a hand-pointed `scan_root` set to the temp dir.
3. Inspect the dumped distilled rows.
4. Adjust ground_truth.yaml `expected_distill.knowledge_points` so the
   `summary_must_contain_any` / `detail_must_mention_all` assertions actually
   match what the configured LLM emitted — but don't over-fit: leave the OR
   sets broad enough that a slightly different paraphrase still passes.
5. Once C001 round-trips clean, generalize the harness, then bulk-author.

This is the same loop §7 of the spec describes for Phase 1 (10-15 cases) —
do it once, then scale.

---

**End of handoff. Source of truth for every excerpt above:
`CodexMonitor/src-tauri/src/bin/opencrab-memory-mcp/backend.rs` at the
cited line ranges. Regenerate this file if you re-cut Phase 6.**
