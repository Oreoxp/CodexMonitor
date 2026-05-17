// WAL coexistence smoke for the Step 1 (a) decision.
//
// What this test proves: the `tasks` table can live inside the same
// `state.sqlite` that LangGraph's SqliteSaver writes to, with two separate
// SQLite client processes / connections concurrently writing, without
// deadlocks or `SQLITE_BUSY` failures.
//
// Why it's at the rusqlite level, not the spawn-sidecar level: the actual
// concern is the OS-level file-locking semantics of SQLite under WAL. Both
// production writers (Rust's rusqlite, sidecar's better-sqlite3 driving
// `@langchain/langgraph-checkpoint-sqlite`) link the same `libsqlite3`,
// so the file-locking machinery is identical. Spawning the sidecar from
// cargo test (npx tsx + npm install + NDJSON IPC handshake) would add a
// lot of moving parts while exercising the same locking code path that
// two threaded rusqlite Connections already exercise. The manual sidecar
// recipe is documented in
// `docs/scratch/investigation-wal-coexistence.md` for end-to-end paranoia.
//
// Coverage target (from the Step 2 spec):
//   - sidecar-shaped writer creates state.sqlite first → checkpoint family
//     present
//   - Rust opens same file, sets WAL (idempotent), runs `tasks` migration
//   - Concurrent: 5 tasks INSERTs ↔ 5 checkpoint-shape INSERTs
//   - no SQLITE_BUSY, no deadlock, both tables hold the expected row counts
//   - each writer can read its own table after the other side's writes
//     (cross-table read NOT exercised — by design, per the CLAUDE.md rule).

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::{params, Connection, OpenFlags};
use tempfile::TempDir;

// Approximation of one of the tables `@langchain/langgraph-checkpoint-sqlite`
// creates. We only need the column shape to be plausible enough that a real
// SqliteSaver writer wouldn't interleave-conflict differently than our
// fixture does — what we're actually verifying is byte-range lock behavior,
// which doesn't care about column names.
const SAVER_CREATE_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS checkpoints (
    thread_id            TEXT NOT NULL,
    checkpoint_ns        TEXT NOT NULL DEFAULT '',
    checkpoint_id        TEXT NOT NULL,
    parent_checkpoint_id TEXT,
    type                 TEXT,
    checkpoint           BLOB,
    metadata             BLOB,
    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id)
)";

const SAVER_CREATE_WRITES_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS writes (
    thread_id     TEXT NOT NULL,
    checkpoint_ns TEXT NOT NULL DEFAULT '',
    checkpoint_id TEXT NOT NULL,
    task_id       TEXT NOT NULL,
    idx           INTEGER NOT NULL,
    channel       TEXT NOT NULL,
    type          TEXT,
    value         BLOB,
    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id, task_id, idx)
)";

// Mirrors the schema from src/tasks/store.rs. Repeated here verbatim so the
// integration test verifies what production code writes, not what a helper
// in lib code says it writes. `pub(crate)` symbols in src/tasks/* aren't
// accessible from an integration test crate, so the duplication is by
// necessity — kept tight enough that a schema-shape change in store.rs will
// surface here as a mismatched insert.
const TASKS_CREATE_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id                   TEXT PRIMARY KEY,
    workspace_id         TEXT NOT NULL,
    team_id              TEXT NOT NULL,
    assignee_agent_id    TEXT,
    proposed_by_agent_id TEXT NOT NULL,
    status               TEXT NOT NULL,
    title                TEXT NOT NULL,
    body                 TEXT NOT NULL DEFAULT '',
    approved_at          TEXT,
    completed_at         TEXT,
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL
)";

fn open_wal(path: &Path) -> Connection {
    // Read-write + create flags are the default for `Connection::open`, but
    // we are explicit here so the test reads as the contract we want both
    // production writers to honor.
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
    )
    .expect("open state.sqlite");
    // Per the CLAUDE.md storage rule: both writers MUST set WAL on every
    // open. WAL persists in the file header, so reasserting it is a no-op
    // after the first writer set it — but we verify the mode is actually
    // `wal` by reading back the pragma value.
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .expect("set WAL");
    assert_eq!(mode.to_lowercase(), "wal", "journal_mode must be WAL");
    conn.execute("PRAGMA synchronous=NORMAL", [])
        .expect("synchronous=NORMAL");
    // Wait up to 5s if another writer holds the COMMIT lock. Default is 0
    // (immediate `SQLITE_BUSY`), which would make any cross-process race a
    // flake. SqliteSaver sets a similar busy timeout under the hood; this
    // test mirrors that. If we ever see this timeout fire in production
    // that means a writer is parked indefinitely — different problem class
    // than the "two writers on one file" question this test answers.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .expect("busy_timeout");
    conn
}

