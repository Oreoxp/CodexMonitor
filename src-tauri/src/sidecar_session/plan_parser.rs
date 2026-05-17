// Phase 3 Step 2 — `<propose_plan>` text-tag parser.
//
// Locked grammar:
//
//   <propose_plan>
//     <task title="..." assignee="agent_id">body content</task>
//     <task title="...">body content</task>
//     ...
//   </propose_plan>
//
// Rules:
//   - `title` is required.
//   - `assignee` is optional (omit attribute → None; Step 4 modal collects it).
//   - body is the `<task>` element's inner text, trimmed.
//   - Empty plans (`<propose_plan></propose_plan>`) parse to `Ok(vec![])` —
//     the caller decides whether to treat that as a no-op or a malformed
//     turn (parser does not editorialize).
//   - Anything that *looks* like a plan but does not parse cleanly returns
//     `Err`. Per spec, the router drops the whole block on Err and tells
//     the emitting agent to retry — there is no partial-commit path.
//   - Nested `<propose_plan>` is rejected: any inner `<propose_plan>` token
//     before the outer close means the grammar is broken, fail loud.
//
// Why text-tag, not Codex tool: see §4 of the Step 2 report. Short
// version: tag inherits the entire Phase 2 send_message plumbing (the
// existing tap + final-text scan in `team_router.rs`), keeps us decoupled
// from Codex's tool-spec evolution, and gives identical UX to send_message
// for the agent author.
//
// Why hand-rolled scanner: matches the precedent set by
// `parse_send_message_tags` — `regex` is not in Cargo.toml; the grammar
// has no recursion to handle (nested plans are explicitly rejected); the
// only attribute extractor we need is already proven on send_message.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPlanTask {
    pub(crate) title: String,
    pub(crate) assignee: Option<String>,
    pub(crate) body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPlan {
    pub(crate) tasks: Vec<ParsedPlanTask>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanParseError {
    /// `<propose_plan>` opened but never closed before EOF.
    UnclosedPlan,
    /// A second `<propose_plan>` opens before the first closes — we don't
    /// support nested plans (per spec).
    NestedPlan,
    /// `<task>` opened but missing its closing `</task>` before the outer
    /// `</propose_plan>` or EOF.
    UnclosedTask,
    /// A `<task>` element has no `title="..."` attribute.
    MissingTitle,
    /// Attribute value not double-quoted (we don't accept single quotes —
    /// same rule as `<send_message>`).
    BadAttributeQuoting,
}

impl fmt::Display for PlanParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanParseError::UnclosedPlan => f.write_str("unclosed <propose_plan>"),
            PlanParseError::NestedPlan => f.write_str("nested <propose_plan> is not supported"),
            PlanParseError::UnclosedTask => f.write_str("unclosed <task>"),
            PlanParseError::MissingTitle => {
                f.write_str("<task> is missing required `title` attribute")
            }
            PlanParseError::BadAttributeQuoting => {
                f.write_str("attribute values must be double-quoted")
            }
        }
    }
}

impl std::error::Error for PlanParseError {}

const OPEN_PLAN: &str = "<propose_plan>";
const CLOSE_PLAN: &str = "</propose_plan>";
const OPEN_TASK_PREFIX: &str = "<task";
const CLOSE_TASK: &str = "</task>";

/// Find every `<propose_plan>...</propose_plan>` block in `text` and parse
/// each into a `ParsedPlan`. Multiple plans in one turn parse to multiple
/// `Ok` entries (in document order). If any individual block fails to
/// parse, that block alone is returned as `Err` — sibling well-formed
/// blocks are still surfaced.
///
/// Mixed turn rule (per Step 2 spec): this parser is independent of the
/// `send_message` parser. The router runs BOTH parsers over the same final
/// text; Step 3 owns the question of which to act on first when a turn
/// emits both. Step 2 does not predict ordering.
pub(crate) fn parse_propose_plan_blocks(text: &str) -> Vec<Result<ParsedPlan, PlanParseError>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open_at) = rest.find(OPEN_PLAN) {
        let body_start = &rest[open_at + OPEN_PLAN.len()..];
        // Reject nested: if another `<propose_plan>` appears before the
        // close, the whole block is malformed.
        let inner_close = body_start.find(CLOSE_PLAN);
        let inner_open = body_start.find(OPEN_PLAN);
        match (inner_close, inner_open) {
            (None, _) => {
                out.push(Err(PlanParseError::UnclosedPlan));
                return out;
            }
            (Some(close_idx), Some(open_idx)) if open_idx < close_idx => {
                out.push(Err(PlanParseError::NestedPlan));
                // Advance past this open token; don't consume nested open
                // again (it will be picked up as its own malformed plan).
                rest = &body_start[open_idx..];
                continue;
            }
            _ => {}
        }
        let close_at = inner_close.unwrap();
        let inner = &body_start[..close_at];
        out.push(parse_inner(inner));
        rest = &body_start[close_at + CLOSE_PLAN.len()..];
    }
    out
}

