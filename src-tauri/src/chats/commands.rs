// Tauri command for the S6-1 `chats` read-DB.
//
// Thin wrapper, mirroring `tasks::commands`: resolve `workspace_id` → workspace
// root via `AppState`, then delegate to `store::list_thread_chats_at_path`.
// The query itself (recipient-inclusive `thread_id=? OR recipient=?`,
// `(ts, seq)` order, pagination) is covered by `chats::store` tests; this layer
// only resolves the path and maps the error.
//
// S6-3a registers the command (wire is live) but the FRONTEND does not call it
// yet — conversation rendering switches to it in S6-3b. So nothing reads
// `chats` in production today, which is why S6-3a's user-input writes are safe.

use std::path::PathBuf;

use tauri::State;

use crate::state::AppState;

use super::store::{list_thread_chats_at_path, ChatRow};

/// Default page size when the caller omits `limit`. The frontend (S6-3b) will
/// pass explicit paging values; this is a safety cap for the no-arg case.
const DEFAULT_CHATS_LIMIT: i64 = 200;

async fn workspace_root(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let workspaces = state.workspaces.lock().await;
    let entry = workspaces
        .get(workspace_id)
        .ok_or_else(|| format!("workspace not found: {workspace_id}"))?;
    Ok(PathBuf::from(&entry.path))
}

/// Read one conversation from `chats` for rendering: rows under `thread_id`
/// plus rows addressed to `agent_id` (inbound Dev↔Dev), ascending by
/// `(ts, seq)`. `limit` caps the page (default `DEFAULT_CHATS_LIMIT`);
/// `before_ts` pages backward (`ts < before_ts`).
#[tauri::command]
pub(crate) async fn list_thread_chats(
    workspace_id: String,
    thread_id: String,
    agent_id: String,
    limit: Option<i64>,
    before_ts: Option<String>,
    state: State<'_, AppState>,
) -> Result<Vec<ChatRow>, String> {
    let root = workspace_root(&state, &workspace_id).await?;
    list_thread_chats_at_path(
        &root,
        &workspace_id,
        &thread_id,
        &agent_id,
        limit.unwrap_or(DEFAULT_CHATS_LIMIT),
        before_ts.as_deref(),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chats::store::{append_chat_at_path, ChatKind, NewChat};
    use tempfile::TempDir;

    fn fresh() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().to_path_buf();
        (dir, p)
    }

    // The `#[tauri::command]` wrapper needs a Tauri `AppState` to drive, which
    // this codebase does not unit-test (cf. `tasks::commands`). We instead
    // exercise the exact delegate the command calls —
    // `list_thread_chats_at_path` — over a `user_input` row written the
    // S6-3a way, proving the user-input producer shape and the read path
    // round-trip across BOTH query branches (the query internals are
    // `chats::store`-tested).
    #[test]
    fn user_input_row_round_trips_through_the_read_delegate() {
        let (_t, root) = fresh();
        // A human message to agent "pm" lands under pm's thread with
        // recipient=pm (what `record_user_chat_best_effort` writes).
        append_chat_at_path(
            &root,
            NewChat {
                workspace_id: "ws-1".into(),
                team_id: "team-1".into(),
                thread_id: Some("thread-pm".into()),
                sender: "user".into(),
                recipient: Some("pm".into()),
                role: None,
                kind: ChatKind::UserInput,
                content: "build me an app".into(),
                ts: "2026-06-05T10:00:00Z".into(),
            },
        )
        .unwrap();

        // Branch 1 — by thread (rendering pm's conversation).
        let by_thread =
            list_thread_chats_at_path(&root, "ws-1", "thread-pm", "pm", 200, None).unwrap();
        assert_eq!(by_thread.len(), 1);
        assert_eq!(by_thread[0].kind, "user_input");
        assert_eq!(by_thread[0].sender, "user");
        assert_eq!(by_thread[0].recipient.as_deref(), Some("pm"));
        assert_eq!(by_thread[0].content, "build me an app");

        // Branch 2 — by recipient (the row is addressed to pm) from a
        // different thread id resolves the same row via the OR.
        let by_recipient =
            list_thread_chats_at_path(&root, "ws-1", "some-other-thread", "pm", 200, None).unwrap();
        assert_eq!(by_recipient.len(), 1);
        assert_eq!(by_recipient[0].content, "build me an app");
    }
}
