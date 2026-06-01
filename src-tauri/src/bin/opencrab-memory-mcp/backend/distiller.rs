use super::{
    open, current_time_ms, segment_thread, embed_pending_once, consolidation_enabled,
    run_consolidation_step, BackendError, HttpEmbedder, LLM_HTTP_TIMEOUT,
};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Phase 6 Step 3b — distillation extractor (LLM client + prompt + trait)
// ---------------------------------------------------------------------------

/// One distilled knowledge point — the unit the LLM returns and what
/// eventually lands in the `log` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KnowledgePoint {
    pub summary: String,
    pub detail: Option<String>,
    /// One of "decision" | "failure" | "pattern" | "fact" (normalised; any
    /// other value the LLM emits is coerced to "fact").
    pub kind: String,
}

/// Errors from one `Extractor::extract` call. `is_retryable()` separates
/// transient network blips from permanent failures so the orchestrator can
/// decide whether to back off and try again or surface the error.
#[derive(Debug)]
pub enum ExtractError {
    /// Configuration is missing or invalid (env vars unset, malformed URL,
    /// etc.). Operator intervention required — do not retry.
    Config(String),
    /// Network failure or 5xx / 408 / 429 from the upstream — almost always
    /// transient. **Retryable.**
    HttpTransient(String),
    /// 4xx from the upstream other than 408 / 429 (auth, bad request, …).
    /// The exact same request will fail again — not retryable.
    HttpClient(String),
    /// We got a 2xx but couldn't make sense of the body. Same input would
    /// likely re-produce the same garbage, but the orchestrator may
    /// surface it for inspection.
    Parse(String),
}

impl ExtractError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, ExtractError::HttpTransient(_))
    }
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::Config(m) => write!(f, "distill config error: {m}"),
            ExtractError::HttpTransient(m) => write!(f, "distill http transient: {m}"),
            ExtractError::HttpClient(m) => write!(f, "distill http client error: {m}"),
            ExtractError::Parse(m) => write!(f, "distill parse error: {m}"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Distillation contract. Tests mock with simple `impl Extractor`s; the
/// orchestrator owns one trait object so it can flip extractors per pass
/// (e.g., a "dry-run" no-op extractor for diagnostics).
#[async_trait::async_trait]
pub trait Extractor: Send + Sync {
    async fn extract(
        &self,
        transcript: &str,
        prior_summary: Option<&str>,
    ) -> Result<Vec<KnowledgePoint>, ExtractError>;
}

/// OpenAI-compatible `/v1/chat/completions` POST. All knobs come from
/// env so the orchestrator can flip provider without recompiling.
pub struct HttpExtractor {
    pub(crate) client: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) api_key: String,
}

/// Assemble the distiller's user message: optional prior-summary block, then
/// the transcript. Shared by `extract` (the live path) and the test-only
/// `build_request_body`, so the user-block shape has a single definition.
fn extraction_user_block(transcript: &str, prior_summary: Option<&str>) -> String {
    let mut user_block = String::new();
    if let Some(prior) = prior_summary {
        user_block.push_str("[PRIOR CONTEXT SUMMARY]\n");
        user_block.push_str(prior);
        user_block.push_str("\n\n");
    }
    user_block.push_str("[TRANSCRIPT]\n");
    user_block.push_str(transcript);
    user_block
}

/// HTTP client for the distiller / judge. NO silent fallback: a client WITHOUT
/// the timeout is precisely the bug, so a `build()` failure (rare — TLS init)
/// panics loudly rather than degrade to an unbounded client.
fn distiller_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(LLM_HTTP_TIMEOUT)
        .build()
        .expect("build distiller/judge HTTP client with timeout")
}