fn parse_inner(inner: &str) -> Result<ParsedPlan, PlanParseError> {
    let mut tasks: Vec<ParsedPlanTask> = Vec::new();
    let mut rest = inner;
    while let Some(open_at) = rest.find(OPEN_TASK_PREFIX) {
        // Must be followed by either whitespace or `>` to count as our tag
        // (so e.g. `<task_foo>` doesn't match).
        let after_prefix = &rest[open_at + OPEN_TASK_PREFIX.len()..];
        let first = after_prefix.chars().next();
        if !matches!(first, Some(c) if c.is_ascii_whitespace() || c == '>') {
            // False positive; advance past and keep scanning.
            rest = after_prefix;
            continue;
        }

        let Some(attr_end_rel) = after_prefix.find('>') else {
            return Err(PlanParseError::UnclosedTask);
        };
        let attrs_str = &after_prefix[..attr_end_rel];
        let body_and_rest = &after_prefix[attr_end_rel + 1..];

        let Some(close_rel) = body_and_rest.find(CLOSE_TASK) else {
            return Err(PlanParseError::UnclosedTask);
        };
        let body = &body_and_rest[..close_rel];
        let after_close = &body_and_rest[close_rel + CLOSE_TASK.len()..];

        let title = match extract_attr(attrs_str, "title")? {
            Some(v) => v,
            None => return Err(PlanParseError::MissingTitle),
        };
        let assignee = extract_attr(attrs_str, "assignee")?;
        tasks.push(ParsedPlanTask {
            title,
            assignee,
            body: body.trim().to_string(),
        });
        rest = after_close;
    }
    Ok(ParsedPlan { tasks })
}

