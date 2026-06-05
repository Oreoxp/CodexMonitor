// Unit tests for the S6-1 chats store.
//
// Determinism: tests pass fixed `ts` strings (not wall-clock), so the
// same-`ts` → `seq` ordering contract is exercised without racing the clock.

use std::path::PathBuf;

use tempfile::TempDir;

use super::store::{self, append_chat_at_path, list_thread_chats_at_path, ChatKind, NewChat};

fn fresh_workspace() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    (dir, path)
}

/// Build a `NewChat` with the shared workspace/team and no role.
fn chat(
    thread_id: Option<&str>,
    sender: &str,
    recipient: Option<&str>,
    kind: ChatKind,
    content: &str,
    ts: &str,
) -> NewChat {
    NewChat {
        workspace_id: "ws-1".into(),
        team_id: "team-1".into(),
        thread_id: thread_id.map(str::to_string),
        sender: sender.into(),
        recipient: recipient.map(str::to_string),
        role: None,
        kind,
        content: content.into(),
        ts: ts.into(),
    }
}

#[test]
fn append_then_read_back() {
    let (_tmp, root) = fresh_workspace();
    let inserted = append_chat_at_path(
        &root,
        chat(
            Some("thread-A"),
            "alice",
            Some("bob"),
            ChatKind::SendMessage,
            "hello bob",
            "2026-06-05T10:00:00Z",
        ),
    )
    .expect("append");
    assert!(inserted.id > 0, "id assigned by sqlite");
    assert_eq!(inserted.seq, 0, "first row in its ts bucket");

    let rows =
        list_thread_chats_at_path(&root, "ws-1", "thread-A", "alice", 50, None).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], inserted);
    assert_eq!(rows[0].content, "hello bob");
    assert_eq!(rows[0].kind, "send_message");
    assert_eq!(rows[0].recipient.as_deref(), Some("bob"));
}

#[test]
fn read_is_recipient_inclusive() {
    let (_tmp, root) = fresh_workspace();
    // Row 1: lives under A's own thread.
    append_chat_at_path(
        &root,
        chat(
            Some("thread-A"),
            "alice",
            None,
            ChatKind::AgentMessage,
            "alice narrates",
            "2026-06-05T10:00:00Z",
        ),
    )
    .unwrap();
    // Row 2: a DIFFERENT thread, but addressed TO alice (inbound Dev↔Dev).
    append_chat_at_path(
        &root,
        chat(
            Some("thread-B"),
            "bob",
            Some("alice"),
            ChatKind::SendMessage,
            "bob to alice",
            "2026-06-05T10:00:01Z",
        ),
    )
    .unwrap();
    // Row 3: unrelated thread + unrelated recipient — must NOT appear for A.
    append_chat_at_path(
        &root,
        chat(
            Some("thread-C"),
            "carol",
            Some("dave"),
            ChatKind::SendMessage,
            "carol to dave",
            "2026-06-05T10:00:02Z",
        ),
    )
    .unwrap();

    let rows = list_thread_chats_at_path(&root, "ws-1", "thread-A", "alice", 50, None).unwrap();
    let contents: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(contents, vec!["alice narrates", "bob to alice"]);
}

#[test]
fn same_ts_ordered_by_seq() {
    let (_tmp, root) = fresh_workspace();
    let ts = "2026-06-05T10:00:00Z";
    let a = append_chat_at_path(
        &root,
        chat(
            Some("t"),
            "alice",
            None,
            ChatKind::AgentMessage,
            "first",
            ts,
        ),
    )
    .unwrap();
    let b = append_chat_at_path(
        &root,
        chat(
            Some("t"),
            "alice",
            None,
            ChatKind::AgentMessage,
            "second",
            ts,
        ),
    )
    .unwrap();
    let c = append_chat_at_path(
        &root,
        chat(
            Some("t"),
            "alice",
            None,
            ChatKind::AgentMessage,
            "third",
            ts,
        ),
    )
    .unwrap();
    assert_eq!(
        (a.seq, b.seq, c.seq),
        (0, 1, 2),
        "seq increments within one ts"
    );

    let rows = list_thread_chats_at_path(&root, "ws-1", "t", "alice", 50, None).unwrap();
    let contents: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(contents, vec!["first", "second", "third"]);
    let seqs: Vec<i64> = rows.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

#[test]
fn limit_and_before_ts_paginate() {
    let (_tmp, root) = fresh_workspace();
    // 5 rows, distinct ascending ts: m0@00 .. m4@04.
    for (i, hms) in ["10:00:00", "10:00:01", "10:00:02", "10:00:03", "10:00:04"]
        .iter()
        .enumerate()
    {
        let full_ts = format!("2026-06-05T{hms}Z");
        append_chat_at_path(
            &root,
            chat(
                Some("t"),
                "alice",
                None,
                ChatKind::AgentMessage,
                &format!("m{i}"),
                &full_ts,
            ),
        )
        .unwrap();
    }

    // Page 1: newest 2, returned ascending.
    let page1 = list_thread_chats_at_path(&root, "ws-1", "t", "alice", 2, None).unwrap();
    let p1: Vec<&str> = page1.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(p1, vec!["m3", "m4"]);

    // Page 2: the 2 rows older than the oldest shown (m3 @ 10:00:03).
    let oldest_shown_ts = page1.first().unwrap().ts.clone();
    let page2 =
        list_thread_chats_at_path(&root, "ws-1", "t", "alice", 2, Some(&oldest_shown_ts)).unwrap();
    let p2: Vec<&str> = page2.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(p2, vec!["m1", "m2"]);
}

#[test]
fn init_schema_idempotent() {
    let (_tmp, root) = fresh_workspace();
    // First open creates the table + indices; seed a row.
    append_chat_at_path(
        &root,
        chat(
            Some("t"),
            "alice",
            None,
            ChatKind::UserInput,
            "hi",
            "2026-06-05T10:00:00Z",
        ),
    )
    .unwrap();
    // Re-open (re-runs init_schema) + an explicit second init — must not error
    // or lose the row.
    let conn = store::open_and_init(&root).unwrap();
    store::init_schema(&conn).unwrap();
    let rows = store::list_thread_chats(&conn, "ws-1", "t", "alice", 50, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "hi");
    assert_eq!(rows[0].kind, "user_input");
}