impl HttpExtractor {
    /// Build from `OPENCRAB_DISTILLER_{BASE_URL,MODEL,API_KEY}`. Returns
    /// `None` if any is missing or empty — the orchestrator treats this
    /// as "distillation disabled" rather than an error (clean local
    /// development).
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("OPENCRAB_DISTILLER_BASE_URL").ok()?;
        let model = std::env::var("OPENCRAB_DISTILLER_MODEL").ok()?;
        let api_key = std::env::var("OPENCRAB_DISTILLER_API_KEY").ok()?;
        if base_url.trim().is_empty() || model.trim().is_empty() || api_key.trim().is_empty() {
            return None;
        }
        Some(Self {
            client: distiller_http_client(),
            base_url,
            model,
            api_key,
        })
    }

    /// Layered credential resolution.
    ///
    /// 1. If all three `OPENCRAB_DISTILLER_*` env vars are present
    ///    (non-empty), use them.
    /// 2. Otherwise, try `<user_root>/distiller.json` — a JSON file with
    ///    `{ "base_url", "model", "api_key" }`. The path is the same
    ///    `~/.opencrab/` root the rest of the bin uses, so this stays
    ///    outside the repo working tree by construction.
    /// 3. Otherwise, `None` (distillation disabled).
    ///
    /// We try env first so an operator can override the file for a
    /// single run without rewriting it. The file fallback exists because
    /// the bin is spawned by codex-cli; shell env vars are not
    /// guaranteed to propagate through that spawn chain.
    pub fn load() -> Option<Self> {
        if let Some(ext) = Self::from_env() {
            return Some(ext);
        }
        Self::from_file()
    }

    fn from_file() -> Option<Self> {
        let root = crate::paths::user_root()?;
        Self::from_file_at(&root.join("distiller.json"))
    }

    fn from_file_at(path: &std::path::Path) -> Option<Self> {
        let body = std::fs::read_to_string(path).ok()?;
        let json: serde_json::Value = serde_json::from_str(&body).ok()?;
        let base_url = json
            .get("base_url")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let model = json
            .get("model")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let api_key = json
            .get("api_key")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        if base_url.is_empty() || model.is_empty() || api_key.is_empty() {
            return None;
        }
        Some(Self {
            client: distiller_http_client(),
            base_url,
            model,
            api_key,
        })
    }

    /// Final URL for the POST. Pure; split out so tests can assert the
    /// `/v1` segment is **not** double-injected. `base_url` is expected to
    /// already include the provider's API version segment (e.g.
    /// `https://api.openai.com/v1`, `https://dashscope.aliyuncs.com/compatible-mode/v1`).
    pub(crate) fn chat_completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }

    /// Pure: assemble a chat/completions request body for an arbitrary
    /// (system, user) message pair. The single body shape behind both the
    /// distiller (`extract`) and the consolidation judge (`judge`) — same
    /// model + temperature, only the two messages differ.
    pub(crate) fn build_chat_body(&self, system: &str, user: &str) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            // Low but non-zero — enough determinism that re-runs on the same
            // input give similar output, without locking the model so hard it
            // can't paraphrase a clearer summary / rationale.
            "temperature": 0.2,
        })
    }

    /// The distiller's request body — `EXTRACTION_PROMPT` as system, the
    /// prior-summary + transcript block as user. A thin, test-covered wrapper
    /// over `build_chat_body` so the extraction body shape stays asserted
    /// (`s4e_a1` / `s3be_b1`). Only `extract`'s tests need it; the live path
    /// goes straight through `post_chat`.
    #[cfg(test)]
    pub(crate) fn build_request_body(
        &self,
        transcript: &str,
        prior_summary: Option<&str>,
    ) -> serde_json::Value {
        self.build_chat_body(EXTRACTION_PROMPT, &extraction_user_block(transcript, prior_summary))
    }

    /// POST one (system, user) chat completion and return the model's answer
    /// with any `<think>…</think>` reasoning stripped. The shared wire layer
    /// behind `extract` and `judge`: build body → POST → classify transient vs
    /// permanent → pull `choices[0].message.content`. The caller owns response
    /// *parsing* (a JSON array for KPs, a JSON object for a verdict).
    pub(crate) async fn post_chat(&self, system: &str, user: &str) -> Result<String, ExtractError> {
        let url = self.chat_completions_url();
        let body = self.build_chat_body(system, user);
        // Hard, reqwest-AGNOSTIC wall around the WHOLE HTTP op (connect→send→read):
        // tokio::time::timeout drops the future at the deadline no matter where it
        // hangs through the proxy. Covers BOTH distill and judge (shared path) —
        // this is the layer the judge-only timeout missed (a distill call hung
        // here 11 min, reqwest's own 120s never firing). Needs the runtime timer.
        let op = async {
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| ExtractError::HttpTransient(format!("send {url}: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                let body_text = resp.text().await.unwrap_or_default();
                // 5xx + 408 (timeout) + 429 (rate limit) → transient. Everything
                // else 4xx is "the request itself is wrong"; retrying won't help.
                let is_transient =
                    status.is_server_error() || status.as_u16() == 408 || status.as_u16() == 429;
                return if is_transient {
                    Err(ExtractError::HttpTransient(format!("HTTP {status}: {body_text}")))
                } else {
                    Err(ExtractError::HttpClient(format!("HTTP {status}: {body_text}")))
                };
            }

            let resp_body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ExtractError::Parse(format!("decode response body: {e}")))?;

            let content = resp_body
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .ok_or_else(|| {
                    ExtractError::Parse(format!(
                        "missing choices[0].message.content in response: {resp_body}"
                    ))
                })?;

            Ok(strip_think_blocks(content))
        };
        match tokio::time::timeout(LLM_HTTP_TIMEOUT, op).await {
            Ok(r) => r,
            Err(_elapsed) => Err(ExtractError::HttpTransient(format!(
                "chat call timed out after {}s (hard wall)",
                LLM_HTTP_TIMEOUT.as_secs()
            ))),
        }
    }
}