/// `Some(value)` if the attribute is present and well-quoted.
/// `None` if absent. `Err(BadAttributeQuoting)` if present but malformed
/// (no `=`, single-quoted, unterminated quote).
fn extract_attr(attrs: &str, name: &str) -> Result<Option<String>, PlanParseError> {
    let mut search_from = 0;
    while let Some(rel) = attrs[search_from..].find(name) {
        let abs = search_from + rel;
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
        let Some(after_eq) = trimmed.strip_prefix('=') else {
            search_from = abs + name.len();
            continue;
        };
        let after_eq = after_eq.trim_start();
        // We accept only double-quoted values (same rule as send_message).
        let Some(after_quote) = after_eq.strip_prefix('"') else {
            return Err(PlanParseError::BadAttributeQuoting);
        };
        let Some(close_rel) = after_quote.find('"') else {
            return Err(PlanParseError::BadAttributeQuoting);
        };
        return Ok(Some(after_quote[..close_rel].to_string()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_task_plan() {
        let text = r#"prelude <propose_plan>
  <task title="set up scaffolding" assignee="dev-bob">create the repo skeleton</task>
</propose_plan> trailing"#;
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        let plan = blocks[0].as_ref().unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].title, "set up scaffolding");
        assert_eq!(plan.tasks[0].assignee.as_deref(), Some("dev-bob"));
        assert_eq!(plan.tasks[0].body, "create the repo skeleton");
    }

    #[test]
    fn parses_multi_task_plan_in_order() {
        let text = r#"<propose_plan>
  <task title="A" assignee="dev-bob">first</task>
  <task title="B" assignee="dev-carol">second</task>
  <task title="C">third (no assignee)</task>
</propose_plan>"#;
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        let plan = blocks[0].as_ref().unwrap();
        assert_eq!(plan.tasks.len(), 3);
        let titles: Vec<_> = plan.tasks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["A", "B", "C"]);
        assert!(plan.tasks[2].assignee.is_none());
    }

    #[test]
    fn empty_plan_returns_ok_with_no_tasks() {
        let text = "<propose_plan></propose_plan>";
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].as_ref().unwrap().tasks.len(), 0);
    }

    #[test]
    fn missing_title_attribute_returns_err() {
        let text = r#"<propose_plan><task assignee="dev-bob">body</task></propose_plan>"#;
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], Err(PlanParseError::MissingTitle));
    }

    #[test]
    fn unclosed_task_returns_err() {
        let text = r#"<propose_plan><task title="A">never closes</propose_plan>"#;
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], Err(PlanParseError::UnclosedTask));
    }

    #[test]
    fn unclosed_plan_returns_err() {
        let text = r#"<propose_plan><task title="A">body</task>"#;
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], Err(PlanParseError::UnclosedPlan));
    }

    #[test]
    fn nested_plan_returns_err() {
        let text = r#"<propose_plan>
            <task title="A">body</task>
            <propose_plan><task title="B">inner</task></propose_plan>
        </propose_plan>"#;
        let blocks = parse_propose_plan_blocks(text);
        // Outer block surfaces as NestedPlan; the nested block is then
        // re-scanned as its own outer-level plan (with its own close), so
        // the test expects two entries: the error and the (recovered)
        // inner plan.
        assert!(!blocks.is_empty());
        assert_eq!(blocks[0], Err(PlanParseError::NestedPlan));
    }

    #[test]
    fn single_quoted_attribute_is_bad_quoting() {
        let text = r#"<propose_plan><task title='A'>body</task></propose_plan>"#;
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], Err(PlanParseError::BadAttributeQuoting));
    }

    #[test]
    fn special_characters_in_body_are_preserved() {
        let text = r#"<propose_plan>
  <task title="utf-8 & escapes" assignee="dev-bob">
中文 + emoji 🦀 + symbols < > & "quotes"
  </task>
</propose_plan>"#;
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        let plan = blocks[0].as_ref().unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert!(plan.tasks[0].body.contains("🦀"));
        assert!(plan.tasks[0].body.contains("中文"));
        // The body is trimmed for outer whitespace but inner content
        // (including angle-brackets used as literal text) is preserved.
        assert!(plan.tasks[0].body.contains("symbols < > &"));
    }

    #[test]
    fn assignee_attribute_order_does_not_matter() {
        let text = r#"<propose_plan>
  <task assignee="dev-bob" title="reversed order">body</task>
</propose_plan>"#;
        let blocks = parse_propose_plan_blocks(text);
        let plan = blocks[0].as_ref().unwrap();
        assert_eq!(plan.tasks[0].title, "reversed order");
        assert_eq!(plan.tasks[0].assignee.as_deref(), Some("dev-bob"));
    }

    #[test]
    fn plan_with_zero_tasks_is_ok() {
        let text = "<propose_plan>   \n   \n</propose_plan>";
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks[0].as_ref().unwrap().tasks.len(), 0);
    }

    #[test]
    fn false_positive_task_prefix_is_skipped() {
        // `<task_foo>` and `<tasks>` must not match our scanner.
        let text = r#"<propose_plan>
            <task_foo title="not us">ignored</task_foo>
            <tasks title="not us either">also ignored</tasks>
            <task title="the real one">body</task>
        </propose_plan>"#;
        let blocks = parse_propose_plan_blocks(text);
        let plan = blocks[0].as_ref().unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].title, "the real one");
    }

    #[test]
    fn no_plan_tag_returns_empty_vec() {
        let text = "just normal chat, no tags at all";
        let blocks = parse_propose_plan_blocks(text);
        assert!(blocks.is_empty());
    }

    #[test]
    fn kickoff_prompt_does_not_match() {
        // The kickoff prompt PM agents receive at thread provisioning is
        // "[System bootstrap] Acknowledge that you are <Name> and you are
        // ready..." (sidecar/src/runtime/state.ts:composeKickoffPrompt).
        // It must NEVER cause our scanner to fire. This guard test exists
        // so anybody adjusting the kickoff message keeps that property.
        let text = "[System bootstrap]\nAcknowledge that you are Alice and you are ready, \
                    in one short sentence (≤15 words).\nDo NOT use any <send_message> tags \
                    in this acknowledgment.";
        let blocks = parse_propose_plan_blocks(text);
        assert!(blocks.is_empty());
    }

    #[test]
    fn coexists_with_send_message_text_in_same_buffer() {
        // The router runs both parsers over the same final-text buffer.
        // The plan parser must not be confused by a `<send_message>` tag
        // (no false match) and must not consume text outside its own
        // block.
        let text = r#"
<send_message to="user" channel="chat">Here's my plan, please review.</send_message>
<propose_plan>
  <task title="step 1" assignee="dev-bob">do thing one</task>
</propose_plan>
"#;
        let blocks = parse_propose_plan_blocks(text);
        assert_eq!(blocks.len(), 1);
        let plan = blocks[0].as_ref().unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].title, "step 1");
    }
}
