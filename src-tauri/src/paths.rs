//! Single source of truth for every OpenCrab `.opencrab` filesystem path.
//!
//! Step 4 refactor: before this module the `.opencrab` literal and the
//! per-layer subdirectory names were scattered across `bootstrap`,
//! `codex_spawn`, `codex::home`, `codex::config`, `tasks::store`,
//! `kanban`, `events`, `team_config` and `shared::agents_config_core`.
//! Every path-segment literal now lives here and nowhere else; the rest of
//! `src-tauri` (non-test code) calls these functions.
//!
//! ## Self-contained on purpose
//!
//! The daemon binary (`src/bin/codex_monitor_daemon.rs`) pulls lib modules
//! in by `#[path]` source-include, and it includes `codex_spawn` /
//! `codex/home.rs` (which now call `crate::paths`). So this module is
//! `#[path]`-included into the daemon bin too, and therefore must depend on
//! nothing but `std` + `libc` (a package-wide dep) + `sha2` is NOT used here
//! (`project_hash` stays in `codex_spawn`). No `crate::`-relative reference
//! leaves this file.
//!
//! ## Behaviour is byte-identical to the pre-refactor code
//!
//! Every function reproduces the exact join chain the old call site used —
//! including the two deliberately-different rollout layouts (see
//! [`workspace_rollout_dir`] vs [`agent_rollout_dir`]). The one intentional
//! behaviour change of Step 4 is NOT in this module: it is that
//! `codex_spawn`'s home resolution now routes through [`home_dir`] (which
//! has the `getpwuid` fallback) instead of its old fallback-less copy.

use std::path::{Path, PathBuf};

/// The OpenCrab state directory name, used for both layers
/// (`~/.opencrab/` and `<cwd>/.opencrab/`). The only place this literal
/// exists in non-test `src-tauri` code.
pub(crate) const OPENCRAB_DIR: &str = ".opencrab";

// ===========================================================================
// Home + layer roots
// ===========================================================================