#[async_trait::async_trait]
impl Extractor for HttpExtractor {
    async fn extract(
        &self,
        transcript: &str,
        prior_summary: Option<&str>,
    ) -> Result<Vec<KnowledgePoint>, ExtractError> {
        let content = self
            .post_chat(EXTRACTION_PROMPT, &extraction_user_block(transcript, prior_summary))
            .await?;
        parse_knowledge_points(&content).map_err(ExtractError::Parse)
    }
}

/// Tolerant parser for the LLM's reply.
///
/// Defends against three common deviations from "pure JSON array":
/// 1. wrapped in a ```json … ``` markdown fence (or unlabeled ``` … ```);
/// 2. surrounded by prose ("Here is the JSON: […]. Hope this helps!");
/// 3. invalid `kind` values / missing optional fields.
///
/// Returns `Err(String)` only for shapes we can't recover from (no '['
/// anywhere; outer is not a JSON array; entirely non-JSON between the
/// brackets). An empty array is `Ok(vec![])`. Per-element corruption
/// (empty / missing `summary`) results in that element being SILENTLY
/// SKIPPED — the orchestrator prefers ingesting a partial good batch
/// over discarding a whole pass.
pub fn parse_knowledge_points(s: &str) -> Result<Vec<KnowledgePoint>, String> {
    // Qwen3-thinking-style models emit `<think>...</think>` before the
    // actual answer. The think text often contains `[`/`]` which would
    // mislead `extract_json_array_slice`, so strip paired blocks first.
    let unthought = strip_think_blocks(s);
    let unfenced = strip_code_fence(&unthought);
    let sliced = extract_json_array_slice(unfenced)?;
    let value: serde_json::Value = serde_json::from_str(sliced)
        .map_err(|e| format!("invalid JSON: {e} (after fence-strip + bracket-slice)"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| "expected top-level JSON array".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let summary = item
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if summary.is_empty() {
            continue;
        }
        let detail = item.get("detail").and_then(|v| match v {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) if s.is_empty() => None,
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        });
        let kind_raw = item.get("kind").and_then(|v| v.as_str()).unwrap_or("fact");
        let kind = normalize_kind(kind_raw);
        out.push(KnowledgePoint {
            summary,
            detail,
            kind,
        });
    }
    Ok(out)
}

