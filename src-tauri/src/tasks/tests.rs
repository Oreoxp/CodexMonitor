// Unit tests for Phase 3 Step 1.
//
// Coverage targets (from the Step 1 spec):
//   - all 9 legal transitions accepted (one path per arrow);
//   - illegal transitions rejected loudly (matrix sample, including the
//     ones the spec called out: done→ready, archived→*, proposed→done);
//   - timestamp side-effects: approved_at on →ready, completed_at on →done,
//     updated_at bumped on every transition;
//   - schema migration is idempotent: running it twice does not error, does
//     not duplicate indices, and does not lose existing rows.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tempfile::TempDir;

use super::commands::{
    create_task_dev_only_at_path, get_task_at_path, list_tasks_at_path, transition_task_at_path,
    CreateTaskDevOnlyInput,
};
use super::state_machine::{is_legal, validate_transition, TaskError};
use super::store::{self, NewTask};
use super::types::{Task, TaskStatus};

// ---------------------------------------------------------------------------
// Step 4 polish: pin error-code prefix format.
//
// The frontend pattern-matches on the leading `[ERR_*]` tag to render
// friendly per-variant UX. A silent rename here breaks the modal's
// IllegalTransition detection in a hard-to-diagnose way (the friendly
// text would never trigger and the raw string would leak through). Pin
// each variant's Display output verbatim so a change has to update this
// test in the same PR.
// ---------------------------------------------------------------------------

#[test]
fn display_pins_error_code_prefixes() {
    let illegal = TaskError::IllegalTransition {
        from: TaskStatus::Ready,
        to: TaskStatus::Proposed,
    };
    assert_eq!(
        illegal.to_string(),
        "[ERR_ILLEGAL_TRANSITION] illegal task transition: ready → proposed"
    );

    let not_found = TaskError::NotFound("abc".into());
    assert_eq!(not_found.to_string(), "[ERR_NOT_FOUND] task not found: abc");

    // Sqlite + Io are wrappers; we only pin the prefix because the
    // underlying error message is implementation-defined. starts_with()
    // is the contract — if Sqlite's printable form ever needs the prefix
    // stripped, do it at the wrapper, not the variant.
    let sqlite_err = rusqlite::Error::QueryReturnedNoRows;
    let sqlite = TaskError::Sqlite(sqlite_err);
    assert!(
        sqlite.to_string().starts_with("[ERR_SQLITE] sqlite:"),
        "got: {sqlite}"
    );

    let io_err = std::io::Error::new(std::io::ErrorKind::Other, "boom");
    let io = TaskError::Io(io_err);
    assert!(io.to_string().starts_with("[ERR_IO] io:"), "got: {io}");
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fresh_workspace() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn seed_proposed(root: &Path) -> Task {
    let input = CreateTaskDevOnlyInput {
        workspace_id: "ws-1".into(),
        team_id: "team-1".into(),
        proposed_by_agent_id: "pm-alice".into(),
        title: "do a thing".into(),
        body: String::new(),
        assignee_agent_id: None,
    };
    create_task_dev_only_at_path(root, input).expect("seed proposed")
}

/// Walk a status sequence by repeated `transition`, asserting each hop
/// lands in the expected status. Returns the final hydrated `Task`.
fn walk(root: &Path, task_id: &str, actor: &str, sequence: &[TaskStatus]) -> Task {
    let mut last: Option<Task> = None;
    for status in sequence {
        let t = transition_task_at_path(root, task_id, *status, actor)
            .unwrap_or_else(|err| panic!("transition to {status:?} failed: {err}"));
        assert_eq!(t.status, *status);
        last = Some(t);
    }
    last.expect("at least one transition in sequence")
}

// ---------------------------------------------------------------------------
// Legal transitions — all 9 arrows
// ---------------------------------------------------------------------------

#[test]
fn legal_transition_proposed_to_ready_via_approve() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let approved = walk(&root, &task.id, "user", &[TaskStatus::Ready]);
    assert!(approved.approved_at.is_some(), "approved_at must be set");
    assert!(approved.completed_at.is_none());
    assert_ne!(approved.updated_at, task.updated_at);
}

#[test]
fn legal_transition_proposed_to_archived_via_reject() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let rejected = walk(&root, &task.id, "user", &[TaskStatus::Archived]);
    assert!(rejected.approved_at.is_none());
    assert!(rejected.completed_at.is_none());
}

#[test]
fn legal_transition_ready_to_running() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    walk(
        &root,
        &task.id,
        "user",
        &[TaskStatus::Ready, TaskStatus::Running],
    );
}

#[test]
fn legal_transition_ready_to_archived_cancel_before_run() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let final_task = walk(
        &root,
        &task.id,
        "user",
        &[TaskStatus::Ready, TaskStatus::Archived],
    );
    // approved_at must still carry the original approval timestamp.
    assert!(final_task.approved_at.is_some());
}

