// Phase 4 Step 1 — bootstrap module tests.
//
// Test isolation: most tests use `tempfile::TempDir` as a synthetic
// workspace root and call the `*_at` helpers directly, so they never write
// to the real `~/.opencrab/` and never touch process env. The one
// exception is `cleanup_workspace_state_spares_machine_global_identity`
// (storage Step 2 regression guard): `cleanup_workspace_state` is reached
// only via the `cwd`-driven public API, and the test points `$HOME` at a
// synthetic user layer to prove the machine-global files survive — it does
// so under the crate-wide `crate::paths::ENV_LOCK` so it stays race-free
// against the other env-mutating tests in the binary.

use std::fs;
use std::path::Path;

use tempfile::tempdir;

use super::*;
use crate::team_config::types::{AgentConfig, ToolsPreset};

fn agent(id: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_string(),
        name: id.to_string(),
        role: "pm".to_string(),
        model: "gpt-5".to_string(),
        system_prompt_template: String::new(),
        tools_preset: ToolsPreset::Readonly,
    }
}

fn assert_dir(p: &Path) {
    assert!(p.exists(), "expected dir to exist: {}", p.display());
    assert!(p.is_dir(), "expected dir not file: {}", p.display());
}

fn assert_file(p: &Path, expected: &str) {
    assert!(p.exists(), "expected file to exist: {}", p.display());
    let bytes = fs::read(p).unwrap();
    assert_eq!(
        bytes,
        expected.as_bytes(),
        "file content mismatch at {}",
        p.display()
    );
}

// -- ensure_user_layer ----------------------------------------------------

#[test]
fn user_layer_fresh_state_creates_dirs_and_templates() {
    let home = tempdir().unwrap();
    let roster = [agent("alice"), agent("bob")];

    ensure_user_layer_at(home.path(), &roster).unwrap();

    assert_dir(&home.path().join("agents"));
    for id in ["alice", "bob"] {
        let agent_dir = home.path().join("agents").join(id);
        assert_dir(&agent_dir);
        assert_file(&agent_dir.join("SOUL.md"), SOUL_TEMPLATE);
        assert_file(&agent_dir.join("IDENTITY.md"), IDENTITY_TEMPLATE);
        assert_file(&agent_dir.join("USER.md"), USER_TEMPLATE);
        assert_file(&agent_dir.join("MEMORY.md"), MEMORY_TEMPLATE);
    }
}

#[test]
fn user_layer_does_not_create_codex_home_or_team_sessions_subtrees() {
    // Phase 4 Step 1-patch (2026-05-18): the per-agent `codex-home/`
    // subtree is intentionally dropped (decision flipped to shared
    // `~/.opencrab/` CODEX_HOME). `team_sessions/<team_id>/<project_hash>/`
    // is per-spawn Step-8 territory — bootstrap MUST NOT create it.
    let home = tempdir().unwrap();
    let roster = [agent("alice"), agent("bob")];

    ensure_user_layer_at(home.path(), &roster).unwrap();

    for id in ["alice", "bob"] {
        let agent_dir = home.path().join("agents").join(id);
        assert!(
            !agent_dir.join("codex-home").exists(),
            "codex-home/ should not be created by ensure_user_layer (per Step 1-patch)"
        );
        assert!(
            !agent_dir.join("team_sessions").exists(),
            "team_sessions/ is Step 8 per-spawn; bootstrap must not create it"
        );
    }
}