/// Remove every paired `<think>…</think>` block from `s`, non-greedy.
/// Each `<think>` is matched to its first following `</think>`. An
/// unclosed `<think>` (no terminator after it) is left untouched —
/// the downstream parser will fail naturally on the residual gibberish,
/// which is the right outcome because there can't be a valid JSON array
/// after an open think tag anyway.
pub(crate) fn strip_think_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        match rest.find("<think>") {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(start) => {
                let after_open = &rest[start + "<think>".len()..];
                match after_open.find("</think>") {
                    Some(end) => {
                        // Paired: drop `<think>…</think>` entirely.
                        out.push_str(&rest[..start]);
                        rest = &after_open[end + "</think>".len()..];
                    }
                    None => {
                        // Unclosed: keep the rest verbatim and bail.
                        out.push_str(rest);
                        return out;
                    }
                }
            }
        }
    }
}

pub(crate) fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    // ```json\n…\n```
    if let Some(rest) = t.strip_prefix("```json") {
        let rest = rest.trim_start_matches(|c: char| c == '\n' || c == '\r');
        if let Some(without_close) = rest.trim_end().strip_suffix("```") {
            return without_close;
        }
    }
    // ```\n…\n```
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches(|c: char| c == '\n' || c == '\r');
        if let Some(without_close) = rest.trim_end().strip_suffix("```") {
            return without_close;
        }
    }
    t
}

fn extract_json_array_slice(s: &str) -> Result<&str, String> {
    let start = s
        .find('[')
        .ok_or_else(|| "no '[' found in extractor reply".to_string())?;
    let end = s
        .rfind(']')
        .ok_or_else(|| "no ']' found in extractor reply".to_string())?;
    if end < start {
        return Err("array brackets out of order in extractor reply".to_string());
    }
    Ok(&s[start..=end])
}

fn normalize_kind(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    match lower.as_str() {
        "decision" | "failure" | "pattern" | "fact" => lower,
        _ => "fact".to_string(),
    }
}

/// The system prompt for the distiller. Adapted from Mem0's
/// `ADDITIVE_EXTRACTION_PROMPT` (research/mem0/mem0/configs/prompts.py:468)
/// — same skeleton (Role + categories + integrity rules + output format +
/// one few-shot), reframed from consumer-personalization to agent-work-
/// memory (see S3-R Q5.3).
///
/// **All adaptations are visible here**; keep them at this single site so
/// future prompt iteration shows up as a diff on this constant.
pub const EXTRACTION_PROMPT: &str = r#"# ROLE

You are a Memory Extractor for an AI software-engineering agent's work log. The agent has just spent a thread debugging, designing, and editing code — sometimes its own, sometimes another agent's. Your job is to extract DURABLE, REUSABLE knowledge from that thread: what should the next instance of this agent (or another agent picking up the same work) carry forward into future sessions?

# KINDS OF KNOWLEDGE (four)

- decision — a choice the agent or user made and the reason behind it. "Chose X over Y because Z."
- failure — something that broke, including the trigger condition and the fix. "X breaks under Y; resolved by Z."
- pattern — a reusable idiom inside THIS codebase / system. "The way to do K here is Z."
- fact — a non-obvious concrete fact about the code or system. "File P contains Q." "Service R requires S."

# INTEGRITY RULES (non-negotiable)

- No Echo. Don't restate the user's task assignment or the agent's question as a memory. The task itself is not a durable lesson. Performing an action is not itself a knowledge point: "I ran X", "I renamed the files", "the build succeeded", "the command exited 0" are NOT memories — unless the action came with a decision (and its reason), a surprise, a failure, or a durable fact about the system. A routine task that simply completed as expected leaves nothing to carry forward; output nothing for it.
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

## Example 4 — routine successful command, nothing durable

When the user block is:

[TRANSCRIPT]
USER: Get the dev environment running so I can build locally.
ASSISTANT: I'll install deps and build.
TOOL CALL exec_command: {"cmd":"make setup && make build"}
TOOL RESULT: dependencies installed
Build succeeded in 12.3s
ASSISTANT: Environment is ready — dependencies installed and the local build passes.