#[test]
fn legal_transition_running_to_blocked() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    walk(
        &root,
        &task.id,
        "pm-alice",
        &[TaskStatus::Ready, TaskStatus::Running, TaskStatus::Blocked],
    );
}

#[test]
fn legal_transition_running_to_done() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let done = walk(
        &root,
        &task.id,
        "pm-alice",
        &[TaskStatus::Ready, TaskStatus::Running, TaskStatus::Done],
    );
    assert!(done.approved_at.is_some());
    assert!(done.completed_at.is_some(), "completed_at must be set");
}

#[test]
fn legal_transition_blocked_to_running_via_unblock() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    walk(
        &root,
        &task.id,
        "pm-alice",
        &[
            TaskStatus::Ready,
            TaskStatus::Running,
            TaskStatus::Blocked,
            TaskStatus::Running,
        ],
    );
}

#[test]
fn legal_transition_blocked_to_archived() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    walk(
        &root,
        &task.id,
        "user",
        &[
            TaskStatus::Ready,
            TaskStatus::Running,
            TaskStatus::Blocked,
            TaskStatus::Archived,
        ],
    );
}

#[test]
fn legal_transition_done_to_archived() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let archived = walk(
        &root,
        &task.id,
        "user",
        &[
            TaskStatus::Ready,
            TaskStatus::Running,
            TaskStatus::Done,
            TaskStatus::Archived,
        ],
    );
    // completed_at must survive the archive step.
    assert!(archived.completed_at.is_some());
}

// ---------------------------------------------------------------------------
// Illegal transitions — rejected loudly with IllegalTransition
// ---------------------------------------------------------------------------

fn assert_illegal(err: TaskError, expected_from: TaskStatus, expected_to: TaskStatus) {
    match err {
        TaskError::IllegalTransition { from, to } => {
            assert_eq!(from, expected_from);
            assert_eq!(to, expected_to);
        }
        other => panic!("expected IllegalTransition, got {other:?}"),
    }
}

#[test]
fn illegal_transition_done_to_ready() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    walk(
        &root,
        &task.id,
        "pm-alice",
        &[TaskStatus::Ready, TaskStatus::Running, TaskStatus::Done],
    );
    let err = transition_task_at_path(&root, &task.id, TaskStatus::Ready, "user")
        .expect_err("done→ready must be illegal");
    assert_illegal(err, TaskStatus::Done, TaskStatus::Ready);
}

#[test]
fn illegal_transition_archived_to_anything() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    walk(&root, &task.id, "user", &[TaskStatus::Archived]);
    for target in [
        TaskStatus::Proposed,
        TaskStatus::Ready,
        TaskStatus::Running,
        TaskStatus::Blocked,
        TaskStatus::Done,
        TaskStatus::Archived,
    ] {
        let err = transition_task_at_path(&root, &task.id, target, "user")
            .expect_err(&format!("archived→{target:?} must be illegal"));
        assert_illegal(err, TaskStatus::Archived, target);
    }
}

#[test]
fn illegal_transition_proposed_to_done() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let err = transition_task_at_path(&root, &task.id, TaskStatus::Done, "pm-alice")
        .expect_err("proposed→done must be illegal");
    assert_illegal(err, TaskStatus::Proposed, TaskStatus::Done);
}

#[test]
fn illegal_transition_proposed_to_running() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let err = transition_task_at_path(&root, &task.id, TaskStatus::Running, "pm-alice")
        .expect_err("proposed→running must be illegal");
    assert_illegal(err, TaskStatus::Proposed, TaskStatus::Running);
}

#[test]
fn illegal_transition_running_to_ready() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    walk(
        &root,
        &task.id,
        "pm-alice",
        &[TaskStatus::Ready, TaskStatus::Running],
    );
    let err = transition_task_at_path(&root, &task.id, TaskStatus::Ready, "user")
        .expect_err("running→ready must be illegal");
    assert_illegal(err, TaskStatus::Running, TaskStatus::Ready);
}

#[test]
fn illegal_transition_done_to_running() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    walk(
        &root,
        &task.id,
        "pm-alice",
        &[TaskStatus::Ready, TaskStatus::Running, TaskStatus::Done],
    );
    let err = transition_task_at_path(&root, &task.id, TaskStatus::Running, "pm-alice")
        .expect_err("done→running must be illegal");
    assert_illegal(err, TaskStatus::Done, TaskStatus::Running);
}

#[test]
fn illegal_transition_self_loop_is_rejected() {
    // Self-loops are not in the matrix. Verifying one explicitly so a future
    // PR cannot silently add `X → X` as a no-op.
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let err = transition_task_at_path(&root, &task.id, TaskStatus::Proposed, "user")
        .expect_err("proposed→proposed must be illegal");
    assert_illegal(err, TaskStatus::Proposed, TaskStatus::Proposed);
}