#[test]
fn user_layer_preserves_pre_existing_content() {
    let home = tempdir().unwrap();
    let roster = [agent("alice")];

    // Pre-seed a non-empty SOUL.md AND an arbitrary orphan dir/file that
    // pre-dates this bootstrap (e.g. leftover from an earlier Step 1 run
    // that did create `codex-home/`). bootstrap must not delete it.
    fs::create_dir_all(home.path().join("agents/alice")).unwrap();
    let custom_soul = "# Soul\n\nI am Alice and I have notes.\n";
    fs::write(home.path().join("agents/alice/SOUL.md"), custom_soul).unwrap();
    // Pre-Step-1-patch orphan: this directory used to be created by
    // `ensure_user_layer`. Pin "we leave it alone" rather than "we
    // remove it" — see Step 1-patch spec ("no migration / cleanup").
    fs::create_dir_all(home.path().join("agents/alice/codex-home")).unwrap();
    let orphan_marker = "{\"placeholder\":true}";
    fs::write(
        home.path().join("agents/alice/codex-home/orphan.json"),
        orphan_marker,
    )
    .unwrap();

    ensure_user_layer_at(home.path(), &roster).unwrap();

    assert_file(&home.path().join("agents/alice/SOUL.md"), custom_soul);
    // Orphan stays put — bootstrap does NOT clean it up.
    assert_file(
        &home.path().join("agents/alice/codex-home/orphan.json"),
        orphan_marker,
    );
    // Missing templates should be seeded alongside the preserved one.
    assert_file(
        &home.path().join("agents/alice/IDENTITY.md"),
        IDENTITY_TEMPLATE,
    );
    assert_file(&home.path().join("agents/alice/USER.md"), USER_TEMPLATE);
    assert_file(&home.path().join("agents/alice/MEMORY.md"), MEMORY_TEMPLATE);
}

