// Phase 6 Step 4 — stdio smoke for the `opencrab-memory-mcp` memory.db
// contract. Replaces the Phase 5 markdown-corpus stub.
//
// Spawns the real built binary with `HOME` pointing at a tempdir so the
// server resolves `~/.opencrab/agents/<id>/memory.db` inside that tempdir
// (not the developer's real home). Drives the MCP protocol via rmcp client.
// Three contract claims, one test each:
//
//   1. log_progress (summary-only / summary+detail) lands rows on disk —
//      verified by INDEPENDENTLY opening memory.db via rusqlite, not by
//      trusting the tool ack. Direct guard against the P5 "tool said ok
//      but the file never appeared" symptom (the original "目录存在但空"
//      bug that motivated P6).
//   2. memory_search + memory_get round-trip through the real server
//      process — the AFTER INSERT trigger populates `log_fts`, bm25 ranks
//      the right row, by-id `get` returns the full untruncated detail.
//   3. Distinct `--agent-id` values write to distinct memory.db files
//      (per-agent isolation by construction; no cross-contamination).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::serve_client;
use rmcp::service::{RoleClient, RunningService};
use rusqlite::Connection;
use serde_json::{json, Value};

const BINARY: &str = env!("CARGO_BIN_EXE_opencrab-memory-mcp");

/// Mirrors `paths.rs::user_agent_memory_db` without depending on lib
/// visibility — integration tests can't see `pub(crate)`. Repeating the
/// layout here means a path-layout regression (mcp server writes somewhere
/// else, or the lib resolver moves) is caught by this test.
fn expected_memory_db(home: &Path, agent_id: &str) -> PathBuf {
    home.join(".opencrab")
        .join("agents")
        .join(agent_id)
        .join("memory.db")
}

#[tokio::test]
async fn stdio_smoke_log_progress_writes_land_on_disk() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let home = tempfile::tempdir().expect("tempdir");
        let agent = "smoke_writer";
        let db = expected_memory_db(home.path(), agent);
        assert!(!db.exists(), "precondition: db must not pre-exist");

        let mut child = spawn_server(home.path(), agent);
        let stdout = child.stdout.take().expect("child stdout");
        let stdin = child.stdin.take().expect("child stdin");
        let client = serve_client((), (stdout, stdin))
            .await
            .expect("mcp initialize handshake");

        // tools/list — the three P6 tools are advertised.
        let tools = client.list_all_tools().await.expect("tools/list");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for required in ["log_progress", "memory_search", "memory_get"] {
            assert!(names.contains(&required), "tools were {names:?}");
        }

        let id_summary_only = call_log_progress(&client, "summary only entry", None).await;
        let id_with_detail = call_log_progress(
            &client,
            "with detail entry",
            Some("Refactored auth to JWT after the security review."),
        )
        .await;
        assert_ne!(id_summary_only, id_with_detail, "ids must be distinct");

        // Shut the server down so its file handle is released before we
        // open memory.db ourselves. Killing is safe — committed transactions
        // are durable under WAL+synchronous=NORMAL.
        drop(client);
        let _ = child.kill().await;
        let _ = child.wait().await;

        // ★ INDEPENDENT disk read — the P5 bug was "log_progress returned
        //   ok but no row landed". We don't trust the tool ack; we open
        //   memory.db ourselves and verify the rows are there with the
        //   exact summary + detail we sent.
        assert!(db.exists(), "memory.db must exist after first write");
        let rows = dump_log_rows(&db);
        assert_eq!(rows.len(), 2, "exactly two rows on disk; got {rows:?}");
        assert_eq!(rows[0].0, id_summary_only);
        assert_eq!(rows[0].1, "summary only entry");
        assert_eq!(rows[0].2, None, "summary-only row must store NULL detail");
        assert_eq!(rows[1].0, id_with_detail);
        assert_eq!(rows[1].1, "with detail entry");
        assert_eq!(
            rows[1].2.as_deref(),
            Some("Refactored auth to JWT after the security review.")
        );
    })
    .await
    .expect("smoke #1 timed out");
}