#[test]
fn state_machine_matrix_predicate_matches_validate() {
    use TaskStatus::*;
    let all = [Proposed, Ready, Running, Blocked, Done, Archived];
    let mut legal_count = 0;
    for from in all {
        for to in all {
            let predicate = is_legal(from, to);
            let validate = validate_transition(from, to).is_ok();
            assert_eq!(
                predicate, validate,
                "is_legal and validate_transition disagreed on {from:?}→{to:?}"
            );
            if predicate {
                legal_count += 1;
            }
        }
    }
    assert_eq!(
        legal_count, 9,
        "Step 1 matrix must have exactly 9 legal arrows; got {legal_count}"
    );
}

// ---------------------------------------------------------------------------
// Side-effects
// ---------------------------------------------------------------------------

#[test]
fn side_effects_approved_at_only_on_ready() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    assert!(task.approved_at.is_none());

    let approved = transition_task_at_path(&root, &task.id, TaskStatus::Ready, "user").unwrap();
    assert!(approved.approved_at.is_some());

    // ready→running must NOT re-stamp approved_at.
    let running =
        transition_task_at_path(&root, &task.id, TaskStatus::Running, "pm-alice").unwrap();
    assert_eq!(
        running.approved_at, approved.approved_at,
        "approved_at must not be re-stamped after the initial approval"
    );
}

#[test]
fn side_effects_completed_at_only_on_done() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let done = walk(
        &root,
        &task.id,
        "pm-alice",
        &[TaskStatus::Ready, TaskStatus::Running, TaskStatus::Done],
    );
    let stamp = done.completed_at.clone();
    assert!(stamp.is_some());

    // done→archived must NOT re-stamp completed_at.
    let archived = transition_task_at_path(&root, &task.id, TaskStatus::Archived, "user").unwrap();
    assert_eq!(archived.completed_at, stamp);
}

#[test]
fn side_effects_updated_at_bumps_on_every_transition() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let initial_updated_at = task.updated_at.clone();

    // Sleep briefly so RFC3339-second precision can resolve a difference.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let approved = transition_task_at_path(&root, &task.id, TaskStatus::Ready, "user").unwrap();
    assert_ne!(approved.updated_at, initial_updated_at);

    std::thread::sleep(std::time::Duration::from_millis(1100));
    let running =
        transition_task_at_path(&root, &task.id, TaskStatus::Running, "pm-alice").unwrap();
    assert_ne!(running.updated_at, approved.updated_at);
}

// ---------------------------------------------------------------------------
// Migration idempotency
// ---------------------------------------------------------------------------

fn schema_fingerprint(conn: &Connection) -> Vec<(String, String)> {
    // (name, sql) of every object in sqlite_master EXCEPT auto-created index
    // metadata. We capture both table and index DDL so any silent CREATE
    // mutation between runs would surface as a diff.
    let mut stmt = conn
        .prepare(
            "SELECT name, COALESCE(sql, '') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn migration_idempotent_no_data_loss() {
    let (_g, root) = fresh_workspace();

    // Pass 1: open + init schema + insert a task.
    let task = seed_proposed(&root);

    // Snapshot schema state after pass 1.
    let after_pass1 = {
        let conn = store::open_and_init(&root).unwrap();
        schema_fingerprint(&conn)
    };
    assert!(
        !after_pass1.is_empty(),
        "schema fingerprint must include the tasks table"
    );

    // Pass 2: re-open. open_and_init runs the same migration statements
    // again; nothing should break, no rows lost.
    let after_pass2 = {
        let conn = store::open_and_init(&root).unwrap();
        schema_fingerprint(&conn)
    };
    assert_eq!(
        after_pass1, after_pass2,
        "schema fingerprint must be identical across repeated migrations"
    );

    let reloaded = get_task_at_path(&root, &task.id).unwrap().unwrap();
    assert_eq!(reloaded.id, task.id);
    assert_eq!(reloaded.title, "do a thing");
    assert_eq!(reloaded.status, TaskStatus::Proposed);
}

#[test]
fn migration_creates_expected_indices() {
    let (_g, root) = fresh_workspace();
    let conn = store::open_and_init(&root).unwrap();
    let fingerprint = schema_fingerprint(&conn);
    let names: Vec<&str> = fingerprint.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"tasks"), "tasks table missing");
    assert!(
        names.contains(&"idx_tasks_workspace_team_status"),
        "workspace+team+status index missing"
    );
    assert!(
        names.contains(&"idx_tasks_team_updated_at"),
        "team+updated_at index missing"
    );
}

// ---------------------------------------------------------------------------
// list / get smoke checks (filter + ordering)
// ---------------------------------------------------------------------------

#[test]
fn list_tasks_filter_by_status() {
    let (_g, root) = fresh_workspace();

    let a = seed_proposed(&root);
    // sleep to make updated_at ordering deterministic across rows
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let _b = seed_proposed(&root);

    transition_task_at_path(&root, &a.id, TaskStatus::Ready, "user").unwrap();

    let proposed =
        list_tasks_at_path(&root, "ws-1", "team-1", Some(&[TaskStatus::Proposed])).unwrap();
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].status, TaskStatus::Proposed);

    let ready = list_tasks_at_path(&root, "ws-1", "team-1", Some(&[TaskStatus::Ready])).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, a.id);

    let all = list_tasks_at_path(&root, "ws-1", "team-1", None).unwrap();
    assert_eq!(all.len(), 2);
    // updated_at DESC: `a` was just transitioned, so it should come first.
    assert_eq!(all[0].id, a.id);
}