#[test]
fn user_layer_empty_roster_creates_root_only() {
    let home = tempdir().unwrap();

    ensure_user_layer_at(home.path(), &[]).unwrap();

    assert_dir(home.path());
    assert_dir(&home.path().join("agents"));
    let agents_entries: Vec<_> = fs::read_dir(home.path().join("agents"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        agents_entries.is_empty(),
        "agents/ should be empty for empty roster, got {agents_entries:?}"
    );
}

#[test]
fn user_layer_three_agents_isolated() {
    let home = tempdir().unwrap();
    let roster = [agent("alice"), agent("bob"), agent("carol")];

    ensure_user_layer_at(home.path(), &roster).unwrap();

    for id in ["alice", "bob", "carol"] {
        let dir = home.path().join("agents").join(id);
        assert_dir(&dir);
        assert_file(&dir.join("SOUL.md"), SOUL_TEMPLATE);
    }
}

#[test]
fn user_layer_template_bytes_match_constants() {
    let home = tempdir().unwrap();
    let roster = [agent("alice")];

    ensure_user_layer_at(home.path(), &roster).unwrap();

    // Hard guard against accidental whitespace edits to the seed text.
    assert_eq!(
        fs::read_to_string(home.path().join("agents/alice/SOUL.md")).unwrap(),
        SOUL_TEMPLATE,
    );
    assert_eq!(
        fs::read_to_string(home.path().join("agents/alice/IDENTITY.md")).unwrap(),
        IDENTITY_TEMPLATE,
    );
    assert_eq!(
        fs::read_to_string(home.path().join("agents/alice/USER.md")).unwrap(),
        USER_TEMPLATE,
    );
    assert_eq!(
        fs::read_to_string(home.path().join("agents/alice/MEMORY.md")).unwrap(),
        MEMORY_TEMPLATE,
    );
}

// -- ensure_project_layer -------------------------------------------------

#[test]
fn project_layer_fresh_state_creates_dirs_and_templates() {
    let cwd = tempdir().unwrap();
    let roster = [agent("alice"), agent("bob")];
    let team_id = "team_demo";

    ensure_project_layer_at(&project_data_dir(cwd.path()), team_id, &roster).unwrap();

    let root = cwd.path().join(OPENCRAB_DIR);
    assert_dir(&root);
    for id in ["alice", "bob"] {
        let dir = root.join("agents").join(id);
        assert_dir(&dir);
        assert_file(&dir.join("KANBAN.md"), KANBAN_TEMPLATE);
        assert_dir(&dir.join("project-memory"));
    }
    assert_file(&root.join("team/decisions.md"), DECISIONS_TEMPLATE);
    let events = root.join("teams").join(team_id).join("events.jsonl");
    assert!(events.exists());
    assert_eq!(fs::metadata(&events).unwrap().len(), 0);
}

#[test]
fn project_layer_leaves_existing_state_sqlite_and_team_json_alone() {
    let cwd = tempdir().unwrap();
    let roster = [agent("alice")];
    let team_id = "team_demo";

    // Simulate a workspace that already finished Phase 3: state.sqlite +
    // team.json exist under .opencrab/. ensure_project_layer must extend the
    // tree around them, never touching them.
    let root = cwd.path().join(OPENCRAB_DIR);
    fs::create_dir_all(&root).unwrap();
    let fake_sqlite = b"SQLITE format 3\0placeholder";
    fs::write(root.join("state.sqlite"), fake_sqlite).unwrap();
    let fake_team_json = "{\"id\":\"team_demo\",\"agents\":[]}";
    fs::write(root.join("team.json"), fake_team_json).unwrap();

    ensure_project_layer_at(&root, team_id, &roster).unwrap();

    assert_eq!(fs::read(root.join("state.sqlite")).unwrap(), fake_sqlite);
    assert_eq!(
        fs::read_to_string(root.join("team.json")).unwrap(),
        fake_team_json,
    );
    // New skeleton exists alongside.
    assert_dir(&root.join("agents").join("alice"));
    assert_file(&root.join("team/decisions.md"), DECISIONS_TEMPLATE);
}

#[test]
fn project_layer_preserves_existing_events_jsonl_content() {
    let cwd = tempdir().unwrap();
    let roster = [agent("alice")];
    let team_id = "team_demo";
    let root = cwd.path().join(OPENCRAB_DIR);

    let teams_dir = root.join("teams").join(team_id);
    fs::create_dir_all(&teams_dir).unwrap();
    let events_path = teams_dir.join("events.jsonl");
    let existing = b"{\"event\":\"already-written\"}\n";
    fs::write(&events_path, existing).unwrap();

    ensure_project_layer_at(&root, team_id, &roster).unwrap();

    assert_eq!(fs::read(&events_path).unwrap(), existing);
}

// -- idempotency ----------------------------------------------------------

#[test]
fn double_invocation_user_layer_is_idempotent() {
    let home = tempdir().unwrap();
    let roster = [agent("alice"), agent("bob")];

    ensure_user_layer_at(home.path(), &roster).unwrap();

    let snapshot: Vec<(_, _)> = collect_mtimes(home.path());

    // Sleep a hair so a re-write would visibly bump mtime — but we expect
    // NO re-write, hence equal mtimes.
    std::thread::sleep(std::time::Duration::from_millis(15));

    ensure_user_layer_at(home.path(), &roster).unwrap();

    let after: Vec<(_, _)> = collect_mtimes(home.path());
    assert_eq!(
        snapshot, after,
        "second ensure_user_layer mutated files (mtime changed)"
    );
}

#[test]
fn double_invocation_project_layer_is_idempotent() {
    let cwd = tempdir().unwrap();
    let roster = [agent("alice")];
    let team_id = "team_demo";
    let root = cwd.path().join(OPENCRAB_DIR);

    ensure_project_layer_at(&root, team_id, &roster).unwrap();
    let snapshot: Vec<(_, _)> = collect_mtimes(&root);

    std::thread::sleep(std::time::Duration::from_millis(15));

    ensure_project_layer_at(&root, team_id, &roster).unwrap();
    let after: Vec<(_, _)> = collect_mtimes(&root);

    assert_eq!(
        snapshot, after,
        "second ensure_project_layer mutated files (mtime changed)"
    );
}

// -- migrate_team_json ----------------------------------------------------

fn project_team_json(cwd: &Path) -> std::path::PathBuf {
    cwd.join(OPENCRAB_DIR).join("team.json")
}

fn home_team_json(home: &Path) -> std::path::PathBuf {
    home.join("team.json")
}

fn legacy_team_json(cwd: &Path) -> std::path::PathBuf {
    cwd.join(OPENCRAB_DIR).join("team.json.legacy")
}

fn valid_team_json_body() -> &'static str {
    r#"{
      "schemaVersion": 1,
      "id": "team_legacy",
      "name": "Legacy P3 Team",
      "createdAt": "2025-01-01T00:00:00Z",
      "templateId": "solo_pm",
      "agents": [],
      "subscriptions": []
    }"#
}

