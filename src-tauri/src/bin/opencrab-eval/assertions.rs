// Per-scenario assertions over a captured turn.
//
// Loose structural checks, not phrase pins — the assert is "agent emitted
// the tag family we expected" and "agent did/didn't call code-modifying
// tools". Behavioral correctness lives in the human-readable transcript
// the harness prints alongside each verdict.
//
// `<propose_plan>` / `<task>` detection is a minimal scan that mirrors the
// production parser's open/close tokens (cf. `sidecar_session::plan_parser`).
// We don't reuse the parser symbol directly because it lives behind
// `pub(crate)` in the codex-monitor crate; reproducing the open/close
// grammar in ~30 lines keeps the bin self-contained and avoids dragging
// in the whole `sidecar_session` tree via `#[path]`.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Tool-call notification methods we treat as code-modifying. Empty for
/// fixture 3's pass condition.
const CODE_MODIFYING_TOOL_HINTS: &[&str] = &["apply_patch", "edit_file", "write_file"];

/// Notification methods that signal a tool/command being executed (any
/// kind). Used to print a brief tool-call summary in the transcript.
const TOOL_CALL_METHOD_PREFIXES: &[&str] = &["item/started", "item/completed"];

#[derive(Debug, Clone)]
pub struct TurnResult {
    /// The agent's final assistant text for the turn (concatenated
    /// `agentMessage` payloads + completed-item text content). Lossy but
    /// good enough for prompt-level assertions.
    pub final_text: String,
    /// Every notification observed between sending the user turn and
    /// `turn/completed`. Used by assertions to look at tool-call shape.
    pub notifications: Vec<(String, Value)>,
}

/// P6 Step 6 — assertion-side handle to per-fixture filesystem state.
///
/// Existing assertions (F01-F03 + F-mem-trivial-control) ignore this; the
/// post-run memory.db assertions for `F-mem-write` and `F-mem-recall` use
/// `memory_db_path()` to independently open the agent's SQLite log store
/// and verify rows landed (not just that the tool ack came back).
///
/// Paths are derived from `FixtureRun.tempdir` + `Fixture.user_correspondent_id`
/// in the runner; the assertion never sees the tempdir's `Drop` guard, so it
/// is safe to open the file before the runner cleans up.
#[derive(Debug, Clone)]
pub struct RunArtifacts {
    pub tempdir: PathBuf,
    pub home_dir: PathBuf,
    pub user_data_dir: PathBuf,
    pub agent_id: String,
}

impl RunArtifacts {
    pub fn memory_db_path(&self) -> PathBuf {
        self.user_data_dir
            .join("agents")
            .join(&self.agent_id)
            .join("memory.db")
    }
}