#[test]
fn get_task_returns_none_for_unknown_id() {
    let (_g, root) = fresh_workspace();
    assert!(get_task_at_path(&root, "no-such-id").unwrap().is_none());
}

#[test]
fn transition_unknown_task_id_returns_not_found() {
    let (_g, root) = fresh_workspace();
    // Ensure schema exists so the failure is "row missing", not "table missing".
    let _ = store::open_and_init(&root).unwrap();
    let err = transition_task_at_path(&root, "no-such-id", TaskStatus::Ready, "user")
        .expect_err("missing id must fail");
    match err {
        TaskError::NotFound(id) => assert_eq!(id, "no-such-id"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// `insert_proposed` writes status=proposed; transition is the only mutator.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Step 2: insert_proposed_batch — transactional, shared timestamp
// ---------------------------------------------------------------------------

#[test]
fn batch_insert_writes_all_rows_with_shared_timestamp() {
    let (_g, root) = fresh_workspace();
    let new_tasks = vec![
        NewTask {
            id: "id-A".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: Some("dev-bob".into()),
            proposed_by_agent_id: "pm-alice".into(),
            title: "A".into(),
            body: "body A".into(),
        },
        NewTask {
            id: "id-B".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "B".into(),
            body: "body B".into(),
        },
    ];
    let persisted = store::insert_proposed_batch_at_path(&root, new_tasks).unwrap();
    assert_eq!(persisted.len(), 2);
    assert_eq!(persisted[0].title, "A");
    assert_eq!(persisted[1].title, "B");
    // All rows in one batch share created_at — this is the wire stand-in
    // for "this plan was emitted at <t>" until a plan_id column lands.
    assert_eq!(persisted[0].created_at, persisted[1].created_at);
    // Both rows are persisted and queryable.
    let listed = list_tasks_at_path(&root, "ws-1", "team-1", None).unwrap();
    assert_eq!(listed.len(), 2);
}

#[test]
fn batch_insert_rolls_back_on_failure() {
    // Inject a UUID collision: two NewTasks with the same `id` value. The
    // second INSERT must fail on PRIMARY KEY, and the transaction must
    // roll back, leaving zero rows. This proves the batch is atomic.
    let (_g, root) = fresh_workspace();
    let dup_id = "deliberate-collision".to_string();
    let new_tasks = vec![
        NewTask {
            id: dup_id.clone(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "A".into(),
            body: String::new(),
        },
        NewTask {
            id: dup_id,
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "B".into(),
            body: String::new(),
        },
    ];
    let err = store::insert_proposed_batch_at_path(&root, new_tasks).unwrap_err();
    // The error surfaces as a Sqlite constraint violation through TaskError.
    assert!(matches!(err, TaskError::Sqlite(_)), "got: {err:?}");
    let listed = list_tasks_at_path(&root, "ws-1", "team-1", None).unwrap();
    assert!(
        listed.is_empty(),
        "batch must roll back; got {} rows",
        listed.len()
    );
}

#[test]
fn batch_insert_with_zero_rows_is_a_noop() {
    // The router calls this when parser returns Ok([]) (empty plan). Should
    // succeed and write nothing.
    let (_g, root) = fresh_workspace();
    let out = store::insert_proposed_batch_at_path(&root, Vec::new()).unwrap();
    assert!(out.is_empty());
    let listed = list_tasks_at_path(&root, "ws-1", "team-1", None).unwrap();
    assert!(listed.is_empty());
}

// ---------------------------------------------------------------------------
// Step 3: hydration query — list_tasks_at_path with status='proposed' filter
// is what `team_router::hydrate_pending_approvals` calls on router start.
// These tests pin the contract: only proposed rows come back, and the
// (id, proposed_by_agent_id) tuple used to rebuild the latch is intact.
// ---------------------------------------------------------------------------

#[test]
fn hydration_query_returns_only_proposed_tasks() {
    let (_g, root) = fresh_workspace();
    // Mix of statuses: 2 proposed, 1 ready (post-approval), 1 archived.
    let seed = vec![
        NewTask {
            id: "p1".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "still proposed A".into(),
            body: String::new(),
        },
        NewTask {
            id: "p2".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "still proposed B".into(),
            body: String::new(),
        },
        NewTask {
            id: "approved".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "already approved".into(),
            body: String::new(),
        },
        NewTask {
            id: "rejected".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "already rejected".into(),
            body: String::new(),
        },
    ];
    store::insert_proposed_batch_at_path(&root, seed).unwrap();
    transition_task_at_path(&root, "approved", TaskStatus::Ready, "user").unwrap();
    transition_task_at_path(&root, "rejected", TaskStatus::Archived, "user").unwrap();

    let proposed =
        list_tasks_at_path(&root, "ws-1", "team-1", Some(&[TaskStatus::Proposed])).unwrap();
    let ids: std::collections::HashSet<&str> = proposed.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains("p1"));
    assert!(ids.contains("p2"));
    // proposed_by_agent_id round-trips intact for the latch metadata.
    for task in &proposed {
        assert_eq!(task.proposed_by_agent_id, "pm-alice");
    }
}

#[test]
fn hydration_query_isolates_by_team_id() {
    // A workspace may carry rows for multiple team_ids (e.g. team
    // recreated, old rows linger). Hydration must scope by team_id so a
    // freshly-started router's latch doesn't accidentally re-attach a
    // stale team's tasks.
    let (_g, root) = fresh_workspace();
    store::insert_proposed_batch_at_path(
        &root,
        vec![
            NewTask {
                id: "old".into(),
                workspace_id: "ws-1".into(),
                team_id: "team-old".into(),
                assignee_agent_id: None,
                proposed_by_agent_id: "pm-alice".into(),
                title: "stale".into(),
                body: String::new(),
            },
            NewTask {
                id: "new".into(),
                workspace_id: "ws-1".into(),
                team_id: "team-new".into(),
                assignee_agent_id: None,
                proposed_by_agent_id: "pm-alice".into(),
                title: "fresh".into(),
                body: String::new(),
            },
        ],
    )
    .unwrap();
    let new_team_proposed =
        list_tasks_at_path(&root, "ws-1", "team-new", Some(&[TaskStatus::Proposed])).unwrap();
    assert_eq!(new_team_proposed.len(), 1);
    assert_eq!(new_team_proposed[0].id, "new");
}

// ---------------------------------------------------------------------------
// Step 3: approve / reject collapsed to the DB-layer flip via the state
// machine. The router-side side-effects (latch removal + system reply
// dispatch) require an AppHandle and are exercised via manual dev-console
// smoke; see docs/scratch/investigation-approval-gate-smoke.md.
// ---------------------------------------------------------------------------

#[test]
fn approve_path_db_layer_marks_ready() {
    // Mirrors what approve_task does at the DB level: transition the
    // proposed row to Ready via the same `transition` chokepoint Step 1
    // built. The state-machine + side-effects (approved_at) are already
    // tested in §1; this test pins that approve_task's DB call has the
    // expected shape.
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let approved = transition_task_at_path(&root, &task.id, TaskStatus::Ready, "user").unwrap();
    assert_eq!(approved.status, TaskStatus::Ready);
    assert!(approved.approved_at.is_some());
    assert!(approved.completed_at.is_none());
}

#[test]
fn approve_already_approved_task_returns_illegal_transition() {
    // approve_task on a task whose status was already flipped to Ready
    // (either via approve a second time, or by a direct transition_task
    // call) must surface as `Err(IllegalTransition)` — we explicitly do
    // NOT pre-check the latch / pending_approvals before transitioning.
    // Source of truth = state machine.
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    transition_task_at_path(&root, &task.id, TaskStatus::Ready, "user").unwrap();
    let err = transition_task_at_path(&root, &task.id, TaskStatus::Ready, "user").unwrap_err();
    match err {
        TaskError::IllegalTransition { from, to } => {
            assert_eq!(from, TaskStatus::Ready);
            assert_eq!(to, TaskStatus::Ready);
        }
        other => panic!("expected IllegalTransition, got {other:?}"),
    }
}

#[test]
fn reject_path_db_layer_marks_archived_no_completed_at() {
    // reject_task transitions proposed → archived. The state machine does
    // NOT stamp completed_at for that arrow (that's done-only). Pin this
    // so a future side-effect change wouldn't accidentally mark rejected
    // tasks as "completed."
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let rejected = transition_task_at_path(&root, &task.id, TaskStatus::Archived, "user").unwrap();
    assert_eq!(rejected.status, TaskStatus::Archived);
    assert!(rejected.completed_at.is_none());
    assert!(rejected.approved_at.is_none());
}

// ---------------------------------------------------------------------------
// Step 4: update_task / TaskPatch — content edit on `proposed` rows
// ---------------------------------------------------------------------------

#[test]
fn update_task_edits_title_body_assignee_on_proposed_row() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let patch = store::TaskPatch {
        title: Some("new title".into()),
        body: Some("new body".into()),
        assignee: store::AssigneeUpdate::Set {
            value: "dev-bob".into(),
        },
    };
    let edited = store::update_content_at_path(&root, &task.id, patch, "user").unwrap();
    assert_eq!(edited.title, "new title");
    assert_eq!(edited.body, "new body");
    assert_eq!(edited.assignee_agent_id.as_deref(), Some("dev-bob"));
    assert_ne!(edited.updated_at, task.updated_at, "updated_at must bump");
    assert_eq!(edited.status, TaskStatus::Proposed);
}

#[test]
fn update_task_keep_assignee_does_not_touch_column() {
    // Seed with assignee=None. Patch with title-only — assignee stays None.
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    assert!(task.assignee_agent_id.is_none());
    let edited = store::update_content_at_path(
        &root,
        &task.id,
        store::TaskPatch {
            title: Some("just retitled".into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap();
    assert_eq!(edited.title, "just retitled");
    assert!(edited.assignee_agent_id.is_none());
}

#[test]
fn update_task_keep_assignee_preserves_existing_value() {
    // Seed → set assignee → patch title only → assignee stays.
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    store::update_content_at_path(
        &root,
        &task.id,
        store::TaskPatch {
            assignee: store::AssigneeUpdate::Set {
                value: "dev-bob".into(),
            },
            ..Default::default()
        },
        "user",
    )
    .unwrap();
    let edited = store::update_content_at_path(
        &root,
        &task.id,
        store::TaskPatch {
            title: Some("only title changed".into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap();
    assert_eq!(edited.title, "only title changed");
    assert_eq!(edited.assignee_agent_id.as_deref(), Some("dev-bob"));
}

#[test]
fn update_task_clear_assignee_writes_null() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    store::update_content_at_path(
        &root,
        &task.id,
        store::TaskPatch {
            assignee: store::AssigneeUpdate::Set {
                value: "dev-bob".into(),
            },
            ..Default::default()
        },
        "user",
    )
    .unwrap();
    let cleared = store::update_content_at_path(
        &root,
        &task.id,
        store::TaskPatch {
            assignee: store::AssigneeUpdate::Clear,
            ..Default::default()
        },
        "user",
    )
    .unwrap();
    assert!(cleared.assignee_agent_id.is_none());
}

#[test]
fn update_task_refuses_non_proposed_row() {
    // Transition to ready, then try to edit. The state-machine error type
    // surfaces so the modal can handle "this row is no longer editable"
    // uniformly with other invalid transitions.
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    transition_task_at_path(&root, &task.id, TaskStatus::Ready, "user").unwrap();
    let err = store::update_content_at_path(
        &root,
        &task.id,
        store::TaskPatch {
            title: Some("nope".into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap_err();
    match err {
        TaskError::IllegalTransition { from, to } => {
            assert_eq!(from, TaskStatus::Ready);
            assert_eq!(to, TaskStatus::Proposed);
        }
        other => panic!("expected IllegalTransition, got {other:?}"),
    }
}

#[test]
fn update_task_unknown_id_returns_not_found() {
    let (_g, root) = fresh_workspace();
    let _ = store::open_and_init(&root).unwrap();
    let err = store::update_content_at_path(
        &root,
        "missing",
        store::TaskPatch {
            title: Some("x".into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap_err();
    match err {
        TaskError::NotFound(id) => assert_eq!(id, "missing"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn update_task_noop_patch_does_not_bump_updated_at() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let before = task.updated_at.clone();
    let same = store::update_content_at_path(&root, &task.id, store::TaskPatch::default(), "user")
        .unwrap();
    assert_eq!(
        same.updated_at, before,
        "no-op patch must not bump updated_at"
    );
    assert_eq!(same.title, task.title);
    assert_eq!(same.body, task.body);
}

#[test]
fn update_task_status_never_changes_via_patch() {
    // Defense-in-depth: TaskPatch has no `status` field, but verify the
    // updated row stays `proposed` after any combination of edits.
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let edited = store::update_content_at_path(
        &root,
        &task.id,
        store::TaskPatch {
            title: Some("retitle".into()),
            body: Some("rebody".into()),
            assignee: store::AssigneeUpdate::Set {
                value: "dev-bob".into(),
            },
        },
        "user",
    )
    .unwrap();
    assert_eq!(edited.status, TaskStatus::Proposed);
}

#[test]
fn taskpatch_deserialization_handles_three_assignee_intents() {
    use serde_json::json;
    // Keep
    let p: store::TaskPatch =
        serde_json::from_value(json!({ "title": "t", "assignee": { "op": "keep" } })).unwrap();
    assert!(matches!(p.assignee, store::AssigneeUpdate::Keep));
    // Set
    let p: store::TaskPatch =
        serde_json::from_value(json!({ "assignee": { "op": "set", "value": "dev-bob" } })).unwrap();
    match p.assignee {
        store::AssigneeUpdate::Set { value } => assert_eq!(value, "dev-bob"),
        other => panic!("expected Set, got {other:?}"),
    }
    // Clear
    let p: store::TaskPatch =
        serde_json::from_value(json!({ "assignee": { "op": "clear" } })).unwrap();
    assert!(matches!(p.assignee, store::AssigneeUpdate::Clear));
    // Default (omitted)
    let p: store::TaskPatch = serde_json::from_value(json!({})).unwrap();
    assert!(matches!(p.assignee, store::AssigneeUpdate::Keep));
}

#[test]
fn insert_proposed_always_creates_proposed_row() {
    let (_g, root) = fresh_workspace();
    let conn = store::open_and_init(&root).unwrap();
    let task = store::insert_proposed(
        &conn,
        NewTask {
            id: "fixed-id".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: Some("dev-bob".into()),
            proposed_by_agent_id: "pm-alice".into(),
            title: "hello".into(),
            body: "world".into(),
        },
    )
    .unwrap();
    assert_eq!(task.status, TaskStatus::Proposed);
    assert!(task.approved_at.is_none());
    assert!(task.completed_at.is_none());
    assert_eq!(task.assignee_agent_id.as_deref(), Some("dev-bob"));
    // Phase 3 bug fix: even single-task inserts via insert_proposed
    // receive a synthetic plan_id so the merged-message code path is
    // uniform. Legacy NULL plan_id is only for rows written by code that
    // bypasses this function entirely (e.g. direct SQL in a future test).
    assert!(task.plan_id.is_some());
    assert!(task.plan_id.as_deref().unwrap().starts_with("plan_"));
    assert!(task.feedback.is_none());
}

// ---------------------------------------------------------------------------
// Phase 3 bug fix: plan-level approval batching — DB shape tests
// ---------------------------------------------------------------------------

#[test]
fn batch_insert_shares_one_plan_id_across_all_rows() {
    let (_g, root) = fresh_workspace();
    let news = vec![
        NewTask {
            id: "t1".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "A".into(),
            body: String::new(),
        },
        NewTask {
            id: "t2".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: Some("dev-bob".into()),
            proposed_by_agent_id: "pm-alice".into(),
            title: "B".into(),
            body: String::new(),
        },
        NewTask {
            id: "t3".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "C".into(),
            body: String::new(),
        },
    ];
    let rows = store::insert_proposed_batch_at_path(&root, news).unwrap();
    assert_eq!(rows.len(), 3);
    let pid = rows[0].plan_id.clone().expect("plan_id present");
    assert!(pid.starts_with("plan_"));
    for row in &rows {
        assert_eq!(row.plan_id.as_deref(), Some(pid.as_str()));
        assert!(row.feedback.is_none());
    }
}

#[test]
fn batch_insert_two_separate_calls_get_distinct_plan_ids() {
    let (_g, root) = fresh_workspace();
    let make = |id: &str| NewTask {
        id: id.into(),
        workspace_id: "ws-1".into(),
        team_id: "team-1".into(),
        assignee_agent_id: None,
        proposed_by_agent_id: "pm-alice".into(),
        title: id.into(),
        body: String::new(),
    };
    let batch_a =
        store::insert_proposed_batch_at_path(&root, vec![make("a1"), make("a2")]).unwrap();
    let batch_b = store::insert_proposed_batch_at_path(&root, vec![make("b1")]).unwrap();
    let pid_a = batch_a[0].plan_id.clone().unwrap();
    let pid_b = batch_b[0].plan_id.clone().unwrap();
    assert_ne!(pid_a, pid_b, "each plan must have its own plan_id");
    // batch_a's two rows share, but not with batch_b's row.
    assert_eq!(batch_a[0].plan_id, batch_a[1].plan_id);
}

#[test]
fn list_by_plan_returns_all_tasks_in_plan_only() {
    let (_g, root) = fresh_workspace();
    let make = |id: &str| NewTask {
        id: id.into(),
        workspace_id: "ws-1".into(),
        team_id: "team-1".into(),
        assignee_agent_id: None,
        proposed_by_agent_id: "pm-alice".into(),
        title: id.into(),
        body: String::new(),
    };
    let plan_a =
        store::insert_proposed_batch_at_path(&root, vec![make("a1"), make("a2"), make("a3")])
            .unwrap();
    let _plan_b = store::insert_proposed_batch_at_path(&root, vec![make("b1")]).unwrap();
    let pid_a = plan_a[0].plan_id.clone().unwrap();

    let conn = store::open_and_init(&root).unwrap();
    let listed = store::list_by_plan(&conn, &pid_a).unwrap();
    assert_eq!(listed.len(), 3);
    let ids: std::collections::HashSet<&str> = listed.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains("a1") && ids.contains("a2") && ids.contains("a3"));
    assert!(!ids.contains("b1"));
}

#[test]
fn plan_still_has_proposed_tracks_completion() {
    let (_g, root) = fresh_workspace();
    let news = vec![
        NewTask {
            id: "t1".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "A".into(),
            body: String::new(),
        },
        NewTask {
            id: "t2".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "B".into(),
            body: String::new(),
        },
        NewTask {
            id: "t3".into(),
            workspace_id: "ws-1".into(),
            team_id: "team-1".into(),
            assignee_agent_id: None,
            proposed_by_agent_id: "pm-alice".into(),
            title: "C".into(),
            body: String::new(),
        },
    ];
    let rows = store::insert_proposed_batch_at_path(&root, news).unwrap();
    let pid = rows[0].plan_id.clone().unwrap();

    let conn = store::open_and_init(&root).unwrap();
    assert!(store::plan_still_has_proposed(&conn, &pid).unwrap());

    transition_task_at_path(&root, "t1", TaskStatus::Ready, "user").unwrap();
    assert!(store::plan_still_has_proposed(&conn, &pid).unwrap());

    transition_task_at_path(&root, "t2", TaskStatus::Archived, "user").unwrap();
    assert!(store::plan_still_has_proposed(&conn, &pid).unwrap());

    transition_task_at_path(&root, "t3", TaskStatus::Ready, "user").unwrap();
    assert!(!store::plan_still_has_proposed(&conn, &pid).unwrap());
}

#[test]
fn transition_with_feedback_persists_to_db() {
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    // Reject with feedback via the new function.
    let rejected = store::transition_with_feedback(
        &store::open_and_init(&root).unwrap(),
        &task.id,
        TaskStatus::Archived,
        "user",
        Some("scope too big — narrow to file moves only"),
    )
    .unwrap();
    assert_eq!(rejected.status, TaskStatus::Archived);
    assert_eq!(
        rejected.feedback.as_deref(),
        Some("scope too big — narrow to file moves only")
    );
    // And it round-trips through a fresh DB read (i.e. it's on disk, not
    // just in the returned struct).
    let reloaded = get_task_at_path(&root, &task.id).unwrap().unwrap();
    assert_eq!(
        reloaded.feedback.as_deref(),
        Some("scope too big — narrow to file moves only")
    );
}

#[test]
fn transition_without_feedback_preserves_existing_feedback() {
    // Defensive: if a row already carries feedback (e.g. set on a prior
    // failed transition attempt), a follow-up `transition` without a
    // feedback arg must NOT null it out. The COALESCE in the UPDATE
    // protects this; pin it here.
    let (_g, root) = fresh_workspace();
    let task = seed_proposed(&root);
    let conn = store::open_and_init(&root).unwrap();
    // Manually set feedback on a still-proposed row (bypass the helper
    // since proposed rows don't normally have feedback).
    conn.execute(
        "UPDATE tasks SET feedback = ?1 WHERE id = ?2",
        rusqlite::params!["pre-existing fb", task.id],
    )
    .unwrap();
    // Now transition via the no-feedback path.
    let approved = store::transition(&conn, &task.id, TaskStatus::Ready, "user").unwrap();
    assert_eq!(approved.feedback.as_deref(), Some("pre-existing fb"));
}

#[test]
fn schema_migration_idempotent_with_plan_id_and_feedback_columns() {
    // Pin: re-running open_and_init never drops or duplicates the new
    // columns. Verified by checking the schema fingerprint stays stable.
    let (_g, root) = fresh_workspace();
    let conn1 = store::open_and_init(&root).unwrap();
    let cols1: Vec<String> = {
        let mut stmt = conn1.prepare("PRAGMA table_info(tasks)").unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert!(cols1.iter().any(|c| c == "plan_id"));
    assert!(cols1.iter().any(|c| c == "feedback"));
    drop(conn1);

    let conn2 = store::open_and_init(&root).unwrap();
    let cols2: Vec<String> = {
        let mut stmt = conn2.prepare("PRAGMA table_info(tasks)").unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert_eq!(cols1, cols2, "schema must be stable across repeated init");
}

#[test]
fn legacy_null_plan_id_rows_round_trip() {
    // Simulate a pre-fix workspace: insert a row directly via SQL with
    // plan_id NULL, then read it back. The hydrator + list_tasks must
    // not choke on NULL plan_id; the notify_* path's legacy branch will
    // pick it up.
    let (_g, root) = fresh_workspace();
    let conn = store::open_and_init(&root).unwrap();
    let now = "2026-05-17T00:00:00Z";
    conn.execute(
        "INSERT INTO tasks (id, workspace_id, team_id, assignee_agent_id, \
         proposed_by_agent_id, status, title, body, approved_at, completed_at, \
         created_at, updated_at, plan_id, feedback) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, NULL)",
        rusqlite::params![
            "legacy-1",
            "ws-1",
            "team-1",
            Option::<String>::None,
            "pm-alice",
            "proposed",
            "legacy task",
            "",
            Option::<String>::None,
            Option::<String>::None,
            now,
            now,
        ],
    )
    .unwrap();
    let reloaded = get_task_at_path(&root, "legacy-1").unwrap().unwrap();
    assert!(reloaded.plan_id.is_none(), "legacy row keeps NULL plan_id");
    assert!(reloaded.feedback.is_none());
    // The status-machine transition still works on a legacy row.
    let approved = transition_task_at_path(&root, "legacy-1", TaskStatus::Ready, "user").unwrap();
    assert_eq!(approved.status, TaskStatus::Ready);
    // And plan_id stays NULL after the transition (transition doesn't
    // synthesize a plan_id).
    assert!(approved.plan_id.is_none());
}