fn seed_project_team_json(cwd: &Path, body: &str) {
    fs::create_dir_all(cwd.join(OPENCRAB_DIR)).unwrap();
    fs::write(project_team_json(cwd), body).unwrap();
}

fn seed_home_team_json(home: &Path, body: &str) {
    fs::create_dir_all(home).unwrap();
    fs::write(home_team_json(home), body).unwrap();
}

#[test]
fn migrate_home_exists_old_absent_is_no_op() {
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let body = valid_team_json_body();
    seed_home_team_json(home.path(), body);

    let outcome = migrate_team_json_at(home.path(), cwd.path()).unwrap();
    assert_eq!(outcome, TeamJsonMigrationOutcome::UsedExistingHomeFile);
    assert_eq!(
        fs::read_to_string(home_team_json(home.path())).unwrap(),
        body
    );
    assert!(!project_team_json(cwd.path()).exists());
    assert!(!legacy_team_json(cwd.path()).exists());
}

#[test]
fn migrate_home_exists_old_exists_renames_old() {
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let home_body = valid_team_json_body();
    let old_body = r#"{"schemaVersion":1,"id":"team_old","agents":[]}"#;
    seed_home_team_json(home.path(), home_body);
    seed_project_team_json(cwd.path(), old_body);

    let outcome = migrate_team_json_at(home.path(), cwd.path()).unwrap();
    assert_eq!(outcome, TeamJsonMigrationOutcome::UsedExistingHomeFile);
    // Home file is untouched.
    assert_eq!(
        fs::read_to_string(home_team_json(home.path())).unwrap(),
        home_body
    );
    // Old file moved to .legacy preserving its content.
    assert!(!project_team_json(cwd.path()).exists());
    assert_eq!(
        fs::read_to_string(legacy_team_json(cwd.path())).unwrap(),
        old_body
    );
}

#[test]
fn migrate_old_only_copies_atomically_and_renames() {
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let body = valid_team_json_body();
    seed_project_team_json(cwd.path(), body);

    let outcome = migrate_team_json_at(home.path(), cwd.path()).unwrap();
    assert_eq!(outcome, TeamJsonMigrationOutcome::MigratedFromProject);
    assert_eq!(
        fs::read_to_string(home_team_json(home.path())).unwrap(),
        body
    );
    assert!(!project_team_json(cwd.path()).exists());
    assert_eq!(
        fs::read_to_string(legacy_team_json(cwd.path())).unwrap(),
        body
    );
    // No stray .tmp file left behind.
    let tmp = home.path().join("team.json.tmp");
    assert!(!tmp.exists(), "atomic write left .tmp behind: {tmp:?}");
}

#[test]
fn migrate_neither_exists_is_no_op() {
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();

    let outcome = migrate_team_json_at(home.path(), cwd.path()).unwrap();
    assert_eq!(outcome, TeamJsonMigrationOutcome::NoTeamConfigured);
    assert!(!home_team_json(home.path()).exists());
    assert!(!project_team_json(cwd.path()).exists());
    assert!(!legacy_team_json(cwd.path()).exists());
}

#[test]
fn migrate_corrupt_old_aborts_and_preserves_files() {
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let corrupt = "this is not { valid json";
    seed_project_team_json(cwd.path(), corrupt);

    let err = migrate_team_json_at(home.path(), cwd.path()).unwrap_err();
    match err {
        BootstrapError::CorruptJson { path, .. } => {
            assert_eq!(path, project_team_json(cwd.path()));
        }
        other => panic!("expected CorruptJson, got {other:?}"),
    }
    // Old file untouched, home not created.
    assert_eq!(
        fs::read_to_string(project_team_json(cwd.path())).unwrap(),
        corrupt
    );
    assert!(!legacy_team_json(cwd.path()).exists());
    assert!(!home_team_json(home.path()).exists());
}