/// Open the agent's memory.db read-only and return all `(summary, detail)`
/// rows ordered by id ascending. None if the file does not exist (a
/// pre-condition violation the caller turns into a fail). Errors propagate
/// as a string for the assertion to render.
pub fn read_memory_db_rows(
    memory_db: &Path,
) -> Result<Vec<(i64, String, Option<String>)>, String> {
    if !memory_db.exists() {
        return Err(format!(
            "memory.db does not exist at {} — log_progress never landed",
            memory_db.display()
        ));
    }
    let conn = rusqlite::Connection::open_with_flags(
        memory_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("open memory.db read-only: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT id, summary, detail FROM log ORDER BY id ASC")
        .map_err(|e| format!("prepare select: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let summary: String = row.get(1)?;
            let detail: Option<String> = row.get(2)?;
            Ok((id, summary, detail))
        })
        .map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

#[derive(Debug)]
pub struct AssertionOutcome {
    pub passed: bool,
    pub notes: String,
}

impl AssertionOutcome {
    fn pass(notes: impl Into<String>) -> Self {
        Self {
            passed: true,
            notes: notes.into(),
        }
    }
    fn fail(notes: impl Into<String>) -> Self {
        Self {
            passed: false,
            notes: notes.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture 01 — PM clear request: well-formed <propose_plan>
// ---------------------------------------------------------------------------

pub fn pm_emits_well_formed_plan(
    turn: &TurnResult,
    _artifacts: &RunArtifacts,
) -> AssertionOutcome {
    let blocks = scan_propose_plan_blocks(&turn.final_text);
    if blocks.is_empty() {
        return AssertionOutcome::fail(
            "no <propose_plan> block — Phase 3 regression (PM wrote prose / Markdown instead of XML)",
        );
    }
    // At least one block must contain at least one <task ...>...</task>.
    let any_with_task = blocks.iter().any(|inner| scan_task_count(inner) > 0);
    if !any_with_task {
        return AssertionOutcome::fail(
            "<propose_plan> present but contains no <task> children — bare Markdown bullets \
             would parse to an empty plan in production (plan_parser.rs)",
        );
    }
    let total_tasks: usize = blocks.iter().map(|inner| scan_task_count(inner)).sum();
    AssertionOutcome::pass(format!(
        "{} <propose_plan> block(s); {} <task> child(ren) total",
        blocks.len(),
        total_tasks
    ))
}

// ---------------------------------------------------------------------------
// Fixture 02 — PM ambiguous request: must clarify
// ---------------------------------------------------------------------------

pub fn pm_clarifies_before_decomposing(
    turn: &TurnResult,
    _artifacts: &RunArtifacts,
) -> AssertionOutcome {
    let has_plan = !scan_propose_plan_blocks(&turn.final_text).is_empty();
    let has_question = turn.final_text.contains('?')
        || contains_any_ci(
            &turn.final_text,
            &[
                "could you",
                "can you",
                "to confirm",
                "to clarify",
                "would you",
                "what do you mean",
                "which ",
                "what kind",
                "more detail",
            ],
        );

    match (has_plan, has_question) {
        (false, true) => AssertionOutcome::pass(
            "PM asked clarifying question(s) and did not jump straight to <propose_plan>",
        ),
        (true, true) => AssertionOutcome::pass(
            "PM proposed a plan AND asked clarifying questions — acceptable per the charter \
             (clarify alongside the block, revise after answers)",
        ),
        (true, false) => AssertionOutcome::fail(
            "PM emitted <propose_plan> for an underspecified request and asked no clarifying \
             questions — risk of guessing wrong + delegating wrong",
        ),
        (false, false) => AssertionOutcome::fail(
            "PM neither asked a question nor proposed a plan — output gave the user nothing \
             actionable",
        ),
    }
}

// ---------------------------------------------------------------------------
// Fixture 03 — QA verification: report findings, don't modify code
// ---------------------------------------------------------------------------

pub fn qa_reports_without_modifying(
    turn: &TurnResult,
    _artifacts: &RunArtifacts,
) -> AssertionOutcome {
    // (a) No code-modifying tool call appeared in the turn.
    let mut modifying_tool_calls = Vec::new();
    for (method, params) in &turn.notifications {
        if !TOOL_CALL_METHOD_PREFIXES.iter().any(|p| method.starts_with(p)) {
            continue;
        }
        let blob = params.to_string();
        for hint in CODE_MODIFYING_TOOL_HINTS {
            if blob.contains(hint) {
                modifying_tool_calls.push((method.clone(), (*hint).to_string()));
                break;
            }
        }
    }
    if !modifying_tool_calls.is_empty() {
        let preview: Vec<String> = modifying_tool_calls
            .iter()
            .take(3)
            .map(|(m, h)| format!("{m}({h})"))
            .collect();
        return AssertionOutcome::fail(format!(
            "QA invoked code-modifying tool(s) [{}] — charter says read-only",
            preview.join(", ")
        ));
    }

    // (b) NEW (step 14): the report must sit inside a `<send_message>`
    // block — the only correct outbound form per the comm guide. The
    // `[From X]` shape is INBOUND wrapper only; agents emitting `[From
    // Dave]` (the step-12 regression) as if it were a way to send do not
    // pass. Bare prose outside a tag reaches no one and also fails.
    let send_blocks = scan_send_message_blocks(&turn.final_text);
    if send_blocks.is_empty() {
        return AssertionOutcome::fail(
            "QA reply has no <send_message> block — the only correct outbound form. \
             `[From X]` is the inbound wrapper, not a way to send; bare prose reaches no one.",
        );
    }

    // (c) Finding-language must appear INSIDE the send_message block(s).
    // Heuristics: verification / findings vocabulary, or explicitly states
    // the file can't be found (acceptable for our fixture — the file
    // doesn't actually exist).
    let joined = send_blocks.join("\n");
    let reports = contains_any_ci(
        &joined,
        &[
            "finding",
            "observed",
            "would check",
            "would verify",
            "i ran",
            "i checked",
            "i looked",
            "could not",
            "cannot find",
            "does not exist",
            "no such file",
            "report",
            "issue",
            "concern",
            "missing",
            "did not find",
            "not present",
            "blocked",
            "unable",
        ],
    );
    if !reports {
        return AssertionOutcome::fail(
            "QA <send_message> block does not read as a report — no finding / observation / \
             not-found language inside the tag",
        );
    }
    AssertionOutcome::pass(format!(
        "QA reported via {} <send_message> block(s) with finding-language; no modifying tools",
        send_blocks.len()
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return every `<propose_plan>...</propose_plan>` inner body in `text`.
/// Hand-rolled scan; mirrors the open/close grammar in
/// `sidecar_session/plan_parser.rs` (we only need detection here, not the
/// production parser's full error surface).
fn scan_propose_plan_blocks(text: &str) -> Vec<&str> {
    const OPEN: &str = "<propose_plan>";
    const CLOSE: &str = "</propose_plan>";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open_at) = rest.find(OPEN) {
        let after = &rest[open_at + OPEN.len()..];
        let Some(close_at) = after.find(CLOSE) else {
            break;
        };
        out.push(&after[..close_at]);
        rest = &after[close_at + CLOSE.len()..];
    }
    out
}

// ---------------------------------------------------------------------------
// Step 18 — tool-mode assertions (over McpToolCall items)
// ---------------------------------------------------------------------------
//
// When eval runs with the `opencrab-team-mcp` stub registered, agent
// coordination actions surface as codex `item/completed` notifications of
// `type: "mcpToolCall"` carrying `server`, `tool`, and a full `arguments`
// JSON. These assertions read that structured signal instead of scanning
// agent_message text.

/// Scan a notification stream for completed MCP tool calls. Returns
/// `(server, tool, arguments)` triples in document order.
pub fn scan_mcp_tool_calls(notifs: &[(String, Value)]) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    for (method, params) in notifs {
        if method != "item/completed" {
            continue;
        }
        let Some(item) = params.get("item") else {
            continue;
        };
        let Some(t) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if t != "mcpToolCall" {
            continue;
        }
        let server = item
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let tool = item
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let args = item
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Null);
        out.push((server, tool, args));
    }
    out
}

// -- Fixture 01 (clear request) — PM must call propose_plan ---------------

pub fn pm_calls_propose_plan_tool(
    turn: &TurnResult,
    _artifacts: &RunArtifacts,
) -> AssertionOutcome {
    let calls = scan_mcp_tool_calls(&turn.notifications);
    let plan_calls: Vec<&(String, String, Value)> =
        calls.iter().filter(|(_, tool, _)| tool == "propose_plan").collect();
    if plan_calls.is_empty() {
        // Same Phase-3-style regression, but now the failure shape is
        // "didn't call the tool" instead of "wrote Markdown bullets".
        return AssertionOutcome::fail(
            "no propose_plan tool call — agent answered in prose / sidestepped the tool",
        );
    }
    let task_count: usize = plan_calls
        .iter()
        .filter_map(|(_, _, args)| args.get("tasks").and_then(Value::as_array).map(|a| a.len()))
        .sum();
    if task_count == 0 {
        return AssertionOutcome::fail(
            "propose_plan called but `tasks` array is empty — no actual plan",
        );
    }
    AssertionOutcome::pass(format!(
        "{} propose_plan call(s); {} task(s) total",
        plan_calls.len(),
        task_count
    ))
}

// -- Fixture 02 (ambiguous) — PM must NOT propose ------------------------

pub fn pm_does_not_propose_when_ambiguous_tool(
    turn: &TurnResult,
    _artifacts: &RunArtifacts,
) -> AssertionOutcome {
    let calls = scan_mcp_tool_calls(&turn.notifications);
    let plan_calls: Vec<&(String, String, Value)> =
        calls.iter().filter(|(_, tool, _)| tool == "propose_plan").collect();
    if !plan_calls.is_empty() {
        return AssertionOutcome::fail(format!(
            "PM called propose_plan for an ambiguous request ({} call(s)) — should clarify first",
            plan_calls.len()
        ));
    }
    // Sanity: agent must have produced SOMETHING — either a send_message
    // tool call (asking the user to clarify) or visible prose. A turn that
    // emits neither is a stall, not a clarification.
    let any_send = calls.iter().any(|(_, tool, _)| tool == "send_message");
    let has_text = !turn.final_text.trim().is_empty();
    if !any_send && !has_text {
        return AssertionOutcome::fail(
            "PM neither called send_message nor produced visible prose — empty turn",
        );
    }
    AssertionOutcome::pass(format!(
        "PM did not call propose_plan (response: {})",
        if any_send {
            format!("{} send_message call(s)", calls.iter().filter(|(_, t, _)| t == "send_message").count())
        } else {
            "prose only".to_string()
        }
    ))
}

// -- Fixture 03 (QA) — must call send_message, no modifying tools --------

pub fn qa_calls_send_message_no_modifying_tool(
    turn: &TurnResult,
    _artifacts: &RunArtifacts,
) -> AssertionOutcome {
    // (a) No code-modifying tool call (same hint list as the text-tag version).
    for (method, params) in &turn.notifications {
        if !TOOL_CALL_METHOD_PREFIXES
            .iter()
            .any(|p| method.starts_with(p))
        {
            continue;
        }
        let blob = params.to_string();
        for hint in CODE_MODIFYING_TOOL_HINTS {
            if blob.contains(hint) {
                return AssertionOutcome::fail(format!(
                    "QA invoked code-modifying tool ({hint}) — charter says read-only"
                ));
            }
        }
    }

    // (b) Must have called the `send_message` tool with finding-language
    // in the `body` argument. A QA turn that reports in raw prose, in
    // `[From X]` text, or via any other channel fails — only the
    // structured tool call counts as "the report reached the recipient".
    let calls = scan_mcp_tool_calls(&turn.notifications);
    let send_calls: Vec<&(String, String, Value)> =
        calls.iter().filter(|(_, tool, _)| tool == "send_message").collect();
    if send_calls.is_empty() {
        return AssertionOutcome::fail(
            "QA did not call send_message — used prose or another channel; \
             the report did not reach the recipient",
        );
    }
    let any_findings = send_calls.iter().any(|(_, _, args)| {
        let body = args
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("");
        contains_any_ci(
            body,
            &[
                "finding",
                "observed",
                "would check",
                "would verify",
                "i ran",
                "i checked",
                "i looked",
                "could not",
                "cannot find",
                "does not exist",
                "no such file",
                "report",
                "issue",
                "concern",
                "missing",
                "did not find",
                "not present",
                "blocked",
                "unable",
            ],
        )
    });
    if !any_findings {
        return AssertionOutcome::fail(
            "QA send_message body has no finding / observation / not-found language",
        );
    }
    AssertionOutcome::pass(format!(
        "QA called send_message ({} call(s)) with finding-language; no modifying tools",
        send_calls.len()
    ))
}

// ---------------------------------------------------------------------------
// P6 Step 6 — F-mem-* formal fixture assertions (promoted from b1-probe-*)
// ---------------------------------------------------------------------------
//
// Step 5 b1-probe assertions were the visibility / behaviour preview.
// Step 6 promotes them: same call-level pins PLUS an independent disk-side
// pin via `RunArtifacts::memory_db_path`. Direct guard against the P5
// pattern "tool ack came back ok but no row landed" — we open memory.db
// ourselves via rusqlite OPEN_READ_ONLY and verify the row(s) match the
// mcpToolCall args we saw on the wire.

/// F-mem-write (promoted from b1-probe-write) — after a substantive
/// decision + "remember for tomorrow" cue, the agent must:
///   1. call `log_progress` ≥ 1× with a well-formed summary,
///   2. produce zero `<daily_log>` text-tag occurrences (P5 corpse check),
///   3. land that summary on disk in `<home>/.opencrab/agents/<id>/memory.db`
///      — read independently via rusqlite, not trusted from the tool ack.
pub fn pm_logs_progress_after_decision(
    turn: &TurnResult,
    artifacts: &RunArtifacts,
) -> AssertionOutcome {
    if turn.final_text.contains("<daily_log>") {
        return AssertionOutcome::fail(
            "final_text contains the dead P5 `<daily_log>` tag — must be 0 (replaced by \
             log_progress tool in P6)",
        );
    }
    let calls = scan_mcp_tool_calls(&turn.notifications);
    let log_calls: Vec<&(String, String, Value)> =
        calls.iter().filter(|(_, tool, _)| tool == "log_progress").collect();
    if log_calls.is_empty() {
        return AssertionOutcome::fail(
            "no log_progress tool call — agent didn't record the decision for its future self",
        );
    }
    let mut bad_summary: Option<String> = None;
    let mut observed_summaries: Vec<String> = Vec::new();
    for (_, _, args) in &log_calls {
        let summary = args.get("summary").and_then(Value::as_str).unwrap_or("");
        if summary.is_empty() {
            bad_summary = Some("summary missing or empty".to_string());
            break;
        }
        let len = summary.chars().count();
        if !(5..=300).contains(&len) {
            bad_summary = Some(format!("summary length {len} outside 5..=300"));
            break;
        }
        if !summary.contains(' ') {
            bad_summary = Some("summary is single-word — not a coherent sentence".to_string());
            break;
        }
        observed_summaries.push(summary.to_string());
    }
    if let Some(reason) = bad_summary {
        return AssertionOutcome::fail(format!(
            "log_progress called but summary fails the well-formed check ({reason})"
        ));
    }

    // Independent post-run disk read — the load-bearing P6 Step 6
    // strengthening. Even if the tool acked, the file must contain a row
    // whose `summary` exactly matches one of the observed `args.summary`
    // values. Anything weaker (e.g. "≥1 row") could regress silently to
    // the P5 "ack ok, file empty" bug; we want byte-equality on at least
    // one observed/written pair.
    let memory_db = artifacts.memory_db_path();
    let rows = match read_memory_db_rows(&memory_db) {
        Ok(rows) => rows,
        Err(err) => {
            return AssertionOutcome::fail(format!(
                "post-run memory.db read failed ({err}) — tool ack came back but row never \
                 landed (the P5 regression class)"
            ));
        }
    };
    if rows.is_empty() {
        return AssertionOutcome::fail(format!(
            "memory.db at {} exists but has 0 rows — log_progress acked without writing",
            memory_db.display()
        ));
    }
    let row_summaries: Vec<&str> = rows.iter().map(|(_, s, _)| s.as_str()).collect();
    let matched: Vec<&String> = observed_summaries
        .iter()
        .filter(|s| row_summaries.iter().any(|row| row == &s.as_str()))
        .collect();
    if matched.is_empty() {
        return AssertionOutcome::fail(format!(
            "memory.db has {} row(s) but none of their summary values match the {} \
             mcpToolCall.args.summary observed on the wire (rows={row_summaries:?}, \
             observed={observed_summaries:?})",
            rows.len(),
            observed_summaries.len(),
        ));
    }

    AssertionOutcome::pass(format!(
        "{} log_progress call(s); {} row(s) on disk; {} summary equality match(es)",
        log_calls.len(),
        rows.len(),
        matched.len(),
    ))
}

/// F-mem-trivial-control (promoted from b1-probe-trivial) — content-free
/// pleasantry must NOT trigger `log_progress` and must NOT contain the
/// dead P5 `<daily_log>` tag.
pub fn pm_does_not_log_on_trivial(
    turn: &TurnResult,
    _artifacts: &RunArtifacts,
) -> AssertionOutcome {
    if turn.final_text.contains("<daily_log>") {
        return AssertionOutcome::fail(
            "final_text contains the dead P5 `<daily_log>` tag — must be 0",
        );
    }
    let calls = scan_mcp_tool_calls(&turn.notifications);
    let log_calls: Vec<&(String, String, Value)> =
        calls.iter().filter(|(_, tool, _)| tool == "log_progress").collect();
    if !log_calls.is_empty() {
        return AssertionOutcome::fail(format!(
            "agent called log_progress {} time(s) on a trivial exchange — should keep \
             trivial turns ephemeral",
            log_calls.len()
        ));
    }
    AssertionOutcome::pass("no log_progress call on trivial exchange")
}

// ---------------------------------------------------------------------------
// F-mem-recall — agent must consult its log, recall a specific seeded
// fact (older than the prelude window), and use it in the final reply.
// ---------------------------------------------------------------------------

/// The verifiable facts seeded into the OLDEST row of memory.db by
/// [`crate::fixture::seed_f_mem_recall`]. The assertion checks that the
/// agent's final_text mentions BOTH the database engine name (`Postgres`
/// / `PostgreSQL`) and at least one distinctive seeded detail — proof
/// that recall traversed past the LIMIT-10 prelude window into the older
/// history via `memory_search` / `memory_get`.
pub const F_MEM_RECALL_ENGINE_KEYWORDS: &[&str] = &["postgres", "postgresql"];
pub const F_MEM_RECALL_DISTINCTIVE_FACTS: &[&str] = &[
    "notes-pg-primary.internal.example.com",
    "notes-pg-primary",
    "5432",
    "10k users",
    "10k",
    "2026-04-12",
    "operational cost",
    "near-zero",
];

pub fn pm_recalls_seeded_fact(
    turn: &TurnResult,
    _artifacts: &RunArtifacts,
) -> AssertionOutcome {
    if turn.final_text.contains("<daily_log>") {
        return AssertionOutcome::fail(
            "final_text contains the dead P5 `<daily_log>` tag — must be 0",
        );
    }
    let calls = scan_mcp_tool_calls(&turn.notifications);
    let recall_calls: Vec<&(String, String, Value)> = calls
        .iter()
        .filter(|(_, tool, _)| tool == "memory_search" || tool == "memory_get")
        .collect();
    if recall_calls.is_empty() {
        return AssertionOutcome::fail(
            "no memory_search / memory_get tool call — agent answered from prior context or \
             guessed instead of consulting its log",
        );
    }
    let lower = turn.final_text.to_ascii_lowercase();
    let engine_hit = F_MEM_RECALL_ENGINE_KEYWORDS.iter().any(|k| lower.contains(k));
    let fact_hits: Vec<&&str> = F_MEM_RECALL_DISTINCTIVE_FACTS
        .iter()
        .filter(|f| lower.contains(&f.to_ascii_lowercase()))
        .collect();
    if !engine_hit {
        return AssertionOutcome::fail(format!(
            "{} recall call(s), but final_text mentions neither `Postgres` nor `PostgreSQL` — \
             search may have hit but the answer didn't surface the recalled engine choice",
            recall_calls.len()
        ));
    }
    if fact_hits.is_empty() {
        return AssertionOutcome::fail(format!(
            "{} recall call(s), final_text names Postgres, but contains NONE of the seeded \
             distinctive facts {:?} — looks like a guess shaped by the question, not a recall",
            recall_calls.len(),
            F_MEM_RECALL_DISTINCTIVE_FACTS,
        ));
    }
    AssertionOutcome::pass(format!(
        "{} recall call(s) ({} memory_search / {} memory_get); final_text names Postgres + \
         {} distinctive seeded fact(s): {:?}",
        recall_calls.len(),
        recall_calls.iter().filter(|(_, t, _)| t == "memory_search").count(),
        recall_calls.iter().filter(|(_, t, _)| t == "memory_get").count(),
        fact_hits.len(),
        fact_hits,
    ))
}

/// Return every `<send_message ...>...</send_message>` inner body in
/// `text`. The opening tag has attributes (`to="..." channel="..."`) so
/// we scan the prefix `<send_message`, find the first `>` (end of open
/// tag), then `</send_message>` for the close. Mirrors the production
/// router's `parse_send_message_tags` open/close grammar without dragging
/// the whole `sidecar_session` tree in via `#[path]`.
fn scan_send_message_blocks(text: &str) -> Vec<&str> {
    const OPEN_PREFIX: &str = "<send_message";
    const CLOSE: &str = "</send_message>";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open_at) = rest.find(OPEN_PREFIX) {
        let after_prefix = &rest[open_at + OPEN_PREFIX.len()..];
        let Some(tag_close) = after_prefix.find('>') else {
            break;
        };
        let after_open = &after_prefix[tag_close + 1..];
        let Some(close_at) = after_open.find(CLOSE) else {
            break;
        };
        out.push(&after_open[..close_at]);
        rest = &after_open[close_at + CLOSE.len()..];
    }
    out
}

/// Count `<task ...>...</task>` children inside a propose_plan body. We
/// require the OPENING `<task` token AND the closing `</task>` to call it
/// a task; a stray bullet `- step one` does not count (production parser
/// would reject it the same way).
fn scan_task_count(inner: &str) -> usize {
    const OPEN_TASK_PREFIX: &str = "<task";
    const CLOSE_TASK: &str = "</task>";
    let mut count = 0;
    let mut rest = inner;
    while let Some(open_at) = rest.find(OPEN_TASK_PREFIX) {
        let after_open = &rest[open_at + OPEN_TASK_PREFIX.len()..];
        let Some(close_at) = after_open.find(CLOSE_TASK) else {
            break;
        };
        count += 1;
        rest = &after_open[close_at + CLOSE_TASK.len()..];
    }
    count
}

fn contains_any_ci(haystack: &str, needles: &[&str]) -> bool {
    let lower = haystack.to_ascii_lowercase();
    needles.iter().any(|n| lower.contains(&n.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn turn(text: &str) -> TurnResult {
        TurnResult {
            final_text: text.to_string(),
            notifications: Vec::new(),
        }
    }

    fn stub_artifacts() -> RunArtifacts {
        // Unit-test stub: the text-only assertions do not read disk. The
        // memory-db assertion (F-mem-write) is covered by the end-to-end
        // eval harness, not here — the harness writes a real memory.db.
        RunArtifacts {
            tempdir: PathBuf::from("/tmp/unit-test-stub"),
            home_dir: PathBuf::from("/tmp/unit-test-stub/home"),
            user_data_dir: PathBuf::from("/tmp/unit-test-stub/home/.opencrab"),
            agent_id: "agent_pm".to_string(),
        }
    }

    #[test]
    fn plan_assert_passes_on_well_formed_block() {
        let outcome = pm_emits_well_formed_plan(
            &turn(
                r#"Here is the plan: <propose_plan><task title="step one">body</task></propose_plan>"#,
            ),
            &stub_artifacts(),
        );
        assert!(outcome.passed, "{}", outcome.notes);
    }

    #[test]
    fn plan_assert_fails_on_bare_markdown_bullets() {
        let outcome = pm_emits_well_formed_plan(
            &turn("<propose_plan>\n- step one\n- step two\n</propose_plan>"),
            &stub_artifacts(),
        );
        assert!(!outcome.passed, "{}", outcome.notes);
        assert!(
            outcome.notes.contains("no <task>"),
            "should flag missing <task> children; got: {}",
            outcome.notes
        );
    }

    #[test]
    fn plan_assert_fails_when_no_propose_plan_at_all() {
        let outcome = pm_emits_well_formed_plan(
            &turn("Sure, I'll just do step one, then step two, then step three."),
            &stub_artifacts(),
        );
        assert!(!outcome.passed);
    }

    #[test]
    fn clarify_assert_passes_when_pm_asks_a_question() {
        let outcome = pm_clarifies_before_decomposing(
            &turn("Could you say which part of the API you mean?"),
            &stub_artifacts(),
        );
        assert!(outcome.passed, "{}", outcome.notes);
    }

    #[test]
    fn clarify_assert_passes_when_pm_proposes_with_questions() {
        let outcome = pm_clarifies_before_decomposing(
            &turn(
                r#"Could you confirm scope? <propose_plan><task title="t">b</task></propose_plan>"#,
            ),
            &stub_artifacts(),
        );
        assert!(outcome.passed);
    }

    #[test]
    fn clarify_assert_fails_when_pm_proposes_without_questions() {
        let outcome = pm_clarifies_before_decomposing(
            &turn(r#"<propose_plan><task title="step one">do everything</task></propose_plan>"#),
            &stub_artifacts(),
        );
        assert!(!outcome.passed, "{}", outcome.notes);
    }

    #[test]
    fn qa_assert_fails_when_modifying_tool_called() {
        let result = TurnResult {
            final_text:
                r#"<send_message to="user" channel="chat">I fixed the auth middleware</send_message>"#
                    .to_string(),
            notifications: vec![(
                "item/started".to_string(),
                json!({ "item": { "type": "command", "name": "apply_patch" } }),
            )],
        };
        let outcome = qa_reports_without_modifying(&result, &stub_artifacts());
        assert!(!outcome.passed, "{}", outcome.notes);
        assert!(outcome.notes.contains("apply_patch"));
    }

    #[test]
    fn qa_assert_passes_on_clean_send_message_report() {
        let outcome = qa_reports_without_modifying(
            &turn(
                r#"<send_message to="user" channel="chat">
I could not find apps/api/src/middleware/auth.ts in this workspace; my finding is
that the file does not exist at the path given.
</send_message>"#,
            ),
            &stub_artifacts(),
        );
        assert!(outcome.passed, "{}", outcome.notes);
    }

    #[test]
    fn qa_assert_fails_when_report_is_bare_prose_no_send_message() {
        let outcome = qa_reports_without_modifying(
            &turn(
                "I could not find apps/api/src/middleware/auth.ts in this workspace; \
                 my finding is that the file does not exist.",
            ),
            &stub_artifacts(),
        );
        assert!(!outcome.passed, "should fail without send_message wrapper");
        assert!(
            outcome.notes.contains("<send_message>"),
            "should explain why; got: {}",
            outcome.notes
        );
    }

    #[test]
    fn qa_assert_fails_when_using_from_x_as_outbound_form() {
        let outcome = qa_reports_without_modifying(
            &turn(
                "[From Dave] Auth middleware verification blocked: \
                 apps/api/src/middleware/auth.ts does not exist in the workspace.",
            ),
            &stub_artifacts(),
        );
        assert!(!outcome.passed, "should reject [From X] outbound form");
    }

    #[test]
    fn qa_assert_fails_when_send_message_present_but_no_findings_inside() {
        let outcome = qa_reports_without_modifying(
            &turn(r#"<send_message to="user" channel="chat">hi there</send_message>"#),
            &stub_artifacts(),
        );
        assert!(!outcome.passed, "should require finding-language in the block");
    }
}
