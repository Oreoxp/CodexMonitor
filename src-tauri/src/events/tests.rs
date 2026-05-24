// Phase 4 Step 6 — events module tests.
//
// Coverage:
//   * Serde round-trip + JSON wire shape for all 8 body variants
//     (incl. #[serde(flatten)] semantics + task_id Option handling).
//   * Timestamp + uuid helper format pins.
//   * `EventLog::open` parent-dir contract (existing → OK; missing → err).
//   * Single-emit + multi-emit ordering.
//   * Concurrent emit across threads (Mutex<File> + writeln! atomicity).
//   * Full task lifecycle drives 6 events in order.
//   * Illegal transitions do NOT leak events.
//   * (from, to) → TeamEventBody mapping is correct.
//   * `list_team_events_at_path` honors `limit` + parse-error skip.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use tempfile::tempdir;

use super::commands::list_team_events_at_path;
use super::log::{EventLog, EventLogError};
use super::types::{
    event_for_transition, iso8601_now_ms, uuid_v4_string, TeamEvent, TeamEventBody,
};
use crate::tasks::TaskStatus;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn project_events_dir(root: &Path, team_id: &str) -> std::path::PathBuf {
    root.join(".opencrab").join("teams").join(team_id)
}

/// Prepare the parent directory + 0-byte events.jsonl (the contract Step 1
/// fulfills in production via `ensure_project_layer`).
fn seed_team_dir(root: &Path, team_id: &str) -> std::path::PathBuf {
    let dir = project_events_dir(root, team_id);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events.jsonl");
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .unwrap();
    path
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// timestamp + uuid helpers
// ---------------------------------------------------------------------------

#[test]
fn iso8601_now_ms_has_millisecond_precision_and_z_suffix() {
    let ts = iso8601_now_ms();
    // `YYYY-MM-DDTHH:MM:SS.mmmZ` — exactly 24 chars.
    assert_eq!(ts.len(), 24, "expected 24-char ISO-8601-ms-Z, got {ts:?}");
    assert!(ts.ends_with('Z'), "timestamp missing Z suffix: {ts}");
    assert_eq!(&ts[4..5], "-", "wrong date separator in {ts}");
    assert_eq!(&ts[10..11], "T", "wrong date/time separator in {ts}");
    assert_eq!(&ts[19..20], ".", "wrong sec/ms separator in {ts}");
    // Round-trip parse via chrono to confirm well-formed.
    let _: chrono::DateTime<chrono::Utc> = ts
        .parse()
        .unwrap_or_else(|err| panic!("timestamp does not parse as RFC3339: {err} ({ts})"));
}

#[test]
fn uuid_v4_string_is_well_formed() {
    let u = uuid_v4_string();
    // Standard hyphenated UUID: 36 chars, hyphens at 8/13/18/23.
    assert_eq!(u.len(), 36, "wrong UUID length: {u}");
    assert_eq!(u.as_bytes()[8], b'-');
    assert_eq!(u.as_bytes()[13], b'-');
    assert_eq!(u.as_bytes()[18], b'-');
    assert_eq!(u.as_bytes()[23], b'-');
    // Re-parse to confirm a real UUID.
    let _: uuid::Uuid = u
        .parse()
        .unwrap_or_else(|err| panic!("not a parseable UUID: {err} ({u})"));
}

// ---------------------------------------------------------------------------
// serde — every variant round-trips + JSON wire shape pins
// ---------------------------------------------------------------------------

#[test]
fn round_trip_all_body_variants() {
    let variants = vec![
        TeamEventBody::TaskProposed {
            agent_id: "agent_alice".into(),
            title: "do the thing".into(),
            description: "longer body".into(),
        },
        TeamEventBody::TaskApproved {
            approved_by: "user".into(),
        },
        TeamEventBody::TaskRejected {
            rejected_by: "user".into(),
            reason: Some("not now".into()),
        },
        TeamEventBody::TaskRejected {
            rejected_by: "user".into(),
            reason: None,
        },
        TeamEventBody::TaskStarted {
            agent_id: "agent_bob".into(),
        },
        TeamEventBody::TaskBlocked {
            agent_id: "agent_bob".into(),
            reason: "waiting on upstream".into(),
        },
        TeamEventBody::TaskUnblocked {
            agent_id: "agent_bob".into(),
        },
        TeamEventBody::TaskDone {
            agent_id: "agent_bob".into(),
        },
        TeamEventBody::TaskArchived {
            archived_by: "user".into(),
        },
    ];
    for body in variants {
        let event = TeamEvent::new("team_demo", Some("task_1"), body.clone());
        let json = serde_json::to_string(&event).unwrap();
        let parsed: TeamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event, "round-trip mismatch: {json}");
    }
}