#[test]
fn migrate_legacy_collision_overwrites_per_policy() {
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let body = valid_team_json_body();
    seed_project_team_json(cwd.path(), body);
    // Pre-existing .legacy from a previous migration attempt.
    let stale_legacy = "stale prior legacy content";
    fs::write(legacy_team_json(cwd.path()), stale_legacy).unwrap();

    let outcome = migrate_team_json_at(home.path(), cwd.path()).unwrap();
    assert_eq!(outcome, TeamJsonMigrationOutcome::MigratedFromProject);
    // Decision: overwrite — the new .legacy is the just-migrated body, not
    // the stale content.
    assert_eq!(
        fs::read_to_string(legacy_team_json(cwd.path())).unwrap(),
        body
    );
}

#[test]
fn migrate_is_idempotent_across_repeated_calls() {
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let body = valid_team_json_body();
    seed_project_team_json(cwd.path(), body);

    // First call: migrate.
    let first = migrate_team_json_at(home.path(), cwd.path()).unwrap();
    assert_eq!(first, TeamJsonMigrationOutcome::MigratedFromProject);
    let home_mtime = fs::metadata(home_team_json(home.path()))
        .unwrap()
        .modified()
        .unwrap();

    // Sleep a hair so a re-write would visibly bump mtime.
    std::thread::sleep(std::time::Duration::from_millis(15));

    // Second + third calls: no-op via UsedExistingHomeFile branch.
    for _ in 0..2 {
        let outcome = migrate_team_json_at(home.path(), cwd.path()).unwrap();
        assert_eq!(outcome, TeamJsonMigrationOutcome::UsedExistingHomeFile);
    }
    let after_mtime = fs::metadata(home_team_json(home.path()))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        home_mtime, after_mtime,
        "repeat migrate mutated home team.json (mtime changed)"
    );
}

#[test]
fn migrate_then_ensure_user_layer_chains_correctly() {
    // Smoke test: migrate produces a roster-bearing home file, then
    // ensure_user_layer can read it back and build per-agent subdirs.
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let body = r#"{
      "schemaVersion": 1,
      "id": "team_demo",
      "name": "Demo",
      "createdAt": "2025-01-01T00:00:00Z",
      "templateId": "pm_plus_one_dev",
      "agents": [
        {"id": "agent_alice", "name": "alice", "role": "pm",
         "model": "gpt-5", "systemPromptTemplate": "", "toolsPreset": "readonly"},
        {"id": "agent_bob", "name": "bob", "role": "dev",
         "model": "gpt-5", "systemPromptTemplate": "", "toolsPreset": "readwrite"}
      ],
      "subscriptions": []
    }"#;
    seed_project_team_json(cwd.path(), body);

    migrate_team_json_at(home.path(), cwd.path()).unwrap();

    // Parse the just-migrated home file (the production code path does
    // the same via `read_team_from_user_layer`).
    let raw = fs::read_to_string(home_team_json(home.path())).unwrap();
    let cfg: crate::team_config::types::TeamConfig = serde_json::from_str(&raw).unwrap();
    assert_eq!(cfg.agents.len(), 2);

    ensure_user_layer_at(home.path(), &cfg.agents).unwrap();

    for id in ["agent_alice", "agent_bob"] {
        let dir = home.path().join("agents").join(id);
        assert_dir(&dir);
        assert_file(&dir.join("SOUL.md"), SOUL_TEMPLATE);
    }
}

// -- helpers --------------------------------------------------------------

fn collect_mtimes(root: &Path) -> Vec<(std::path::PathBuf, std::time::SystemTime)> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walk(dir: &Path, out: &mut Vec<(std::path::PathBuf, std::time::SystemTime)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = entry.metadata().unwrap();
        if meta.is_dir() {
            walk(&path, out);
        } else {
            out.push((path, meta.modified().unwrap()));
        }
    }
}