#[tokio::test]
async fn stdio_smoke_search_then_get_round_trips_through_the_server() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let home = tempfile::tempdir().expect("tempdir");
        let mut child = spawn_server(home.path(), "smoke_search");
        let stdout = child.stdout.take().expect("stdout");
        let stdin = child.stdin.take().expect("stdin");
        let client = serve_client((), (stdout, stdin))
            .await
            .expect("handshake");

        // Two entries; only the first has the search term in detail.
        let target_id = call_log_progress(
            &client,
            "auth refactor day 1",
            Some("Refactored the OAuth middleware to use the new token model."),
        )
        .await;
        call_log_progress(
            &client,
            "lunch break",
            Some("Notes about the cafeteria menu and tomorrow's standup."),
        )
        .await;

        // memory_search — FTS5 matches only the OAuth row.
        let mut args = serde_json::Map::new();
        args.insert("query".to_string(), json!("OAuth"));
        let result = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "memory_search".into(),
                arguments: Some(args),
                task: None,
            })
            .await
            .expect("tools/call memory_search");
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["count"], 1, "structured = {structured}");
        assert_eq!(structured["hits"][0]["id"], target_id);
        assert!(
            structured["hits"][0]["snippet"]
                .as_str()
                .unwrap_or_default()
                .contains("OAuth"),
            "snippet = {}",
            structured["hits"][0]["snippet"],
        );

        // memory_get by id — returns the full untruncated detail.
        let mut args = serde_json::Map::new();
        args.insert("id".to_string(), json!(target_id));
        let result = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "memory_get".into(),
                arguments: Some(args),
                task: None,
            })
            .await
            .expect("tools/call memory_get");
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["found"], true);
        assert_eq!(structured["id"], target_id);
        assert_eq!(structured["summary"], "auth refactor day 1");
        assert_eq!(
            structured["detail"].as_str(),
            Some("Refactored the OAuth middleware to use the new token model."),
        );

        drop(client);
        let _ = child.kill().await;
        let _ = child.wait().await;
    })
    .await
    .expect("smoke #2 timed out");
}

#[tokio::test]
async fn stdio_smoke_distinct_agent_ids_write_to_distinct_memory_dbs() {
    tokio::time::timeout(Duration::from_secs(40), async {
        let home = tempfile::tempdir().expect("tempdir");
        let alice_db = expected_memory_db(home.path(), "alice");
        let bob_db = expected_memory_db(home.path(), "bob");

        // Two servers, same HOME, different agent ids — they must land in
        // distinct memory.db files with no cross-write.
        let mut alice_child = spawn_server(home.path(), "alice");
        let alice_stdout = alice_child.stdout.take().expect("alice stdout");
        let alice_stdin = alice_child.stdin.take().expect("alice stdin");
        let alice_client = serve_client((), (alice_stdout, alice_stdin))
            .await
            .expect("alice handshake");

        let mut bob_child = spawn_server(home.path(), "bob");
        let bob_stdout = bob_child.stdout.take().expect("bob stdout");
        let bob_stdin = bob_child.stdin.take().expect("bob stdin");
        let bob_client = serve_client((), (bob_stdout, bob_stdin))
            .await
            .expect("bob handshake");

        let alice_id = call_log_progress(
            &alice_client,
            "alice's entry",
            Some("only alice should ever see this"),
        )
        .await;
        let bob_id = call_log_progress(
            &bob_client,
            "bob's entry",
            Some("only bob should ever see this"),
        )
        .await;

        drop(alice_client);
        drop(bob_client);
        let _ = alice_child.kill().await;
        let _ = bob_child.kill().await;
        let _ = alice_child.wait().await;
        let _ = bob_child.wait().await;

        // Each agent's file exists in its own ~/.opencrab/agents/<id>/ dir
        // and carries only its own row.
        assert!(alice_db.exists(), "alice memory.db must exist");
        assert!(bob_db.exists(), "bob memory.db must exist");
        let alice_rows = dump_log_rows(&alice_db);
        let bob_rows = dump_log_rows(&bob_db);
        assert_eq!(alice_rows.len(), 1, "alice rows = {alice_rows:?}");
        assert_eq!(bob_rows.len(), 1, "bob rows = {bob_rows:?}");
        assert_eq!(alice_rows[0].0, alice_id);
        assert_eq!(alice_rows[0].1, "alice's entry");
        assert_eq!(
            alice_rows[0].2.as_deref(),
            Some("only alice should ever see this")
        );
        assert_eq!(bob_rows[0].0, bob_id);
        assert_eq!(bob_rows[0].1, "bob's entry");
        assert_eq!(
            bob_rows[0].2.as_deref(),
            Some("only bob should ever see this")
        );
    })
    .await
    .expect("smoke #3 timed out");
}