#[test]
fn task_proposed_wire_shape_flattens_eventtype_at_top_level() {
    let event = TeamEvent::new(
        "team_demo",
        Some("task_1"),
        TeamEventBody::TaskProposed {
            agent_id: "agent_alice".into(),
            title: "T".into(),
            description: "B".into(),
        },
    );
    let v: serde_json::Value = serde_json::to_value(&event).unwrap();
    // Top-level keys: schemaVersion, event_id, timestamp, team_id, task_id,
    // and flattened body (eventType + agent_id + title + description).
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["eventType"], "task_proposed");
    assert_eq!(v["team_id"], "team_demo");
    assert_eq!(v["task_id"], "task_1");
    assert_eq!(v["agent_id"], "agent_alice");
    assert_eq!(v["title"], "T");
    assert_eq!(v["description"], "B");
    // body is NOT nested.
    assert!(
        v.get("body").is_none(),
        "body key should not exist at top level"
    );
}

#[test]
fn task_id_none_is_omitted_from_json() {
    let event = TeamEvent::new(
        "team_demo",
        None,
        TeamEventBody::TaskApproved {
            approved_by: "user".into(),
        },
    );
    let v: serde_json::Value = serde_json::to_value(&event).unwrap();
    assert!(
        v.get("task_id").is_none(),
        "task_id should be omitted when None"
    );
}

#[test]
fn task_rejected_with_none_reason_omits_reason() {
    let event = TeamEvent::new(
        "team_demo",
        Some("task_1"),
        TeamEventBody::TaskRejected {
            rejected_by: "user".into(),
            reason: None,
        },
    );
    let v: serde_json::Value = serde_json::to_value(&event).unwrap();
    assert!(
        v.get("reason").is_none(),
        "reason should be omitted when None"
    );
}

#[test]
fn schema_version_constant_is_one() {
    assert_eq!(TeamEvent::SCHEMA_VERSION, 1);
}

// ---------------------------------------------------------------------------
// EventLog open contract
// ---------------------------------------------------------------------------

#[test]
fn open_succeeds_when_step1_layout_is_in_place() {
    let cwd = tempdir().unwrap();
    seed_team_dir(cwd.path(), "team_demo");
    let log = EventLog::open(cwd.path(), "team_demo").unwrap();
    assert!(log
        .path()
        .ends_with(".opencrab/teams/team_demo/events.jsonl"));
}

#[test]
fn open_fails_loudly_when_parent_dir_missing() {
    let cwd = tempdir().unwrap();
    // No `ensure_project_layer` ran — open should NOT silently create.
    let err = EventLog::open(cwd.path(), "team_demo").unwrap_err();
    match err {
        EventLogError::Io { path, .. } => {
            assert!(path.to_string_lossy().contains("team_demo/events.jsonl"));
        }
        other => panic!("expected Io error, got {other}"),
    }
}

// ---------------------------------------------------------------------------
// emit — ordering + concurrency
// ---------------------------------------------------------------------------