// -- storage Step 2 regression guards -------------------------------------

#[test]
fn team_and_agent_ids_are_stable_across_workspaces() {
    // Storage Step 2: team identity (team_id + agent_ids) is machine-global.
    // Bootstrapping a second workspace must reuse the existing
    // `~/.opencrab/team.json`, never re-mint a fresh identity.
    let home = tempdir().unwrap();
    let cwd_a = tempdir().unwrap();
    let cwd_b = tempdir().unwrap();
    let body = r#"{
      "schemaVersion": 1,
      "id": "team_stable_fixture",
      "name": "Stability Fixture",
      "createdAt": "2025-01-01T00:00:00Z",
      "templateId": "pm_plus_one_dev",
      "agents": [
        {"id": "agent_alice", "name": "alice", "role": "pm",
         "model": "gpt-5", "systemPromptTemplate": "", "toolsPreset": "readonly"},
        {"id": "agent_bob", "name": "bob", "role": "dev",
         "model": "gpt-5", "systemPromptTemplate": "", "toolsPreset": "readwrite"}
      ],
      "subscriptions": []
    }"#;
    seed_home_team_json(home.path(), body);

    // "Bootstrap workspace X" = the migration precondition + identity read.
    let read_identity = |cwd: &Path| -> (String, Vec<String>) {
        migrate_team_json_at(home.path(), cwd).unwrap();
        let raw = fs::read_to_string(home_team_json(home.path())).unwrap();
        let cfg: crate::team_config::types::TeamConfig =
            serde_json::from_str(&raw).unwrap();
        (cfg.id, cfg.agents.into_iter().map(|a| a.id).collect())
    };
    let (team_a, agents_a) = read_identity(cwd_a.path());
    let (team_b, agents_b) = read_identity(cwd_b.path());

    assert_eq!(team_a, team_b, "team_id drifted between workspaces");
    assert_eq!(team_a, "team_stable_fixture", "team_id was re-minted");
    assert_eq!(agents_a, agents_b, "agent_ids drifted between workspaces");
    assert_eq!(
        agents_a,
        vec!["agent_alice".to_string(), "agent_bob".to_string()],
        "agent_ids were re-minted",
    );
}

#[test]
fn cleanup_workspace_state_spares_machine_global_identity() {
    // Storage Step 2: removing a workspace deletes that workspace's project
    // layer (`<cwd>/.opencrab/`) ONLY. The machine-global
    // `~/.opencrab/team.json` + `~/.opencrab/agents/` are identity and MUST
    // survive — re-introducing their deletion is the exact step-2 bug.
    let _guard = crate::paths::ENV_LOCK.lock().expect("env lock");
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();

    // Point $HOME at a synthetic user layer — the blast radius a
    // re-introduced `team.json` deletion would actually hit.
    let prev_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", home.path());
    let user_root = home.path().join(OPENCRAB_DIR);
    fs::create_dir_all(user_root.join("agents").join("agent_alice")).unwrap();
    fs::write(user_root.join("team.json"), "{\"id\":\"team_x\"}").unwrap();

    // Populate this workspace's project layer.
    let project_dir = cwd.path().join(OPENCRAB_DIR);
    fs::create_dir_all(project_dir.join("agents").join("agent_alice")).unwrap();
    fs::write(project_dir.join("state.sqlite"), b"sqlite").unwrap();

    cleanup_workspace_state(cwd.path());

    // Capture before restoring $HOME so an assert panic cannot skip restore.
    let project_layer_gone = !project_dir.exists();
    let team_json_survives = user_root.join("team.json").exists();
    let agents_survive = user_root.join("agents").join("agent_alice").exists();

    match prev_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }

    assert!(project_layer_gone, "cleanup must delete <cwd>/.opencrab/");
    assert!(
        team_json_survives,
        "cleanup must NOT delete the machine-global ~/.opencrab/team.json",
    );
    assert!(
        agents_survive,
        "cleanup must NOT delete the machine-global ~/.opencrab/agents/",
    );
}