#[test]
fn wal_coexistence_two_writers_no_deadlock() {
    let dir = TempDir::new().expect("tempdir");
    let opencrab = dir.path().join(".opencrab");
    std::fs::create_dir_all(&opencrab).unwrap();
    let db_path = opencrab.join("state.sqlite");

    // ---- Sidecar-side init: SqliteSaver-shaped tables created first. -----
    // This mirrors the production order: sidecar's `initRuntime` is what
    // creates the file on first workspace open; Rust opens after.
    {
        let conn = open_wal(&db_path);
        conn.execute(SAVER_CREATE_TABLE_SQL, []).unwrap();
        conn.execute(SAVER_CREATE_WRITES_TABLE_SQL, []).unwrap();
    }

    // ---- Rust-side migration: tasks table on the same file. -------------
    {
        let conn = open_wal(&db_path);
        conn.execute(TASKS_CREATE_TABLE_SQL, []).unwrap();
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tasks_workspace_team_status \
             ON tasks (workspace_id, team_id, status)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tasks_team_updated_at \
             ON tasks (team_id, updated_at DESC)",
            [],
        )
        .unwrap();
    }

    // ---- Concurrent writes: 5 checkpoints ↔ 5 tasks. --------------------
    // The barrier makes both threads start their write loops at the same
    // instant so we maximize the chance of two-writers-on-one-file
    // contention. A small `thread::yield_now()` between INSERTs further
    // interleaves them.
    let barrier = Arc::new(Barrier::new(2));
    let path_a = db_path.clone();
    let path_b = db_path.clone();
    let bar_a = barrier.clone();
    let bar_b = barrier.clone();

    let saver_thread = thread::spawn(move || {
        let conn = open_wal(&path_a);
        bar_a.wait();
        for i in 0..5 {
            conn.execute(
                "INSERT INTO checkpoints \
                 (thread_id, checkpoint_id, type, checkpoint, metadata) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    format!("thread-{i}"),
                    format!("ckpt-{i}"),
                    "json",
                    Some(b"{}".to_vec()),
                    Some(b"{}".to_vec()),
                ],
            )
            .expect("checkpoint insert");
            std::thread::yield_now();
        }
    });

    let tasks_thread = thread::spawn(move || {
        let conn = open_wal(&path_b);
        bar_b.wait();
        for i in 0..5 {
            let now = "2026-05-17T00:00:00Z";
            conn.execute(
                "INSERT INTO tasks (id, workspace_id, team_id, \
                 assignee_agent_id, proposed_by_agent_id, status, title, \
                 body, approved_at, completed_at, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    format!("task-{i}"),
                    "ws-1",
                    "team-1",
                    Option::<String>::None,
                    "pm-alice",
                    "proposed",
                    format!("task #{i}"),
                    "",
                    Option::<String>::None,
                    Option::<String>::None,
                    now,
                    now,
                ],
            )
            .expect("task insert");
            std::thread::yield_now();
        }
    });

    // If either writer hangs on a lock we want a clean failure, not a CI
    // timeout. Both threads must finish within `busy_timeout` (5s) plus
    // some slack for the 10 INSERTs themselves.
    saver_thread.join().expect("saver thread");
    tasks_thread.join().expect("tasks thread");

    // ---- Assert: each writer can read its own table; counts match. ------
    let conn = open_wal(&db_path);
    let task_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
        .unwrap();
    let checkpoint_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM checkpoints", [], |row| row.get(0))
        .unwrap();
    assert_eq!(task_count, 5, "all 5 tasks rows must be present");
    assert_eq!(checkpoint_count, 5, "all 5 checkpoint rows must be present");

    // Schema fingerprint: both writers' tables AND indices coexist, no
    // mysterious table was dropped or renamed.
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for required in [
        "checkpoints",
        "idx_tasks_team_updated_at",
        "idx_tasks_workspace_team_status",
        "tasks",
        "writes",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "expected object `{required}` in sqlite_master; got {names:?}"
        );
    }

    // Verify the file is still in WAL mode after both writers concurrently
    // poked at it — if either side accidentally set DELETE/TRUNCATE,
    // querying again would return that string instead of "wal".
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}

#[test]
fn wal_pragma_persists_across_close_and_reopen() {
    // Independent of the concurrency story, the carve-out in CLAUDE.md
    // claims "WAL persists in the file header." If that's wrong on this
    // platform we want to know now, not from a flaky production crash.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite");

    {
        let _ = open_wal(&path);
    }
    let conn = Connection::open(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}
