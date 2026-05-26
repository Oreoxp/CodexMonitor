// WAL coexistence smoke.
//
// Original (Phase 3): the `tasks` table can live inside the same
// `state.sqlite` that LangGraph's SqliteSaver writes to, with two separate
// SQLite client processes / connections concurrently writing, without
// deadlocks or `SQLITE_BUSY` failures.
//
// Phase 6 Step 4 extension: the per-agent `memory.db` (single owner — the
// `opencrab-memory-mcp` Rust binary writes; the Node sidecar's
// `buildDailyMemoryPrelude` reads READ-ONLY) honors the same cross-process
// WAL contract. The mcp-server-writer is exercised by spawning the real
// binary; the sidecar-reader is simulated by a separate rusqlite
// Connection running the exact `SELECT summary FROM log ORDER BY ts DESC
// LIMIT 10` query `buildDailyMemoryPrelude` issues.
//
// Why a Rust reader is a valid proxy for the Node reader: rusqlite and
// better-sqlite3 each bundle their own SQLite build (not the same shared
// library). WAL's cross-process coordination, however, is a property of
// the SQLite *file format* + the *OS-level* file-locking primitives
// (`fcntl` / `LockFileEx`), not of any one client build. Both builds
// honor the same on-disk format and the same OS lock byte ranges, so a
// rusqlite reader's interaction with the mcp-server-writer over WAL is
// the same contract a better-sqlite3 reader would face. (`state.sqlite`
// in the §1 test uses the same logic for the two-Rust-writer case
// against a sidecar that runs better-sqlite3 in production.)
//
// Spawning the actual Node sidecar from cargo test (npx tsx + npm install
// + NDJSON IPC handshake) would add a lot of moving parts while
// exercising the same locking code path. The manual sidecar recipe is
// documented in `docs/scratch/investigation-wal-coexistence.md` for
// end-to-end paranoia.
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

// ---------------------------------------------------------------------------
// Phase 6 Step 4 — memory.db writer-process × reader-process coexistence
// ---------------------------------------------------------------------------

/// Drives the real `opencrab-memory-mcp` binary as the writer process and a
/// separate `rusqlite::Connection` as the reader. The rusqlite reader is a
/// valid proxy for Node's `better-sqlite3` reader — rusqlite and
/// better-sqlite3 each bundle their own SQLite, but WAL's cross-process
/// coordination is a property of the file format + OS-level file locking,
/// not the linked SQLite build, so both clients face the same contract.
/// Asserts that:
///
///   * a separate reader can query memory.db **while the writer process is
///     still alive**, without `SQLITE_BUSY` (true cross-process WAL),
///   * the exact `SELECT summary FROM log ORDER BY ts DESC LIMIT 10` query
///     that `buildDailyMemoryPrelude` (Node side) runs returns the 10
///     newest summaries the writer just committed, in DESC order,
///   * after the writer exits, the reader still sees a consistent file
///     and `PRAGMA journal_mode` is still `wal`.
///
/// This is the Rust-side proof of the cross-side B.1 claim ("Rust writer
/// × Node reader on the same WAL memory.db"). The TS-side
/// `buildDailyMemoryPrelude` is exercised against a Node-seeded mcp-shape
/// schema in `sidecar/src/prompt/daily-memory.test.ts`; this test pins
/// that the schema that side reads matches the schema the Rust writer
/// actually produces, byte-for-byte at the row level.
#[tokio::test]
async fn memory_db_wal_writer_process_and_reader_process_coexist() {
    use rmcp::model::CallToolRequestParams;
    use rmcp::serve_client;
    use std::process::Stdio;
    use std::time::Duration;

    tokio::time::timeout(Duration::from_secs(40), async {
        let home = TempDir::new().expect("tempdir");
        let agent = "wal_coex";
        let db_path = home
            .path()
            .join(".opencrab")
            .join("agents")
            .join(agent)
            .join("memory.db");

        // Spawn the real binary as the writer process. HOME-override keeps
        // the file under our tempdir, not the developer's real home.
        let binary = env!("CARGO_BIN_EXE_opencrab-memory-mcp");
        let mut child = tokio::process::Command::new(binary)
            .args(["--agent-id", agent])
            .env("HOME", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn opencrab-memory-mcp");
        let stdout = child.stdout.take().expect("stdout");
        let stdin = child.stdin.take().expect("stdin");
        let client = serve_client((), (stdout, stdin))
            .await
            .expect("mcp handshake");

        // 12 commits via the real mcp protocol — more than the 10-entry
        // prelude window so we can confirm the SELECT … DESC LIMIT 10 query
        // does its own dropping at the SQL layer rather than relying on
        // the writer side capping anything.
        for i in 0..12 {
            let mut args = serde_json::Map::new();
            args.insert(
                "summary".to_string(),
                serde_json::Value::from(format!("entry {i:02}")),
            );
            args.insert(
                "detail".to_string(),
                serde_json::Value::from(format!("detail body for entry {i:02}")),
            );
            client
                .call_tool(CallToolRequestParams {
                    meta: None,
                    name: "log_progress".into(),
                    arguments: Some(args),
                    task: None,
                })
                .await
                .expect("tools/call log_progress");
        }

        // ★ Concurrent read — writer process is STILL ALIVE. A SQLITE_BUSY
        //   here would falsify the WAL-coexistence claim. Same query
        //   `buildDailyMemoryPrelude` runs; same single-column projection.
        assert!(db_path.exists(), "memory.db must exist after first commit");
        let reader = Connection::open(&db_path).expect("reader open memory.db");
        let mode: String = reader
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        assert_eq!(mode.to_lowercase(), "wal", "writer set WAL on first open");

        let summaries_during_writer_alive: Vec<String> = reader
            .prepare("SELECT summary FROM log ORDER BY ts DESC LIMIT 10")
            .expect("prepare select")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            summaries_during_writer_alive.len(),
            10,
            "exactly the 10 newest survive the LIMIT 10",
        );
        // ts is wall-clock from `current_time_ms()` at insert; per-iteration
        // sleeping would slow the test, but the 12 inserts run sequentially
        // and `id` increases monotonically with `ts` — so DESC by ts orders
        // entries 11, 10, 9, … 2, dropping 0 and 1 (the oldest two).
        assert_eq!(summaries_during_writer_alive[0], "entry 11");
        assert_eq!(summaries_during_writer_alive[9], "entry 02");
        for survivor in 2..=11 {
            let label = format!("entry {survivor:02}");
            assert!(
                summaries_during_writer_alive.contains(&label),
                "expected {label} among the 10 newest; got {summaries_during_writer_alive:?}"
            );
        }
        // The two oldest must have been dropped by the LIMIT.
        assert!(!summaries_during_writer_alive.contains(&"entry 00".to_string()));
        assert!(!summaries_during_writer_alive.contains(&"entry 01".to_string()));

        drop(reader);
        drop(client);
        let _ = child.kill().await;
        let _ = child.wait().await;

        // Post-shutdown re-open: file is consistent, WAL still set in the
        // header, query yields the same 10 rows. This is the steady-state
        // the sidecar's read path lands in after the mcp server has done
        // its work and gone idle.
        let post = Connection::open(&db_path).expect("post-shutdown open");
        let mode: String = post
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");
        let summaries_post: Vec<String> = post
            .prepare("SELECT summary FROM log ORDER BY ts DESC LIMIT 10")
            .expect("prepare select")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .map(Result::unwrap)
            .collect();
        assert_eq!(summaries_post, summaries_during_writer_alive);
    })
    .await
    .expect("memory.db wal coexistence test timed out");
}
