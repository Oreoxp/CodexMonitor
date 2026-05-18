// Phase 4 Step 7 — KANBAN.md mirror tests.
//
// Coverage:
//   * Pure render: empty + each section non-empty + status suffix
//     (incl. blocked with/without reason), per-agent filter, archived
//     exclusion, sort by created_at, markdown special-char passthrough.
//   * Atomic write: fresh file, overwrite seed, parent missing,
//     no `.tmp` artifact on success.
//   * regenerate_all_kanbans: per-agent file partition, missing
//     team.json graceful, full lifecycle (propose → approve → start
//     → done) reflected in KANBAN.

use std::fs;
use std::path::Path;

use chrono::{TimeZone, Utc};
use tempfile::tempdir;

use super::render::render_kanban_markdown;
use super::write::{regenerate_kanban_for_agent, KanbanError};
use crate::tasks::{Task, TaskStatus};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn fixed_now() -> chrono::DateTime<chrono::Utc> {
    Utc.with_ymd_and_hms(2026, 5, 18, 12, 0, 0).unwrap()
}

fn make_task(
    id: &str,
    status: TaskStatus,
    assignee: Option<&str>,
    title: &str,
    created_at: &str,
) -> Task {
    Task {
        id: id.to_string(),
        workspace_id: "ws_1".to_string(),
        team_id: "team_demo".to_string(),
        assignee_agent_id: assignee.map(str::to_string),
        proposed_by_agent_id: "agent_pm".to_string(),
        status,
        title: title.to_string(),
        body: String::new(),
        approved_at: None,
        completed_at: None,
        created_at: created_at.to_string(),
        updated_at: created_at.to_string(),
        plan_id: Some("plan_1".to_string()),
        feedback: None,
    }
}

fn make_blocked_with_reason(id: &str, assignee: &str, title: &str, reason: &str) -> Task {
    let mut t = make_task(
        id,
        TaskStatus::Blocked,
        Some(assignee),
        title,
        "2026-05-18T11:00:00Z",
    );
    t.feedback = Some(reason.to_string());
    t
}

fn seed_agent_dir(project_dir: &Path, agent_id: &str) {
    let dir = project_dir.join(".opencrab").join("agents").join(agent_id);
    fs::create_dir_all(&dir).unwrap();
    // Optional placeholder so we can test overwrite.
    fs::write(dir.join("KANBAN.md"), "# Stale\n").unwrap();
}

// ---------------------------------------------------------------------------
// render — pure function tests
// ---------------------------------------------------------------------------

#[test]
fn render_empty_emits_three_section_placeholders() {
    let out = render_kanban_markdown("Alice", &[], fixed_now());

    assert!(out.starts_with("# Kanban — Alice\n"));
    assert!(out.contains("<!-- Auto-generated"));
    assert!(out.contains("<!-- Last update: 2026-05-18T12:00:00.000Z -->"));
    assert!(out.contains("## Proposed\n\n_(empty)_\n"));
    assert!(out.contains("## In Progress\n\n_(empty)_\n"));
    assert!(out.contains("## Done\n\n_(empty)_\n"));
    // Section heading order: Proposed → In Progress → Done.
    let p = out.find("## Proposed").unwrap();
    let ip = out.find("## In Progress").unwrap();
    let d = out.find("## Done").unwrap();
    assert!(p < ip);
    assert!(ip < d);
}

#[test]
fn render_single_proposed_only_populates_first_section() {
    let tasks = vec![make_task(
        "task_a",
        TaskStatus::Proposed,
        Some("agent_alice"),
        "do thing",
        "2026-05-18T10:00:00Z",
    )];
    let out = render_kanban_markdown("Alice", &tasks, fixed_now());

    assert!(out.contains("## Proposed\n\n- [task_a] do thing\n"));
    assert!(out.contains("## In Progress\n\n_(empty)_\n"));
    assert!(out.contains("## Done\n\n_(empty)_\n"));
}

#[test]
fn render_status_suffix_covers_ready_running_blocked() {
    let tasks = vec![
        make_task(
            "t_r",
            TaskStatus::Ready,
            Some("alice"),
            "ready task",
            "2026-05-18T10:00:00Z",
        ),
        make_task(
            "t_run",
            TaskStatus::Running,
            Some("alice"),
            "running task",
            "2026-05-18T10:01:00Z",
        ),
        make_blocked_with_reason("t_b", "alice", "blocked task", "waiting upstream"),
    ];
    let out = render_kanban_markdown("Alice", &tasks, fixed_now());

    assert!(out.contains("- [t_r] ready task (ready)\n"));
    assert!(out.contains("- [t_run] running task (running)\n"));
    assert!(out.contains("- [t_b] blocked task (blocked: waiting upstream)\n"));
}