/// Resolve the user's home directory: `$HOME`, then `$USERPROFILE`, then —
/// on unix — the `getpwuid` entry (a fallback for daemon environments that
/// do not export `HOME`). Moved verbatim from the former
/// `codex::home::resolve_home_dir`.
pub(crate) fn home_dir() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("HOME") {
        if !value.trim().is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    if let Ok(value) = std::env::var("USERPROFILE") {
        if !value.trim().is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    #[cfg(unix)]
    {
        // Fallback for daemon environments that do not expose HOME.
        unsafe {
            let uid = libc::geteuid();
            let pwd = libc::getpwuid(uid);
            if !pwd.is_null() {
                let dir_ptr = (*pwd).pw_dir;
                if !dir_ptr.is_null() {
                    if let Ok(dir) = std::ffi::CStr::from_ptr(dir_ptr).to_str() {
                        if !dir.trim().is_empty() {
                            return Some(PathBuf::from(dir));
                        }
                    }
                }
            }
        }
    }
    None
}

/// The user layer root: `~/.opencrab/`. This is also the shared
/// `CODEX_HOME` for every team agent. `None` only when no home directory
/// can be resolved at all.
pub(crate) fn user_root() -> Option<PathBuf> {
    home_dir().map(|home| home.join(OPENCRAB_DIR))
}

/// The project layer root for a workspace: `<cwd>/.opencrab/`. Pure path
/// join — no IO, never fails.
pub(crate) fn project_root(cwd: &Path) -> PathBuf {
    cwd.join(OPENCRAB_DIR)
}

// ===========================================================================
// User layer — `~/.opencrab/...`
// ===========================================================================

/// `~/.opencrab/team.json` — the machine-global team roster.
pub(crate) fn team_json(user_root: &Path) -> PathBuf {
    user_root.join("team.json")
}

/// `~/.opencrab/config.toml` — codex-cli's self-managed model/provider
/// config (shared `CODEX_HOME`).
pub(crate) fn config_toml(user_root: &Path) -> PathBuf {
    user_root.join("config.toml")
}

/// `~/.opencrab/agents/` — per-agent identity directory parent.
pub(crate) fn user_agents_dir(user_root: &Path) -> PathBuf {
    user_root.join("agents")
}

/// `~/.opencrab/agents/<agent_id>/` — one agent's identity directory.
pub(crate) fn user_agent_dir(user_root: &Path, agent_id: &str) -> PathBuf {
    user_agents_dir(user_root).join(agent_id)
}

/// `~/.opencrab/agents/<agent_id>/SOUL.md`.
pub(crate) fn user_agent_soul_md(user_root: &Path, agent_id: &str) -> PathBuf {
    user_agent_dir(user_root, agent_id).join("SOUL.md")
}

/// `~/.opencrab/agents/<agent_id>/ROLE.md` — per-agent role charter (P6).
/// Sits between SOUL (persona) and IDENTITY (avatar/vibe card) in
/// `CONTEXT_FILE_ORDER`, so the cache-stable prefix reads
/// persona → charter → identity card → user model.
pub(crate) fn user_agent_role_md(user_root: &Path, agent_id: &str) -> PathBuf {
    user_agent_dir(user_root, agent_id).join("ROLE.md")
}

/// `~/.opencrab/agents/<agent_id>/IDENTITY.md`.
pub(crate) fn user_agent_identity_md(user_root: &Path, agent_id: &str) -> PathBuf {
    user_agent_dir(user_root, agent_id).join("IDENTITY.md")
}

/// `~/.opencrab/agents/<agent_id>/USER.md`.
pub(crate) fn user_agent_user_md(user_root: &Path, agent_id: &str) -> PathBuf {
    user_agent_dir(user_root, agent_id).join("USER.md")
}

/// `~/.opencrab/agents/<agent_id>/MEMORY.md`.
pub(crate) fn user_agent_memory_md(user_root: &Path, agent_id: &str) -> PathBuf {
    user_agent_dir(user_root, agent_id).join("MEMORY.md")
}

// ===========================================================================
// Project layer — `<cwd>/.opencrab/...`
// ===========================================================================

/// `<cwd>/.opencrab/team.json` — the legacy project-layer team file
/// (migrated to the user layer; this path is read during migration).
pub(crate) fn project_team_json(project_root: &Path) -> PathBuf {
    project_root.join("team.json")
}

/// Given a `team.json` path, the sibling `team.json.legacy` path in the
/// same directory. Mirrors the historical `Path::with_file_name` call in
/// the team.json migration so behaviour stays byte-identical.
pub(crate) fn team_json_legacy_sibling(team_json: &Path) -> PathBuf {
    team_json.with_file_name("team.json.legacy")
}

/// `<cwd>/.opencrab/state.sqlite` — LangGraph checkpoint + Rust `tasks`
/// table, shared file.
pub(crate) fn project_state_sqlite(project_root: &Path) -> PathBuf {
    project_root.join("state.sqlite")
}

/// `<cwd>/.opencrab/threads.json` — per-workspace agent→Codex-thread
/// bindings.
pub(crate) fn project_threads_json(project_root: &Path) -> PathBuf {
    project_root.join("threads.json")
}

/// `<cwd>/.opencrab/agents/` — per-agent project-layer directory parent.
pub(crate) fn project_agents_dir(project_root: &Path) -> PathBuf {
    project_root.join("agents")
}

/// `<cwd>/.opencrab/agents/<agent_id>/`.
pub(crate) fn project_agent_dir(project_root: &Path, agent_id: &str) -> PathBuf {
    project_agents_dir(project_root).join(agent_id)
}

/// `<cwd>/.opencrab/agents/<agent_id>/KANBAN.md`.
pub(crate) fn project_agent_kanban_md(project_root: &Path, agent_id: &str) -> PathBuf {
    project_agent_dir(project_root, agent_id).join("KANBAN.md")
}

/// `<cwd>/.opencrab/agents/<agent_id>/project-memory/`.
pub(crate) fn project_agent_memory_dir(project_root: &Path, agent_id: &str) -> PathBuf {
    project_agent_dir(project_root, agent_id).join("project-memory")
}

/// `<cwd>/.opencrab/agents/<agent_id>/project-memory/<date>.md` — one
/// per-agent daily-memory journal file. `date` is a `YYYY-MM-DD` stamp
/// computed in the user's local timezone; this resolver is a pure join, so
/// the caller owns the date formatting. Mirrors `paths.ts`
/// `projectAgentMemoryFile`. The Phase 5 write layer (Step 3 Block B) is the
/// consumer — the read layer enumerates the directory rather than naming
/// files.
pub(crate) fn project_agent_memory_file(
    project_root: &Path,
    agent_id: &str,
    date: &str,
) -> PathBuf {
    project_agent_memory_dir(project_root, agent_id).join(format!("{date}.md"))
}

/// `<cwd>/.opencrab/team/` — team-shared markdown directory.
pub(crate) fn project_team_dir(project_root: &Path) -> PathBuf {
    project_root.join("team")
}

/// `<cwd>/.opencrab/team/decisions.md`.
pub(crate) fn project_team_decisions_md(project_root: &Path) -> PathBuf {
    project_team_dir(project_root).join("decisions.md")
}

/// `<cwd>/.opencrab/teams/` — per-team audit directory parent.
pub(crate) fn project_teams_dir(project_root: &Path) -> PathBuf {
    project_root.join("teams")
}

/// `<cwd>/.opencrab/teams/<team_id>/`.
pub(crate) fn project_team_events_dir(project_root: &Path, team_id: &str) -> PathBuf {
    project_teams_dir(project_root).join(team_id)
}

/// `<cwd>/.opencrab/teams/<team_id>/events.jsonl`.
pub(crate) fn project_team_events_jsonl(project_root: &Path, team_id: &str) -> PathBuf {
    project_team_events_dir(project_root, team_id).join("events.jsonl")
}

// ===========================================================================
// Rollout directories (user layer) — two deliberately-different layouts
// ===========================================================================
//
// These two encode an *existing inconsistency* that Step 4 must preserve
// verbatim (it is a separate later step's job to reconcile, not this one):
//   * the stdio / CLI-flag path is workspace-scoped and flat;
//   * the WebSocket / `thread/start` path is per-agent and nests a
//     `<team_id>` segment inside the agent directory.
// `project_hash` itself is computed by `codex_spawn::project_hash` (a hash,
// not a path segment) and passed in here as a ready string.

/// Workspace-scoped rollout dir (stdio transport / `--team-session-dir`
/// CLI flag): `~/.opencrab/team_sessions/<project_hash>/`.
pub(crate) fn workspace_rollout_dir(user_root: &Path, project_hash: &str) -> PathBuf {
    user_root.join("team_sessions").join(project_hash)
}

/// Per-agent rollout dir (WebSocket `thread/start` `sessionDir` override):
/// `~/.opencrab/agents/<agent_id>/team_sessions/<team_id>/<project_hash>/`.
pub(crate) fn agent_rollout_dir(
    user_root: &Path,
    agent_id: &str,
    team_id: &str,
    project_hash: &str,
) -> PathBuf {
    user_agents_dir(user_root)
        .join(agent_id)
        .join("team_sessions")
        .join(team_id)
        .join(project_hash)
}

// ===========================================================================
// Test-only shared env lock
// ===========================================================================
//
// `home_dir()` reads process-global env vars. Any test that mutates `$HOME`
// / `$USERPROFILE` must serialize against EVERY other such test in the same
// test binary, or the parallel test harness races on the env. This is the
// single crate-wide lock all env-mutating tests take — `paths::tests`,
// `bootstrap::tests`, and `codex::home::tests` all funnel through it.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Set (`Some`) or unset (`None`) an env var.
    fn set_env(key: &str, val: Option<&str>) {
        match val {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn home_dir_prefers_home_over_userprofile() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prev_home = std::env::var("HOME").ok();
        let prev_profile = std::env::var("USERPROFILE").ok();

        set_env("HOME", Some("/tmp/opencrab-home-tier"));
        set_env("USERPROFILE", Some("/tmp/opencrab-userprofile-tier"));
        assert_eq!(home_dir(), Some(PathBuf::from("/tmp/opencrab-home-tier")));

        set_env("HOME", prev_home.as_deref());
        set_env("USERPROFILE", prev_profile.as_deref());
    }

    #[test]
    fn home_dir_falls_back_to_userprofile_when_home_unset() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prev_home = std::env::var("HOME").ok();
        let prev_profile = std::env::var("USERPROFILE").ok();

        set_env("HOME", None);
        set_env("USERPROFILE", Some("/tmp/opencrab-userprofile-only"));
        assert_eq!(
            home_dir(),
            Some(PathBuf::from("/tmp/opencrab-userprofile-only")),
        );

        set_env("HOME", prev_home.as_deref());
        set_env("USERPROFILE", prev_profile.as_deref());
    }

    #[test]
    fn user_root_appends_opencrab_dir_under_home() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prev_home = std::env::var("HOME").ok();

        set_env("HOME", Some("/tmp/opencrab-root-test"));
        assert_eq!(
            user_root(),
            Some(PathBuf::from("/tmp/opencrab-root-test").join(OPENCRAB_DIR)),
        );

        set_env("HOME", prev_home.as_deref());

        // `project_root` is a pure join — env-independent.
        assert_eq!(
            project_root(Path::new("/ws")),
            Path::new("/ws").join(OPENCRAB_DIR),
        );
    }

    #[test]
    fn project_agent_memory_file_names_date_md_under_memory_dir() {
        // Pure join — env-independent. The daily-memory file lives directly
        // inside the agent's `project-memory/` directory, named `<date>.md`.
        let root = Path::new("/ws/.opencrab");
        let file = project_agent_memory_file(root, "alice", "2026-05-22");
        assert_eq!(
            file,
            project_agent_memory_dir(root, "alice").join("2026-05-22.md"),
        );
        assert_eq!(
            file,
            Path::new("/ws/.opencrab/agents/alice/project-memory/2026-05-22.md"),
        );
    }
}
