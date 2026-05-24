// Phase 4 Step 8 — codex_spawn tests.

use std::fs;
use std::path::Path;

use tempfile::tempdir;

use super::{
    agent_team_session_dir, build_memory_mcp_server_entry, ensure_agent_team_session_dir,
    ensure_team_session_dir, project_hash, team_session_dir_for_workspace, CodexSpawnError,
    PROJECT_HASH_LEN_HEX,
};

// ---------------------------------------------------------------------------
// project_hash
// ---------------------------------------------------------------------------

#[test]
fn project_hash_is_deterministic_for_same_path() {
    let cwd = tempdir().unwrap();
    let a = project_hash(cwd.path()).unwrap();
    let b = project_hash(cwd.path()).unwrap();
    assert_eq!(a, b);
}

#[test]
fn project_hash_length_matches_constant() {
    let cwd = tempdir().unwrap();
    let hash = project_hash(cwd.path()).unwrap();
    assert_eq!(hash.len(), PROJECT_HASH_LEN_HEX);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn project_hash_differs_for_different_paths() {
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    assert_ne!(
        project_hash(a.path()).unwrap(),
        project_hash(b.path()).unwrap()
    );
}

#[test]
fn project_hash_collapses_relative_and_absolute_to_same_value() {
    // canonicalize resolves both to the same absolute path, so the hash
    // must be identical regardless of which form the caller supplied.
    let cwd = tempdir().unwrap();
    let abs = cwd.path();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(abs).unwrap();
    let rel_form = project_hash(Path::new(".")).unwrap();
    std::env::set_current_dir(original).unwrap();
    let abs_form = project_hash(abs).unwrap();
    assert_eq!(rel_form, abs_form);
}

#[cfg(unix)]
#[test]
fn project_hash_collapses_symlink_to_target() {
    let target = tempdir().unwrap();
    let link_parent = tempdir().unwrap();
    let link = link_parent.path().join("alias");
    std::os::unix::fs::symlink(target.path(), &link).unwrap();
    let target_hash = project_hash(target.path()).unwrap();
    let link_hash = project_hash(&link).unwrap();
    assert_eq!(target_hash, link_hash);
}

#[test]
fn project_hash_errors_on_nonexistent_path() {
    let bogus = Path::new("/this/path/should/not/exist/opencrab-step8");
    let err = project_hash(bogus).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("canonicalize"));
}

// ---------------------------------------------------------------------------
// team_session_dir_for_workspace
// ---------------------------------------------------------------------------

#[test]
fn team_session_dir_layout_is_user_dir_slash_team_sessions_slash_hash() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let dir = team_session_dir_for_workspace(user.path(), cwd.path()).unwrap();
    let hash = project_hash(cwd.path()).unwrap();
    let expected = user.path().join("team_sessions").join(&hash);
    assert_eq!(dir, expected);
}

#[test]
fn team_session_dir_changes_when_cwd_changes() {
    let user = tempdir().unwrap();
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    let dir_a = team_session_dir_for_workspace(user.path(), a.path()).unwrap();
    let dir_b = team_session_dir_for_workspace(user.path(), b.path()).unwrap();
    assert_ne!(dir_a, dir_b);
}

#[test]
fn team_session_dir_does_not_touch_filesystem() {
    // team_session_dir_for_workspace is path-only — it canonicalizes
    // cwd but does not create the destination dir.
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let dir = team_session_dir_for_workspace(user.path(), cwd.path()).unwrap();
    assert!(!dir.exists(), "should not have created {}", dir.display());
}

// ---------------------------------------------------------------------------
// ensure_team_session_dir
// ---------------------------------------------------------------------------

#[test]
fn ensure_team_session_dir_creates_path() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let dir = ensure_team_session_dir(user.path(), cwd.path()).unwrap();
    assert!(dir.exists() && dir.is_dir());
}

#[test]
fn ensure_team_session_dir_is_idempotent() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let first = ensure_team_session_dir(user.path(), cwd.path()).unwrap();
    let second = ensure_team_session_dir(user.path(), cwd.path()).unwrap();
    assert_eq!(first, second);
    assert!(first.exists());
}

#[test]
fn ensure_team_session_dir_preserves_existing_content() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let dir = ensure_team_session_dir(user.path(), cwd.path()).unwrap();
    // Drop a sentinel inside; second ensure call must leave it intact.
    let sentinel = dir.join("sentinel.jsonl");
    fs::write(&sentinel, "preserved").unwrap();
    let dir_again = ensure_team_session_dir(user.path(), cwd.path()).unwrap();
    assert_eq!(dir, dir_again);
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "preserved");
}

#[test]
fn ensure_team_session_dir_creates_parent_when_missing() {
    // `<user>/team_sessions` does not exist beforehand — ensure must
    // mkdir -p all the way.
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let parent = user.path().join("team_sessions");
    assert!(!parent.exists());
    let dir = ensure_team_session_dir(user.path(), cwd.path()).unwrap();
    assert!(parent.exists());
    assert!(dir.exists());
}

// ---------------------------------------------------------------------------
// Step 8-fix — agent_team_session_dir (per-agent path)
// ---------------------------------------------------------------------------

#[test]
fn agent_team_session_dir_layout_matches_spec() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let dir = agent_team_session_dir(user.path(), "agent_alice", "team_demo", cwd.path()).unwrap();
    let hash = project_hash(cwd.path()).unwrap();
    let expected = user
        .path()
        .join("agents")
        .join("agent_alice")
        .join("team_sessions")
        .join("team_demo")
        .join(hash);
    assert_eq!(dir, expected);
}