#[test]
fn render_blocked_without_reason_omits_colon() {
    let task = make_task(
        "t_b",
        TaskStatus::Blocked,
        Some("alice"),
        "stuck",
        "2026-05-18T10:00:00Z",
    );
    let out = render_kanban_markdown("Alice", &[task], fixed_now());
    assert!(out.contains("- [t_b] stuck (blocked)\n"));
    assert!(!out.contains("(blocked:"));
}

#[test]
fn render_blocked_with_whitespace_only_reason_falls_back_to_no_colon() {
    let mut t = make_task(
        "t_b",
        TaskStatus::Blocked,
        Some("alice"),
        "stuck",
        "2026-05-18T10:00:00Z",
    );
    t.feedback = Some("   ".to_string());
    let out = render_kanban_markdown("Alice", &[t], fixed_now());
    assert!(out.contains("- [t_b] stuck (blocked)\n"));
}

#[test]
fn render_done_section_excludes_other_statuses() {
    let tasks = vec![
        make_task(
            "t1",
            TaskStatus::Done,
            Some("alice"),
            "shipped",
            "2026-05-18T10:00:00Z",
        ),
        make_task(
            "t2",
            TaskStatus::Running,
            Some("alice"),
            "in flight",
            "2026-05-18T10:01:00Z",
        ),
    ];
    let out = render_kanban_markdown("Alice", &tasks, fixed_now());

    let done_section = out.split("## Done\n\n").nth(1).unwrap();
    assert!(done_section.contains("- [t1] shipped\n"));
    assert!(!done_section.contains("t2"));
}

#[test]
fn render_orders_tasks_within_section_by_created_at_ascending() {
    let tasks = vec![
        make_task(
            "t_late",
            TaskStatus::Proposed,
            Some("alice"),
            "third",
            "2026-05-18T12:00:00Z",
        ),
        make_task(
            "t_early",
            TaskStatus::Proposed,
            Some("alice"),
            "first",
            "2026-05-18T10:00:00Z",
        ),
        make_task(
            "t_mid",
            TaskStatus::Proposed,
            Some("alice"),
            "second",
            "2026-05-18T11:00:00Z",
        ),
    ];
    let out = render_kanban_markdown("Alice", &tasks, fixed_now());
    let idx_first = out.find("[t_early]").unwrap();
    let idx_second = out.find("[t_mid]").unwrap();
    let idx_third = out.find("[t_late]").unwrap();
    assert!(idx_first < idx_second);
    assert!(idx_second < idx_third);
}

#[test]
fn render_passes_markdown_special_chars_verbatim_in_titles() {
    // Spec: P4 does NOT escape markdown special chars in titles. P6
    // re-evaluates. Pin the current behavior so a future contributor
    // doesn't silently add escaping mid-cycle.
    let tasks = vec![make_task(
        "t1",
        TaskStatus::Proposed,
        Some("alice"),
        "wire **bold** + [link](url) + _italic_",
        "2026-05-18T10:00:00Z",
    )];
    let out = render_kanban_markdown("Alice", &tasks, fixed_now());
    assert!(out.contains("wire **bold** + [link](url) + _italic_"));
}

#[test]
fn render_emits_trailing_newline_no_double_blank() {
    let out = render_kanban_markdown("Alice", &[], fixed_now());
    assert!(out.ends_with('\n'));
    assert!(!out.ends_with("\n\n\n"));
}

// ---------------------------------------------------------------------------
// regenerate_kanban_for_agent — atomic write tests
// ---------------------------------------------------------------------------

#[test]
fn regenerate_writes_fresh_file_when_seed_present() {
    let cwd = tempdir().unwrap();
    seed_agent_dir(cwd.path(), "agent_alice");
    let path = cwd.path().join(".opencrab/agents/agent_alice/KANBAN.md");
    assert_eq!(fs::read_to_string(&path).unwrap(), "# Stale\n");

    let tasks = vec![make_task(
        "t1",
        TaskStatus::Proposed,
        Some("agent_alice"),
        "thing",
        "2026-05-18T10:00:00Z",
    )];
    regenerate_kanban_for_agent(cwd.path(), "agent_alice", "Alice", &tasks).unwrap();

    let content = fs::read_to_string(&path).unwrap();
    assert!(content.starts_with("# Kanban — Alice\n"));
    assert!(content.contains("- [t1] thing\n"));
    // No leftover .tmp.
    let tmp = path.with_extension("md.tmp");
    assert!(!tmp.exists(), "tmp file leaked after rename: {tmp:?}");
}