#[test]
fn emit_writes_one_line_per_event_in_order() {
    let cwd = tempdir().unwrap();
    let path = seed_team_dir(cwd.path(), "team_demo");
    let log = EventLog::open(cwd.path(), "team_demo").unwrap();

    log.emit(
        Some("t1"),
        TeamEventBody::TaskApproved {
            approved_by: "user".into(),
        },
    )
    .unwrap();
    log.emit(
        Some("t2"),
        TeamEventBody::TaskRejected {
            rejected_by: "user".into(),
            reason: Some("nope".into()),
        },
    )
    .unwrap();

    let lines = read_lines(&path);
    assert_eq!(lines.len(), 2);
    let e1: TeamEvent = serde_json::from_str(&lines[0]).unwrap();
    let e2: TeamEvent = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(e1.task_id.as_deref(), Some("t1"));
    assert_eq!(e2.task_id.as_deref(), Some("t2"));
    assert!(matches!(e1.body, TeamEventBody::TaskApproved { .. }));
    assert!(matches!(e2.body, TeamEventBody::TaskRejected { .. }));
}

#[test]
fn concurrent_emits_produce_no_torn_lines() {
    let cwd = tempdir().unwrap();
    let path = seed_team_dir(cwd.path(), "team_demo");
    let log = Arc::new(EventLog::open(cwd.path(), "team_demo").unwrap());

    let threads = 8;
    let per_thread = 50;
    let mut handles = Vec::with_capacity(threads);
    for tid in 0..threads {
        let log = Arc::clone(&log);
        handles.push(std::thread::spawn(move || {
            for i in 0..per_thread {
                log.emit(
                    Some(&format!("task-{tid}-{i}")),
                    TeamEventBody::TaskApproved {
                        approved_by: format!("user-{tid}"),
                    },
                )
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let lines = read_lines(&path);
    assert_eq!(lines.len(), threads * per_thread);
    // Every line must parse — proof against interleaved bytes within a line.
    for line in &lines {
        let _parsed: TeamEvent = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("torn line detected: err={err} line={line:?}"));
    }
}

#[test]
fn emit_does_not_truncate_existing_content() {
    let cwd = tempdir().unwrap();
    let path = seed_team_dir(cwd.path(), "team_demo");
    // Pre-fill the file with a synthetic prior line (representing a
    // previous process's output).
    fs::write(&path, "{\"prior\":\"event\"}\n").unwrap();

    let log = EventLog::open(cwd.path(), "team_demo").unwrap();
    log.emit(
        Some("t1"),
        TeamEventBody::TaskApproved {
            approved_by: "user".into(),
        },
    )
    .unwrap();

    let content = fs::read_to_string(&path).unwrap();
    assert!(content.starts_with("{\"prior\":\"event\"}\n"));
    assert!(content.contains("task_approved"));
}

// ---------------------------------------------------------------------------
// (from, to) → variant mapping
// ---------------------------------------------------------------------------

#[test]
fn event_for_transition_covers_legal_matrix() {
    use TaskStatus::*;
    // Every legal transition should produce a Some(_) body.
    let cases = vec![
        (Proposed, Ready, "task_approved"),
        (Proposed, Archived, "task_rejected"),
        (Ready, Running, "task_started"),
        (Ready, Archived, "task_archived"),
        (Running, Blocked, "task_blocked"),
        (Running, Done, "task_done"),
        (Blocked, Running, "task_unblocked"),
        (Blocked, Archived, "task_archived"),
        (Done, Archived, "task_archived"),
    ];
    for (from, to, expected_type) in cases {
        let body = event_for_transition(from, to, "user", None)
            .unwrap_or_else(|| panic!("no event for {from:?} → {to:?}"));
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(
            v["eventType"], expected_type,
            "{from:?} → {to:?} mapped to {v:?}"
        );
    }
}

#[test]
fn event_for_transition_returns_none_for_illegal_pair() {
    // Defensive — the state machine guards this in production, but the
    // mapping helper must not panic if reached.
    assert!(event_for_transition(TaskStatus::Done, TaskStatus::Running, "u", None).is_none());
    assert!(event_for_transition(TaskStatus::Proposed, TaskStatus::Running, "u", None).is_none());
}

#[test]
fn task_rejected_carries_feedback_when_present() {
    let body = event_for_transition(
        TaskStatus::Proposed,
        TaskStatus::Archived,
        "user",
        Some("too vague"),
    )
    .unwrap();
    match body {
        TeamEventBody::TaskRejected { reason, .. } => {
            assert_eq!(reason.as_deref(), Some("too vague"));
        }
        other => panic!("expected TaskRejected, got {other:?}"),
    }
}

#[test]
fn task_blocked_reason_falls_back_to_empty_string() {
    let body =
        event_for_transition(TaskStatus::Running, TaskStatus::Blocked, "agent_bob", None).unwrap();
    match body {
        TeamEventBody::TaskBlocked { reason, .. } => assert_eq!(reason, ""),
        other => panic!("expected TaskBlocked, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// list_team_events_at_path
// ---------------------------------------------------------------------------

#[test]
fn list_returns_empty_for_empty_file() {
    let cwd = tempdir().unwrap();
    seed_team_dir(cwd.path(), "team_demo");
    let out = list_team_events_at_path(cwd.path(), "team_demo", None).unwrap();
    assert!(out.events.is_empty());
    assert_eq!(out.total, 0);
    assert_eq!(out.parse_errors, 0);
}

#[test]
fn list_returns_all_events_in_write_order() {
    let cwd = tempdir().unwrap();
    seed_team_dir(cwd.path(), "team_demo");
    let log = EventLog::open(cwd.path(), "team_demo").unwrap();
    for i in 0..5 {
        log.emit(
            Some(&format!("t{i}")),
            TeamEventBody::TaskApproved {
                approved_by: format!("user-{i}"),
            },
        )
        .unwrap();
    }
    let out = list_team_events_at_path(cwd.path(), "team_demo", None).unwrap();
    assert_eq!(out.events.len(), 5);
    for (i, ev) in out.events.iter().enumerate() {
        assert_eq!(ev.task_id.as_deref(), Some(format!("t{i}").as_str()));
    }
}

#[test]
fn list_limit_returns_last_n_only() {
    let cwd = tempdir().unwrap();
    seed_team_dir(cwd.path(), "team_demo");
    let log = EventLog::open(cwd.path(), "team_demo").unwrap();
    for i in 0..5 {
        log.emit(
            Some(&format!("t{i}")),
            TeamEventBody::TaskApproved {
                approved_by: "u".into(),
            },
        )
        .unwrap();
    }
    let out = list_team_events_at_path(cwd.path(), "team_demo", Some(2)).unwrap();
    assert_eq!(out.events.len(), 2);
    assert_eq!(out.total, 5);
    // Last 2 entries = t3, t4.
    assert_eq!(out.events[0].task_id.as_deref(), Some("t3"));
    assert_eq!(out.events[1].task_id.as_deref(), Some("t4"));
}

#[test]
fn list_skips_malformed_lines_and_counts_them() {
    let cwd = tempdir().unwrap();
    let path = seed_team_dir(cwd.path(), "team_demo");
    let log = EventLog::open(cwd.path(), "team_demo").unwrap();
    log.emit(
        Some("t1"),
        TeamEventBody::TaskApproved {
            approved_by: "u".into(),
        },
    )
    .unwrap();
    // Append garbage to simulate partial / corrupted tail.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f, "this is not json").unwrap();
    writeln!(f, "{{partial\":").unwrap();
    drop(f);
    log.emit(
        Some("t2"),
        TeamEventBody::TaskApproved {
            approved_by: "u".into(),
        },
    )
    .unwrap();

    let out = list_team_events_at_path(cwd.path(), "team_demo", None).unwrap();
    assert_eq!(out.events.len(), 2);
    assert_eq!(out.parse_errors, 2);
}

#[test]
fn list_errors_when_team_file_missing() {
    let cwd = tempdir().unwrap();
    // No `ensure_project_layer` ran.
    let err = list_team_events_at_path(cwd.path(), "team_demo", None).unwrap_err();
    assert!(err.contains("not found"));
}

// ---------------------------------------------------------------------------
// "lifecycle" sim — drive a Task through 6 transitions, check 6 events
// ---------------------------------------------------------------------------

#[test]
fn full_task_lifecycle_emits_six_ordered_events() {
    let cwd = tempdir().unwrap();
    let path = seed_team_dir(cwd.path(), "team_demo");
    let log = EventLog::open(cwd.path(), "team_demo").unwrap();

    // Step 1: proposed
    log.emit(
        Some("task_alpha"),
        TeamEventBody::TaskProposed {
            agent_id: "agent_pm".into(),
            title: "do thing".into(),
            description: "details".into(),
        },
    )
    .unwrap();

    // Step 2: approved (proposed → ready)
    let body = event_for_transition(TaskStatus::Proposed, TaskStatus::Ready, "user", None).unwrap();
    log.emit(Some("task_alpha"), body).unwrap();

    // Step 3: started (ready → running)
    let body =
        event_for_transition(TaskStatus::Ready, TaskStatus::Running, "agent_pm", None).unwrap();
    log.emit(Some("task_alpha"), body).unwrap();

    // Step 4: blocked (running → blocked) with reason
    let body = event_for_transition(
        TaskStatus::Running,
        TaskStatus::Blocked,
        "agent_pm",
        Some("waiting on input"),
    )
    .unwrap();
    log.emit(Some("task_alpha"), body).unwrap();

    // Step 5: unblocked (blocked → running)
    let body =
        event_for_transition(TaskStatus::Blocked, TaskStatus::Running, "agent_pm", None).unwrap();
    log.emit(Some("task_alpha"), body).unwrap();

    // Step 6: done (running → done)
    let body =
        event_for_transition(TaskStatus::Running, TaskStatus::Done, "agent_pm", None).unwrap();
    log.emit(Some("task_alpha"), body).unwrap();

    let lines = read_lines(&path);
    assert_eq!(lines.len(), 6);
    let events: Vec<TeamEvent> = lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let kinds: Vec<&'static str> = events
        .iter()
        .map(|e| match e.body {
            TeamEventBody::TaskProposed { .. } => "task_proposed",
            TeamEventBody::TaskApproved { .. } => "task_approved",
            TeamEventBody::TaskStarted { .. } => "task_started",
            TeamEventBody::TaskBlocked { .. } => "task_blocked",
            TeamEventBody::TaskUnblocked { .. } => "task_unblocked",
            TeamEventBody::TaskDone { .. } => "task_done",
            TeamEventBody::TaskRejected { .. } => "task_rejected",
            TeamEventBody::TaskArchived { .. } => "task_archived",
            TeamEventBody::MemoryFlush { .. } => "memory_flush",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "task_proposed",
            "task_approved",
            "task_started",
            "task_blocked",
            "task_unblocked",
            "task_done",
        ]
    );
    // All events share the same task_id + team_id.
    for ev in &events {
        assert_eq!(ev.team_id, "team_demo");
        assert_eq!(ev.task_id.as_deref(), Some("task_alpha"));
        assert_eq!(ev.schema_version, 1);
    }
    // Timestamps are lex-sortable and non-decreasing across the sequence.
    let timestamps: Vec<&str> = events.iter().map(|e| e.timestamp.as_str()).collect();
    let mut sorted = timestamps.clone();
    sorted.sort();
    assert_eq!(timestamps, sorted, "timestamps not lex-sorted");
}