Your output should be exactly (the build merely succeeded — no decision, surprise, failure, or new fact):

[]

## Example 5 — mechanical task completed, nothing learned

When the user block is:

[TRANSCRIPT]
USER: Rename every markdown file under docs/ so the spaces become hyphens.
ASSISTANT: Sure, I'll rename them.
TOOL CALL exec_command: {"cmd":"rename 's/ /-/g' docs/*.md"}
TOOL RESULT: renamed 3 files
ASSISTANT: Done — renamed 3 files, all succeeded.

Your output should be exactly (a mechanical edit that just completed is an echo of the task, not a lesson):

[]
"#;

// ---------------------------------------------------------------------------
// Phase 6 Step 3-distill — orchestration loop (distill_once)
// ---------------------------------------------------------------------------

/// A thread becomes a distillation candidate when its last growth event is
/// older than this — long enough that the agent has plausibly stopped
/// adding to the thread and we can compress a finished segment safely.
pub const DISTILL_IDLE_MS: i64 = 120_000;

/// Hard safety valve: if the un-distilled tail of a thread grows beyond
/// this many lines, distill even if the thread isn't "idle". Prevents one
/// hot thread from accumulating an unbounded backlog.
pub const DISTILL_MAX_PENDING: i64 = 400;

/// Per-segment transcript char budget fed to the LLM. Older content
/// (head of segment) is dropped first; the truncation point is marked
/// with `[...earlier content truncated...]`.
pub const DISTILL_MAX_TRANSCRIPT_CHARS: usize = 48_000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DistillStats {
    pub threads_seen: u64,
    pub threads_triggered: u64,
    pub segments_processed: u64,
    pub points_written: u64,
    pub transient_errors: u64,
    pub client_errors: u64,
    pub parse_skips: u64,
}

#[derive(Debug, Clone)]
struct DistillCandidate {
    thread_id: String,
    project_hash: Option<String>,
    last_line_no: i64,
    last_distilled_line_no: i64,
    last_growth_ts: Option<i64>,
}

/// One distillation pass. `now_ms` is injected so tests can fake "the
/// thread has been idle for N ms" without sleeping. The pass takes
/// `&Connection` (long-lived from the caller) and `&dyn Extractor` so
/// real-LLM and fake extractors share one code path.
pub async fn distill_once(
    conn: &Connection,
    extractor: &dyn Extractor,
    now_ms: i64,
) -> Result<DistillStats, BackendError> {
    let mut stats = DistillStats::default();
    let candidates = read_distill_candidates(conn)?;
    stats.threads_seen = candidates.len() as u64;

    for cand in candidates {
        let idle = match cand.last_growth_ts {
            Some(growth) => now_ms - growth > DISTILL_IDLE_MS,
            None => false,
        };
        let pending = cand.last_line_no - cand.last_distilled_line_no;
        let fire = idle || pending > DISTILL_MAX_PENDING;
        if !fire {
            continue;
        }
        stats.threads_triggered += 1;

        let rows = read_distill_lines(
            conn,
            &cand.thread_id,
            cand.last_distilled_line_no,
            cand.last_line_no,
        )?;
        if rows.is_empty() {
            // Cursor + last_line_no disagree with raw_event reality; just
            // skip — next pass after an ingest reconciles.
            continue;
        }
        let lines_ref: Vec<(i64, &str)> = rows.iter().map(|(n, p)| (*n, p.as_str())).collect();
        let segments = segment_thread(&lines_ref);

        for segment in segments {
            stats.segments_processed += 1;

            let transcript_str = truncate_transcript(&segment.transcript);
            if transcript_str.trim().is_empty() {
                // All-Dropped segment: no LLM call, just slide the cursor
                // past so we never look at these lines again.
                advance_distill_cursor(conn, &cand.thread_id, segment.end_line_no)?;
                continue;
            }

            let result = extractor
                .extract(&transcript_str, segment.prior_summary.as_deref())
                .await;
            match result {
                Ok(kps) => {
                    let n = kps.len() as u64;
                    write_distilled_segment(conn, &cand, &kps, segment.end_line_no, now_ms)?;
                    stats.points_written += n;
                }
                Err(ExtractError::HttpTransient(msg)) => {
                    eprintln!(
                        "[distill] thread={} transient: {msg} — leaving cursor at {} for next pass",
                        cand.thread_id, cand.last_distilled_line_no
                    );
                    stats.transient_errors += 1;
                    break; // don't advance cursor; next pass retries this segment
                }
                Err(ExtractError::HttpClient(msg)) => {
                    eprintln!(
                        "[distill] thread={} client error (won't retry on same input): {msg}",
                        cand.thread_id
                    );
                    stats.client_errors += 1;
                    break; // same as transient: cursor stays put for now
                }
                Err(ExtractError::Parse(msg)) => {
                    eprintln!(
                        "[distill] thread={} parse error on segment [{}-{}], skipping past: {msg}",
                        cand.thread_id, segment.start_line_no, segment.end_line_no
                    );
                    // ADVANCE the cursor past this segment so we don't
                    // loop on garbage. The next segment in the same
                    // thread still gets processed.
                    advance_distill_cursor(conn, &cand.thread_id, segment.end_line_no)?;
                    stats.parse_skips += 1;
                    continue;
                }
                Err(other @ ExtractError::Config(_)) => {
                    // Treat like client: stop this thread for the pass.
                    eprintln!(
                        "[distill] thread={} fatal config error: {other}",
                        cand.thread_id
                    );
                    stats.client_errors += 1;
                    break;
                }
            }
        }
    }

    Ok(stats)
}