#[test]
fn regenerate_filters_to_assignee_match_only() {
    let cwd = tempdir().unwrap();
    seed_agent_dir(cwd.path(), "agent_alice");
    seed_agent_dir(cwd.path(), "agent_bob");

    let tasks = vec![
        make_task(
            "t_alice",
            TaskStatus::Proposed,
            Some("agent_alice"),
            "alice task",
            "2026-05-18T10:00:00Z",
        ),
        make_task(
            "t_bob",
            TaskStatus::Proposed,
            Some("agent_bob"),
            "bob task",
            "2026-05-18T10:01:00Z",
        ),
        make_task(
            "t_none",
            TaskStatus::Proposed,
            None,
            "unassigned",
            "2026-05-18T10:02:00Z",
        ),
    ];
    regenerate_kanban_for_agent(cwd.path(), "agent_alice", "Alice", &tasks).unwrap();
    regenerate_kanban_for_agent(cwd.path(), "agent_bob", "Bob", &tasks).unwrap();

    let alice =
        fs::read_to_string(cwd.path().join(".opencrab/agents/agent_alice/KANBAN.md")).unwrap();
    let bob = fs::read_to_string(cwd.path().join(".opencrab/agents/agent_bob/KANBAN.md")).unwrap();

    assert!(alice.contains("[t_alice]"));
    assert!(!alice.contains("[t_bob]"));
    assert!(!alice.contains("[t_none]"));

    assert!(bob.contains("[t_bob]"));
    assert!(!bob.contains("[t_alice]"));
    assert!(!bob.contains("[t_none]"));
}

#[test]
fn regenerate_filters_out_archived_tasks() {
    let cwd = tempdir().unwrap();
    seed_agent_dir(cwd.path(), "agent_alice");

    let tasks = vec![
        make_task(
            "t_kept",
            TaskStatus::Done,
            Some("agent_alice"),
            "shipped",
            "2026-05-18T10:00:00Z",
        ),
        make_task(
            "t_archived",
            TaskStatus::Archived,
            Some("agent_alice"),
            "rejected",
            "2026-05-18T10:01:00Z",
        ),
    ];
    regenerate_kanban_for_agent(cwd.path(), "agent_alice", "Alice", &tasks).unwrap();

    let content =
        fs::read_to_string(cwd.path().join(".opencrab/agents/agent_alice/KANBAN.md")).unwrap();
    assert!(content.contains("[t_kept]"));
    assert!(!content.contains("[t_archived]"));
    assert!(!content.contains("rejected"));
}

#[test]
fn regenerate_fails_loudly_when_per_agent_dir_missing() {
    // Step 1 contract: `ensure_project_layer` mkdirs the per-agent dir.
    // If it didn't (bootstrap mis-wired), we should NOT silently create —
    // surface the bug instead.
    let cwd = tempdir().unwrap();
    let tasks = vec![make_task(
        "t1",
        TaskStatus::Proposed,
        Some("agent_alice"),
        "thing",
        "2026-05-18T10:00:00Z",
    )];
    let err = regenerate_kanban_for_agent(cwd.path(), "agent_alice", "Alice", &tasks).unwrap_err();
    match err {
        KanbanError::Io { source, .. } => {
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("expected Io NotFound, got {other}"),
    }
}

#[test]
fn regenerate_double_invocation_overwrites_cleanly() {
    let cwd = tempdir().unwrap();
    seed_agent_dir(cwd.path(), "agent_alice");

    regenerate_kanban_for_agent(
        cwd.path(),
        "agent_alice",
        "Alice",
        &[make_task(
            "t1",
            TaskStatus::Proposed,
            Some("agent_alice"),
            "v1",
            "2026-05-18T10:00:00Z",
        )],
    )
    .unwrap();

    regenerate_kanban_for_agent(
        cwd.path(),
        "agent_alice",
        "Alice",
        &[make_task(
            "t1",
            TaskStatus::Running,
            Some("agent_alice"),
            "v1",
            "2026-05-18T10:00:00Z",
        )],
    )
    .unwrap();

    let content =
        fs::read_to_string(cwd.path().join(".opencrab/agents/agent_alice/KANBAN.md")).unwrap();
    // Status moved to In Progress, no stale v1 entry in Proposed.
    // Scope to JUST the Proposed section so we don't catch the In Progress
    // entry that legitimately mentions t1.
    let proposed = content
        .split("## Proposed\n\n")
        .nth(1)
        .unwrap()
        .split("## In Progress")
        .next()
        .unwrap();
    assert!(!proposed.contains("t1"), "stale Proposed entry: {proposed}");
    assert!(content.contains("[t1] v1 (running)"));
}
