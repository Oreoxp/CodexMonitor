// Phase 5 Step 3 Block B — pre-compaction memory flush.
//
// Block A senses (`token_watcher` signals when a thread nears codex's
// auto-compaction). Block B acts: it injects a "flush turn" — a system-issued
// user-role message asking the agent to write a note to its future self —
// and, when the agent answers with a `<daily_log>` tag, appends that note
// to the agent's daily memory file `project-memory/<YYYY-MM-DD>.md`.
//
// This module owns the pure + filesystem helpers: the flush prompt, the
// `<daily_log>` tag parser, the day-file format, and the append. The team
// router (`team_router.rs`) owns the per-thread orchestration — injecting the
// turn, matching the response by turn id, and emitting the audit event.
//
// **Tag rename 2026-05-23**: the original tag was `<write_memory>`. Renamed
// to `<daily_log>` after the closing `</write_memory>` was observed coming
// back from the chat surface in a linkified form
// (`<[write_memory](codex-file:%2Fwrite_memory)>` — the markdown-render
// pipeline treats `/write_memory` as a file path). The rename alone does
// not fix that: `</daily_log>` has the same `/` and would be linkified the
// same way. The rename is cosmetic — the real protections are the robust
// parser below (which accepts both the clean and the linkified close form
// as defence-in-depth) and the visible-level log in `team_router::
// handle_flush_response` that prints a truncated raw response when no
// usable tag is found. (Static investigation as of 2026-05-23 indicates the
// linkifier is frontend-only and the parser's input is in fact clean — the
// defence is for a future upstream change we have not audited.)

use std::path::Path;

use serde_json::Value;

/// The `<daily_log>` tag the agent wraps its flush note in. `<verb_noun>` to
/// match the family (`send_message` / `propose_plan` / `daily_log`).
const DAILY_LOG_OPEN: &str = "<daily_log>";
const DAILY_LOG_CLOSE: &str = "</daily_log>";
/// Defence-in-depth: the start of the linkified `</daily_log>` form a
/// markdown-render pipeline produces when it treats `/daily_log` as a file
/// path. The full shape is `<[daily_log](codex-file:%2Fdaily_log)>` — we
/// match on the prefix `<[daily_log` then scan to the first `>` to recover
/// the close. See the module header for why this is defence-in-depth rather
/// than the load-bearing path.
const DAILY_LOG_CLOSE_LINKIFIED_PREFIX: &str = "<[daily_log";

/// The flush prompt — the user-role message injected as the flush turn. The
/// `[System memory-flush]` opener marks it unmistakably as a system
/// housekeeping message, so it does not impersonate a user / conversation
/// message.
pub(crate) fn flush_prompt() -> String {
    [
        "[System memory-flush]",
        "Your conversation context is about to be compacted — details from \
         this session will be summarized away and may be lost. Before that \
         happens, append a log entry for your future self who will pick this \
         project up later.",
        "Put the entry inside a single <daily_log>...</daily_log> tag. \
         Cover, briefly: the key decisions made, where you have got to, any \
         pitfalls or gotchas to avoid, and the next concrete step.",
        "Emit only the <daily_log> tag. Do not call tools.",
    ]
    .join("\n")
}

/// Scan `text` for every `<daily_log>...</daily_log>` block and return the
/// trimmed, non-empty contents in document order. Unterminated or empty tags
/// are skipped. A correctly-answered flush produces exactly one; returning a
/// `Vec` keeps a stray second tag from being silently dropped.
///
/// Robust to the linkified-close form: if `</daily_log>` is not present but
/// `<[daily_log...>` is (see `DAILY_LOG_CLOSE_LINKIFIED_PREFIX` for the
/// shape), the parser accepts that as the close and resumes scanning past
/// the first `>` after it. Whichever close form appears first wins.
pub(crate) fn parse_daily_log_tags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open_at) = rest.find(DAILY_LOG_OPEN) {
        let after_open = &rest[open_at + DAILY_LOG_OPEN.len()..];
        let Some((content_end, close_consumed)) = find_daily_log_close(after_open) else {
            // No usable close — stop scanning. The whole-buffer failure log
            // happens in `team_router::handle_flush_response` (it has the
            // thread + agent id context this module deliberately doesn't).
            break;
        };
        let content = after_open[..content_end].trim();
        if !content.is_empty() {
            out.push(content.to_string());
        }
        rest = &after_open[close_consumed..];
    }
    out
}

