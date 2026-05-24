// Phase 5 Step 2 — stdio smoke test for the `opencrab-memory-mcp` binary.
//
// Spawns the real built binary and drives a full MCP session over stdio with
// an rmcp client: the `initialize` handshake (done by `serve_client`), then
// `tools/list` and a `tools/call` for `memory_search`. This proves the
// binary speaks the same protocol Codex will speak to it — end to end, over
// real pipes.

use std::process::Stdio;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::serve_client;

#[tokio::test]
async fn stdio_smoke_lists_tools_and_searches() {
    // A broken binary would hang the handshake; cap the whole exchange.
    tokio::time::timeout(Duration::from_secs(20), run_smoke())
        .await
        .expect("opencrab-memory-mcp stdio smoke test timed out");
}

async fn run_smoke() {
    // The temp dir stands in for the agent's `project-memory/` directory;
    // seed one journal day so `memory_search` has something to find.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("2026-05-20.md"),
        "Adopted the alpha rollout plan after the review.\n",
    )
    .expect("seed journal day");

    let binary = env!("CARGO_BIN_EXE_opencrab-memory-mcp");
    let mut child = tokio::process::Command::new(binary)
        .args([
            "--agent-id",
            "smoke",
            "--memory-dir",
            dir.path().to_str().expect("utf-8 temp path"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn opencrab-memory-mcp");
    let stdout = child.stdout.take().expect("child stdout");
    let stdin = child.stdin.take().expect("child stdin");

    // `serve_client` performs the MCP `initialize` handshake on connect — a
    // successful return means the binary handshook over stdio.
    let client = serve_client((), (stdout, stdin))
        .await
        .expect("mcp initialize handshake");

    // tools/list — both memory tools are advertised.
    let tools = client.list_all_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert!(names.contains(&"memory_search"), "tools were {names:?}");
    assert!(names.contains(&"memory_get"), "tools were {names:?}");

    // tools/call memory_search — the seeded day comes back as a ranked hit.
    let mut arguments = serde_json::Map::new();
    arguments.insert("query".to_string(), serde_json::json!("alpha"));
    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "memory_search".into(),
            arguments: Some(arguments),
            task: None,
        })
        .await
        .expect("tools/call memory_search");

    let structured = result
        .structured_content
        .expect("memory_search returns structured content");
    assert_eq!(structured["count"], 1, "structured = {structured}");
    assert_eq!(structured["hits"][0]["date"], "2026-05-20");
    assert!(
        structured["hits"][0]["snippet"]
            .as_str()
            .unwrap_or_default()
            .contains("alpha rollout"),
        "snippet = {}",
        structured["hits"][0]["snippet"],
    );

    drop(client);
    let _ = child.kill().await;
}