#[test]
fn agent_team_session_dir_changes_when_any_input_changes() {
    let user = tempdir().unwrap();
    let cwd_a = tempdir().unwrap();
    let cwd_b = tempdir().unwrap();
    let base = agent_team_session_dir(user.path(), "alice", "team_demo", cwd_a.path()).unwrap();
    let other_agent =
        agent_team_session_dir(user.path(), "bob", "team_demo", cwd_a.path()).unwrap();
    let other_team =
        agent_team_session_dir(user.path(), "alice", "team_demo2", cwd_a.path()).unwrap();
    let other_cwd =
        agent_team_session_dir(user.path(), "alice", "team_demo", cwd_b.path()).unwrap();
    assert_ne!(base, other_agent);
    assert_ne!(base, other_team);
    assert_ne!(base, other_cwd);
}

#[test]
fn agent_team_session_dir_rejects_empty_agent_id() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let err = agent_team_session_dir(user.path(), "", "team_demo", cwd.path()).unwrap_err();
    match err {
        CodexSpawnError::InvalidId { kind, .. } => assert_eq!(kind, "agent_id"),
        other => panic!("expected InvalidId, got {other}"),
    }
}

#[test]
fn agent_team_session_dir_rejects_path_separator_in_agent_id() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    for evil in ["a/b", "a\\b"] {
        let err = agent_team_session_dir(user.path(), evil, "team", cwd.path()).unwrap_err();
        assert!(
            matches!(
                err,
                CodexSpawnError::InvalidId {
                    kind: "agent_id",
                    ..
                }
            ),
            "expected InvalidId for {evil}, got {err}"
        );
    }
}

#[test]
fn agent_team_session_dir_rejects_dot_and_dotdot() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    for evil in [".", ".."] {
        let err = agent_team_session_dir(user.path(), evil, "team", cwd.path()).unwrap_err();
        assert!(
            matches!(err, CodexSpawnError::InvalidId { .. }),
            "{evil}: {err}"
        );
    }
}

#[test]
fn agent_team_session_dir_rejects_invalid_team_id() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let err = agent_team_session_dir(user.path(), "alice", "../escape", cwd.path()).unwrap_err();
    match err {
        CodexSpawnError::InvalidId { kind, .. } => assert_eq!(kind, "team_id"),
        other => panic!("expected InvalidId team_id, got {other}"),
    }
}

#[test]
fn ensure_agent_team_session_dir_creates_full_path() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    // None of the intermediate dirs exist.
    assert!(!user.path().join("agents").exists());
    let dir =
        ensure_agent_team_session_dir(user.path(), "agent_alice", "team_demo", cwd.path()).unwrap();
    assert!(dir.exists() && dir.is_dir());
    // Whole chain materialized.
    assert!(user
        .path()
        .join("agents/agent_alice/team_sessions/team_demo")
        .exists());
}

#[test]
fn ensure_agent_team_session_dir_is_idempotent() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let a = ensure_agent_team_session_dir(user.path(), "alice", "team", cwd.path()).unwrap();
    let b = ensure_agent_team_session_dir(user.path(), "alice", "team", cwd.path()).unwrap();
    assert_eq!(a, b);

    // Drop a sentinel; second call must leave it alone.
    let sentinel = a.join("rollout.jsonl");
    fs::write(&sentinel, "preserved").unwrap();
    let _ = ensure_agent_team_session_dir(user.path(), "alice", "team", cwd.path()).unwrap();
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "preserved");
}

#[test]
fn ensure_agent_team_session_dir_propagates_validation_error() {
    let user = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let err =
        ensure_agent_team_session_dir(user.path(), "evil/agent", "team", cwd.path()).unwrap_err();
    assert!(matches!(
        err,
        CodexSpawnError::InvalidId {
            kind: "agent_id",
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Phase 5 Step 2 — build_memory_mcp_server_entry
// ---------------------------------------------------------------------------

#[test]
fn build_memory_mcp_server_entry_bakes_agent_id_and_memory_dir() {
    let binary = Path::new("/opt/bin/opencrab-memory-mcp");
    let entry = build_memory_mcp_server_entry(binary, "alice", Path::new("/ws"))
        .expect("entry builds for a valid agent id");

    // `command` is the resolved binary path.
    assert_eq!(entry["command"], binary.display().to_string());

    // `args` carry the agent id + the agent's own project-memory directory,
    // so each agent's spawned server is scoped to exactly its own memory.
    let expected_dir = Path::new("/ws")
        .join(".opencrab")
        .join("agents")
        .join("alice")
        .join("project-memory")
        .display()
        .to_string();
    assert_eq!(
        entry["args"],
        serde_json::json!(["--agent-id", "alice", "--memory-dir", expected_dir]),
    );
}

#[test]
fn build_memory_mcp_server_entry_scopes_distinct_agents_to_distinct_dirs() {
    let binary = Path::new("/opt/bin/opencrab-memory-mcp");
    let alice = build_memory_mcp_server_entry(binary, "alice", Path::new("/ws")).unwrap();
    let bob = build_memory_mcp_server_entry(binary, "bob", Path::new("/ws")).unwrap();
    assert_ne!(alice["args"], bob["args"]);
}

#[test]
fn build_memory_mcp_server_entry_rejects_path_traversal_agent_id() {
    let binary = Path::new("/opt/bin/opencrab-memory-mcp");
    for evil in ["../escape", "a/b", "..", "."] {
        let err = build_memory_mcp_server_entry(binary, evil, Path::new("/ws")).unwrap_err();
        assert!(
            matches!(
                err,
                CodexSpawnError::InvalidId {
                    kind: "agent_id",
                    ..
                }
            ),
            "expected InvalidId for {evil:?}, got {err}",
        );
    }
}