/// Locate the close marker following an open `<daily_log>`. Returns
/// `(content_end, close_consumed)` measured from the start of `after_open`:
/// `content_end` is where the agent's note ends (exclusive); `close_consumed`
/// is where scanning should resume for the next `<daily_log>` (past the
/// close marker itself).
///
/// Recognises two forms — clean `</daily_log>` and linkified
/// `<[daily_log...>` — and picks whichever appears first so a stray
/// linkified mention later in the buffer doesn't overshoot a real clean
/// close earlier on.
fn find_daily_log_close(after_open: &str) -> Option<(usize, usize)> {
    let clean = after_open.find(DAILY_LOG_CLOSE);
    let linkified = after_open.find(DAILY_LOG_CLOSE_LINKIFIED_PREFIX);
    match (clean, linkified) {
        (None, None) => None,
        (Some(c), None) => Some((c, c + DAILY_LOG_CLOSE.len())),
        (None, Some(l)) => {
            // Linkified shape: `<[daily_log...]...>`. Scan to the first `>`
            // after the prefix. If no `>` (truncated stream), give up.
            let bracket = after_open[l..].find('>')?;
            Some((l, l + bracket + 1))
        }
        (Some(c), Some(l)) => {
            if c <= l {
                Some((c, c + DAILY_LOG_CLOSE.len()))
            } else {
                let bracket = after_open[l..].find('>')?;
                Some((l, l + bracket + 1))
            }
        }
    }
}

/// Extract the flush note from a flush-turn response: every `<daily_log>`
/// block joined with a blank line. Returns `None` when the response carried
/// no usable tag (missing or malformed) — the caller treats `None` as a soft
/// failure: persist nothing, log, carry on.
pub(crate) fn flush_content(response_text: &str) -> Option<String> {
    let blocks = parse_daily_log_tags(response_text);
    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n"))
    }
}

/// Render one day-file block: a lightweight local-time marker line followed
/// by the agent's verbatim note. Blocks are joined into the file with a blank
/// line between them (see `append_flush_block`), so each flush is one
/// blank-line-delimited block — compatible with Step 1's whole-file injection
/// and Step 2's paragraph-granular FTS5 index.
pub(crate) fn render_flush_block(local_time: &str, content: &str) -> String {
    format!("[flush {local_time}]\n{}", content.trim())
}

/// Append `block` to `memory_file`, blank-line-separated from any existing
/// content. Creates the file (and parent dirs) if absent.
fn append_flush_block(memory_file: &Path, block: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = memory_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let had_content = std::fs::read_to_string(memory_file)
        .map(|existing| !existing.trim().is_empty())
        .unwrap_or(false);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(memory_file)?;
    if had_content {
        // A blank line separates this block from the previous one.
        write!(file, "\n{block}\n")?;
    } else {
        write!(file, "{block}\n")?;
    }
    Ok(())
}

/// Persist a flush note for `agent_id` into
/// `<workspace>/.opencrab/agents/<agent_id>/project-memory/<date>.md`,
/// stamped with `local_time`. `date` / `local_time` are explicit so this is
/// deterministic and unit-testable; [`persist_flush`] is the production entry
/// that fills them from the local clock. Returns the character count of the
/// note that was written.
pub(crate) fn persist_flush_at(
    workspace_path: &Path,
    agent_id: &str,
    date: &str,
    local_time: &str,
    content: &str,
) -> std::io::Result<usize> {
    let project_root = crate::paths::project_root(workspace_path);
    let memory_file = crate::paths::project_agent_memory_file(&project_root, agent_id, date);
    let block = render_flush_block(local_time, content);
    append_flush_block(&memory_file, &block)?;
    Ok(content.chars().count())
}

/// Production entry: persist a flush note, dating it by the user machine's
/// local calendar day + time — the Phase 5 timezone decision ("the user's
/// day"; carried over from Step 1).
pub(crate) fn persist_flush(
    workspace_path: &Path,
    agent_id: &str,
    content: &str,
) -> std::io::Result<usize> {
    let now = chrono::Local::now();
    persist_flush_at(
        workspace_path,
        agent_id,
        &now.format("%Y-%m-%d").to_string(),
        &now.format("%H:%M:%S").to_string(),
        content,
    )
}