fn read_distill_candidates(conn: &Connection) -> Result<Vec<DistillCandidate>, BackendError> {
    let mut stmt = conn.prepare(
        "SELECT thread_id, project_hash, last_line_no, last_distilled_line_no, last_growth_ts \
         FROM raw_thread \
         WHERE last_distilled_line_no < last_line_no \
         ORDER BY thread_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(DistillCandidate {
            thread_id: r.get(0)?,
            project_hash: r.get(1)?,
            last_line_no: r.get(2)?,
            last_distilled_line_no: r.get(3)?,
            last_growth_ts: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn read_distill_lines(
    conn: &Connection,
    thread_id: &str,
    after_line_no: i64,
    up_to_line_no: i64,
) -> Result<Vec<(i64, String)>, BackendError> {
    let mut stmt = conn.prepare(
        "SELECT line_no, payload FROM raw_event \
         WHERE thread_id = ?1 AND line_no > ?2 AND line_no <= ?3 \
         ORDER BY line_no",
    )?;
    let rows = stmt.query_map(
        params![thread_id, after_line_no, up_to_line_no],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Keep the **tail** of the transcript when it overflows the budget —
/// the LLM cares more about recent context than ancient setup. The
/// dropped head is replaced with a single marker line so the model
/// knows it's working with a trimmed segment.
fn truncate_transcript(t: &str) -> String {
    let total = t.chars().count();
    if total <= DISTILL_MAX_TRANSCRIPT_CHARS {
        return t.to_string();
    }
    let marker = "[...earlier content truncated...]\n";
    let marker_chars = marker.chars().count();
    let budget = DISTILL_MAX_TRANSCRIPT_CHARS.saturating_sub(marker_chars);
    let skip = total - budget;
    let tail: String = t.chars().skip(skip).collect();
    let mut out = String::with_capacity(marker.len() + tail.len());
    out.push_str(marker);
    out.push_str(&tail);
    out
}

/// IMMEDIATE-tx UPDATE of `raw_thread.last_distilled_line_no` only.
/// Used for empty-transcript segments and parse-error skip-past.
fn advance_distill_cursor(
    conn: &Connection,
    thread_id: &str,
    new_line_no: i64,
) -> Result<(), BackendError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = conn.execute(
        "UPDATE raw_thread SET last_distilled_line_no = ?1 WHERE thread_id = ?2",
        params![new_line_no, thread_id],
    );
    match result {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(BackendError::Sqlite(e))
        }
    }
}

/// IMMEDIATE-tx: insert every KP as a `log` row with `origin='distill'`,
/// then advance the cursor. All-or-nothing — if any insert fails the
/// cursor stays put and the next pass retries the segment.
fn write_distilled_segment(
    conn: &Connection,
    cand: &DistillCandidate,
    kps: &[KnowledgePoint],
    end_line_no: i64,
    now_ms: i64,
) -> Result<(), BackendError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let inner = (|| -> Result<(), BackendError> {
        for kp in kps {
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin, project_hash, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    now_ms,
                    &kp.summary,
                    &kp.detail,
                    "distill",
                    &cand.project_hash,
                    &kp.kind,
                ],
            )?;
        }
        conn.execute(
            "UPDATE raw_thread SET last_distilled_line_no = ?1 WHERE thread_id = ?2",
            params![end_line_no, &cand.thread_id],
        )?;
        Ok(())
    })();
    match inner {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// How often the production distill loop wakes up to scan for candidate
/// threads. Less frequent than the ingester poll (15 s) because the
/// distiller only does useful work after `DISTILL_IDLE_MS` of silence —
/// a 2-minute idle threshold paired with a 30-second poll is plenty.
pub const DISTILL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Production distill loop. Owns one long-lived [`Connection`] (WAL +
/// busy_timeout via [`open`]) and runs forever, polling every
/// [`DISTILL_POLL_INTERVAL`]. Errors are logged and the loop continues —
/// **this function never panics out of the loop**; the only early exit
/// path is the initial `open()` failure (when there's nothing the loop
/// could do).
///
/// Lives on a dedicated OS thread with its own current_thread tokio
/// runtime (set up by [`super::main`]). That decouples the LLM call's
/// async work from the MCP server's runtime — they don't compete on the
/// same executor, and the long-running blocking SQL paths inside
/// [`distill_once`] stay off the server's hot loop.
pub async fn run_distill_loop(
    memory_db: PathBuf,
    extractor: HttpExtractor,
    embedder: Option<HttpEmbedder>,
) {
    // One connection for the whole lifetime of the loop. Stays on this
    // thread; the rusqlite::Connection isn't Sync, so we never share it.
    let conn = match open(&memory_db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[distill] open failed, loop not starting: {e}");
            return;
        }
    };
    loop {
        match distill_once(&conn, &extractor, current_time_ms()).await {
            Ok(stats) => {
                // Quiet by default; chatter only when something interesting
                // happened so a happy-path log doesn't spam every 30 s.
                if stats.points_written > 0
                    || stats.transient_errors > 0
                    || stats.client_errors > 0
                    || stats.parse_skips > 0
                {
                    eprintln!("[distill] {stats:?}");
                }
            }
            Err(e) => eprintln!("[distill] pass failed: {e}"),
        }

        // Embedding is independent of distillation: even if the
        // distiller had a bad pass, the embedder still processes its
        // own pending queue. Failures are logged + the loop continues.
        if let Some(emb) = &embedder {
            match embed_pending_once(&conn, emb).await {
                Ok(stats) => {
                    if stats.rows_embedded > 0 || stats.errors > 0 {
                        eprintln!("[embed] {stats:?}");
                    }
                }
                Err(e) => eprintln!("[embed] pass failed: {e}"),
            }
        }

        // Low-frequency consolidation (kill-switched, default OFF). Disabled →
        // not touched at all. When enabled, the step itself no-ops until enough
        // new LIVE KPs have accrued (see `run_consolidation_step`). Same conn +
        // thread; `consolidate_once` is `!Send` + needs the time driver, both of
        // which this loop's runtime has. Failures are logged; the loop continues.
        if consolidation_enabled() {
            if let Err(e) = run_consolidation_step(&conn, &extractor).await {
                eprintln!("[consolidate] step failed: {e}");
            }
        }

        tokio::time::sleep(DISTILL_POLL_INTERVAL).await;
    }
}