/// P6-hang probe — the "agent → MCP tool call → return" round-trip must
/// RETURN (not spin) even when a concurrent writer holds the memory.db write
/// lock.
///
/// Why this case: `log_progress` runs as a *blocking* synchronous SQLite call
/// inside the server's async `call_tool` (`server.rs`), and the server runs on
/// a `current_thread` tokio runtime (`main.rs` `flavor = "current_thread"`). A
/// held write lock is exactly what a wedged concurrent writer (an ingest pass
/// or a distiller transaction) would create. This test forces that contention
/// and asserts the tool call still comes back within a hard outer bound.
///
///   * If this ever TIMES OUT, the hang is reproduced at the *server boundary*
///     (the surgical fix would be to wrap the backend call in `spawn_blocking`
///     so it can't stall the single executor thread).
///   * If it RETURNS (BUSY/err or ok), the server side does not hang even under
///     write contention — the spin lives elsewhere (codex-side MCP client, or a
///     writer holding the lock with no bound).
#[tokio::test]
async fn stdio_log_progress_returns_even_under_write_lock_contention() {
    tokio::time::timeout(Duration::from_secs(40), async {
        let home = tempfile::tempdir().expect("tempdir");
        let agent = "contention";
        let db = expected_memory_db(home.path(), agent);

        let mut child = spawn_server(home.path(), agent);
        let stdout = child.stdout.take().expect("stdout");
        let stdin = child.stdin.take().expect("stdin");
        let client = serve_client((), (stdout, stdin)).await.expect("handshake");

        // 1) One successful call so memory.db + schema exist on disk.
        let _ = call_log_progress(&client, "warm up", None).await;
        assert!(db.exists(), "db should exist after first write");

        // 2) Grab the WAL write lock from an independent connection and hold it.
        //    busy_timeout(0): take the lock instantly and keep it until we drop
        //    the connection — the SERVER is the side that must contend.
        let lock_conn = Connection::open(&db).expect("open lock conn");
        lock_conn
            .busy_timeout(Duration::from_millis(0))
            .expect("busy_timeout");
        lock_conn
            .execute_batch("BEGIN IMMEDIATE")
            .expect("acquire write lock");

        // 3) Call log_progress through the server WHILE the lock is held. The
        //    server's open()/migrate()/INSERT must contend; with its 5 s
        //    busy_timeout it should surface a BUSY error and RETURN — not spin.
        let mut args = serde_json::Map::new();
        args.insert("summary".to_string(), Value::from("under contention"));
        let started = std::time::Instant::now();
        let call = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "log_progress".into(),
                arguments: Some(args),
                task: None,
            })
            .await;
        let elapsed = started.elapsed();

        // The contract under test is "it returns", regardless of ok vs error.
        // A BUSY surfaces as Err; a (surprising) success is Ok. Either proves
        // there is no forever-hang at the server boundary. A real hang would
        // have tripped the outer 40 s guard and failed the test instead.
        match &call {
            Ok(_) => eprintln!("[p6-hang] contended call returned Ok in {elapsed:?}"),
            Err(e) => eprintln!("[p6-hang] contended call returned Err in {elapsed:?}: {e}"),
        }

        // Release the lock and confirm the server still serves a later call —
        // i.e. the contention did not wedge the server for good.
        drop(lock_conn);
        let after = call_log_progress(&client, "after lock released", None).await;
        assert!(after > 0, "server must keep serving after contention clears");

        drop(client);
        let _ = child.kill().await;
        let _ = child.wait().await;
    })
    .await
    .expect("P6-hang contention probe timed out — round-trip hung at the SERVER boundary");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn spawn_server(home: &Path, agent_id: &str) -> tokio::process::Child {
    tokio::process::Command::new(BINARY)
        .args(["--agent-id", agent_id])
        // HOME-overrides keep `paths::home_dir()` (which the server consults
        // to compute `~/.opencrab/agents/<id>/memory.db`) pointed at the
        // tempdir, not the developer's real home.
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn opencrab-memory-mcp")
}

async fn call_log_progress(
    client: &RunningService<RoleClient, ()>,
    summary: &str,
    detail: Option<&str>,
) -> i64 {
    let mut args = serde_json::Map::new();
    args.insert("summary".to_string(), Value::from(summary));
    if let Some(d) = detail {
        args.insert("detail".to_string(), Value::from(d));
    }
    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "log_progress".into(),
            arguments: Some(args),
            task: None,
        })
        .await
        .expect("tools/call log_progress");
    result.structured_content.expect("structured content")["id"]
        .as_i64()
        .expect("id is integer")
}

/// Open `db` and return `(id, summary, detail)` for every row in the `log`
/// table, ordered by id. The independent-read path the smoke uses to verify
/// the mcp server's writes actually landed on disk.
fn dump_log_rows(db: &Path) -> Vec<(i64, String, Option<String>)> {
    let conn = Connection::open(db).expect("open memory.db");
    let mut stmt = conn
        .prepare("SELECT id, summary, detail FROM log ORDER BY id")
        .expect("prepare");
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .map(Result::unwrap)
        .collect()
}