/// Pull the turn id out of a `turn/start` response (`{ turn: { id } }`,
/// possibly `result`-wrapped) so the flush turn's eventual `turn/completed`
/// can be matched precisely.
pub(crate) fn extract_turn_id(response: &Value) -> Option<String> {
    fn walk<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
        let mut current = value;
        for segment in path {
            current = current.get(segment)?;
        }
        current.as_str()
    }
    walk(response, &["turn", "id"])
        .or_else(|| walk(response, &["result", "turn", "id"]))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_prompt_is_a_system_framed_daily_log_instruction() {
        let prompt = flush_prompt();
        // System-framed so it does not impersonate a user / conversation turn.
        assert!(prompt.starts_with("[System memory-flush]"));
        // Tells the agent the why, the tag, and to stay focused.
        assert!(prompt.contains("compacted"));
        assert!(prompt.contains("<daily_log>"));
        assert!(prompt.contains("</daily_log>"));
        assert!(prompt.contains("Do not call tools"));
        // Cosmetic regression guard: the old tag name should be fully retired.
        assert!(!prompt.contains("write_memory"));
    }

    #[test]
    fn parse_daily_log_extracts_a_single_tag() {
        let text = "Noting this down.\n\
                    <daily_log>Chose Postgres. Next: draft the schema.</daily_log>\nDone.";
        assert_eq!(
            parse_daily_log_tags(text),
            vec!["Chose Postgres. Next: draft the schema.".to_string()],
        );
    }

    #[test]
    fn parse_daily_log_extracts_multiple_tags_in_order() {
        let text = "<daily_log>first</daily_log> then <daily_log>second</daily_log>";
        assert_eq!(
            parse_daily_log_tags(text),
            vec!["first".to_string(), "second".to_string()],
        );
    }

    #[test]
    fn parse_daily_log_skips_malformed_empty_and_missing() {
        // Empty tag.
        assert!(parse_daily_log_tags("<daily_log>   </daily_log>").is_empty());
        // Unterminated tag.
        assert!(parse_daily_log_tags("<daily_log>never closed").is_empty());
        // No tag at all.
        assert!(parse_daily_log_tags("the agent just chatted, no tag").is_empty());
    }

    #[test]
    fn parse_daily_log_recovers_from_linkified_close_form() {
        // Defence-in-depth: simulates the codex-file linkified `</daily_log>`
        // shape — `</daily_log>` → `<[daily_log](codex-file:%2Fdaily_log)>` —
        // that a markdown-render pipeline produces when it treats
        // `/daily_log` as a file path. As of 2026-05-23 we do NOT believe
        // this reaches the Rust parser (the linkifier is frontend-only) but
        // we accept it so an upstream change cannot silently drop a flush.
        let text = "leaving a note. \
                    <daily_log>Picked SQLite; next: write FTS5 \
                    tests.<[daily_log](codex-file:%2Fdaily_log)>";
        assert_eq!(
            parse_daily_log_tags(text),
            vec!["Picked SQLite; next: write FTS5 tests.".to_string()],
        );
    }

    #[test]
    fn parse_daily_log_handles_real_world_agent_response() {
        // The exact text a PM agent emitted in production (2026-05-23 user
        // smoke). Mixed-CJK + emoji + multi-section structure. Pinned here
        // because the user's report ("目录存在但空") had this as the proven-
        // clean flush response — if the parser ever silently regresses on a
        // shape like this, the dir-without-file failure mode recurs.
        let text = "<daily_log>\n\
2026-05-23 | 记事本功能开发 - Task 1 完成\n\
【关键决策】\n\
- 表名采用 sys_note 符合 RuoYi 命名规范，主键 note_id 自增\n\
- 字段设计参考 sys_user：title(varchar100), content(text), user_id(bigint), 标准审计字段+del_flag\n\
- 索引：idx_user_id + idx_create_time\n\
【当前进度】\n\
✅ Task 1: backend/sql/sys_note.sql 已创建\n\
⏳ Task 2: 待生成后端 CRUD\n\
【注意事项】\n\
- Entity 需继承 BaseEntity；Controller 路径 /system/note/*\n\
- 前端 Vue2+ElementUI\n\
【下一步】\n\
生成 SysNote Entity 类\n\
</daily_log>";
        let blocks = parse_daily_log_tags(text);
        assert_eq!(blocks.len(), 1, "expected exactly one extracted block");
        let body = &blocks[0];
        // Spot-checks: structure intact, CJK preserved, no leading/trailing
        // whitespace pollution.
        assert!(body.starts_with("2026-05-23 | 记事本功能开发"));
        assert!(body.ends_with("生成 SysNote Entity 类"));
        assert!(body.contains("【关键决策】"));
        assert!(body.contains("✅ Task 1"));
        // `flush_content` is the production entry — confirm it agrees.
        let joined = flush_content(text).expect("flush_content should extract the block");
        assert_eq!(joined, *body);
    }

    #[test]
    fn parse_daily_log_prefers_earlier_close_form() {
        // Clean close appears before a stray linkified mention later in the
        // buffer — the clean close must win so the linkified text doesn't
        // overshoot into the next paragraph.
        let text = "<daily_log>note A</daily_log> then later prose mentioning \
                    <[daily_log](codex-file:%2Fdaily_log)>";
        assert_eq!(parse_daily_log_tags(text), vec!["note A".to_string()]);
    }

    #[test]
    fn flush_content_joins_tags_or_returns_none() {
        assert_eq!(
            flush_content("<daily_log>a</daily_log><daily_log>b</daily_log>"),
            Some("a\n\nb".to_string()),
        );
        // Missing tag → soft failure signalled as None.
        assert_eq!(flush_content("the agent said something but emitted no tag"), None);
        // Malformed (empty) tag → None.
        assert_eq!(flush_content("<daily_log></daily_log>"), None);
    }

    #[test]
    fn render_flush_block_marks_time_then_trims_content() {
        assert_eq!(
            render_flush_block("09:30:00", "  did things  "),
            "[flush 09:30:00]\ndid things",
        );
    }

    #[test]
    fn persist_flush_writes_a_dated_block() {
        let tmp = tempfile::tempdir().unwrap();
        let chars =
            persist_flush_at(tmp.path(), "alice", "2026-05-23", "09:00:00", "first note").unwrap();
        assert_eq!(chars, "first note".chars().count());
        let file = crate::paths::project_agent_memory_file(
            &crate::paths::project_root(tmp.path()),
            "alice",
            "2026-05-23",
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[flush 09:00:00]\nfirst note\n",
        );
    }

    #[test]
    fn persist_flush_appends_same_day_flushes_as_blank_line_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        persist_flush_at(tmp.path(), "alice", "2026-05-23", "09:00:00", "morning note").unwrap();
        persist_flush_at(tmp.path(), "alice", "2026-05-23", "14:30:00", "afternoon note").unwrap();
        let file = crate::paths::project_agent_memory_file(
            &crate::paths::project_root(tmp.path()),
            "alice",
            "2026-05-23",
        );
        // Two flushes, one file, blank-line-separated blocks.
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[flush 09:00:00]\nmorning note\n\n[flush 14:30:00]\nafternoon note\n",
        );
    }

    #[test]
    fn persist_flush_separates_distinct_days_into_distinct_files() {
        let tmp = tempfile::tempdir().unwrap();
        persist_flush_at(tmp.path(), "alice", "2026-05-22", "23:59:00", "day one").unwrap();
        persist_flush_at(tmp.path(), "alice", "2026-05-23", "00:01:00", "day two").unwrap();
        let root = crate::paths::project_root(tmp.path());
        assert!(crate::paths::project_agent_memory_file(&root, "alice", "2026-05-22").exists());
        assert!(crate::paths::project_agent_memory_file(&root, "alice", "2026-05-23").exists());
    }

    #[test]
    fn persist_flush_production_entry_lands_one_dated_file() {
        // The real-clock entry point — confirm it writes one dated file under
        // the agent's project-memory dir without panicking.
        let tmp = tempfile::tempdir().unwrap();
        persist_flush(tmp.path(), "alice", "real-clock note").unwrap();
        let dir = crate::paths::project_agent_memory_dir(
            &crate::paths::project_root(tmp.path()),
            "alice",
        );
        let count = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).count();
        assert_eq!(count, 1, "exactly one dated file written");
    }

    #[test]
    fn extract_turn_id_reads_plain_and_result_wrapped_responses() {
        use serde_json::json;
        assert_eq!(
            extract_turn_id(&json!({ "turn": { "id": "turn-1" } })),
            Some("turn-1".to_string()),
        );
        assert_eq!(
            extract_turn_id(&json!({ "result": { "turn": { "id": "turn-2" } } })),
            Some("turn-2".to_string()),
        );
        assert_eq!(extract_turn_id(&json!({ "no": "turn here" })), None);
    }

    #[test]
    fn memory_flush_event_serializes_as_a_housekeeping_event_type() {
        // The MemoryFlush event is the durable, auditable housekeeping mark.
        // `flush_turn_id` lets a later phase (UI re-build) correlate it back
        // to the flush turn-pair in the chat event stream.
        let body = crate::events::TeamEventBody::MemoryFlush {
            agent_id: "alice".to_string(),
            flush_turn_id: "turn-abc-123".to_string(),
            chars_written: 128,
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["eventType"], "memory_flush");
        assert_eq!(value["agent_id"], "alice");
        assert_eq!(value["flush_turn_id"], "turn-abc-123");
        assert_eq!(value["chars_written"], 128);
    }
}
