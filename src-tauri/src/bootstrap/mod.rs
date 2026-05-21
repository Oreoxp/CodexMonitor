// Phase 4 Steps 1-2 — double-layer storage bootstrap + team.json migration.
//
// Creates the directory + template skeleton for the OpenCrab 3.0 double-
// layer storage layout, and physically migrates `team.json` from the legacy
// project-layer location (`<cwd>/.opencrab/team.json`) to the user-layer
// canonical location (`~/.opencrab/team.json`). Idempotent: re-running
// never overwrites a file that already exists, and re-runs after partial
// state complete the layout.
//
// What this module does NOT do (deferred to later P4 steps):
//   - File loader + inode/mtime cache (Step 3)
//   - Cache-boundary sentinel (Step 4)
//   - events.jsonl append logic (Step 6) — Step 1 only touches the empty file
//   - KANBAN.md content rendering (Step 7) — Step 1 only seeds an empty placeholder
//   - CODEX_HOME per-spawn injection (Step 8) — Step 1 only mkdirs the target dir
//
// Wiring (see sidecar_session/commands.rs::sidecar_provision +
// team_config/commands.rs read/create commands):
//   Every Tauri command that reads or writes team.json calls
//   `migrate_team_json(cwd)` first, as a precondition. After migration the
//   user-layer copy is authoritative; reads + writes go to `~/.opencrab/
//   team.json`. `sidecar_provision` additionally invokes
//   `ensure_user_layer(&team.agents)` + `ensure_project_layer(workspace_root,
//   &team.id, &team.agents)` once the team is loaded from the home file.
//   Bootstrap failures propagate as the Tauri command's error string so
//   users see a real message rather than silent partial state.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::codex::home::{resolve_home_dir, OPENCRAB_HOME_DIR_NAME};
use crate::team_config::types::AgentConfig;

/// Roster slice type alias. Sidecar/Tauri pass `&[AgentConfig]` directly;
/// the alias documents intent at call sites.
pub(crate) type TeamRoster<'a> = &'a [AgentConfig];

const OPENCRAB_DIR: &str = ".opencrab";

const SOUL_TEMPLATE: &str = "# Soul\n\n";
const IDENTITY_TEMPLATE: &str = "# Identity\n\n<!-- agent 角色 / 身份 / 跨项目稳定的元信息 -->\n";
const USER_TEMPLATE: &str = "# User Notes\n\n";
const MEMORY_TEMPLATE: &str = "# Long-term Memory\n\n<!-- 跨项目长期记忆。dynamic 日记走 project-memory/YYYY-MM-DD.md(P5) -->\n";
const KANBAN_TEMPLATE: &str = "# Project Kanban\n\n";
const DECISIONS_TEMPLATE: &str = "<!-- team/decisions.md -->\n# Team Decisions\n\n";

#[derive(Debug)]
pub(crate) enum BootstrapError {
    HomeUnresolved,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Step 2 — the legacy `team.json` exists but is not parseable as JSON.
    /// Migration aborts so the user can inspect or repair the file; the
    /// home-layer copy is **not** created.
    CorruptJson {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootstrapError::HomeUnresolved => {
                write!(f, "bootstrap: could not resolve $HOME for user layer")
            }
            BootstrapError::Io { path, source } => {
                write!(f, "bootstrap io error at {}: {source}", path.display())
            }
            BootstrapError::CorruptJson { path, source } => {
                write!(
                    f,
                    "bootstrap: legacy team.json at {} is not valid JSON ({source}); \
                     migration refused — inspect and repair the file, or move it aside manually",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for BootstrapError {}

impl BootstrapError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        BootstrapError::Io {
            path: path.into(),
            source,
        }
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve `~/.opencrab/` (the user layer root). Errors if $HOME is not set
/// and no fallback works.
pub(crate) fn user_data_dir() -> Result<PathBuf, BootstrapError> {
    resolve_home_dir()
        .map(|home| home.join(OPENCRAB_HOME_DIR_NAME))
        .ok_or(BootstrapError::HomeUnresolved)
}

/// Resolve `<cwd>/.opencrab/` (the project layer root). Pure path join — no
/// IO, no fallibility.
pub(crate) fn project_data_dir(cwd: &Path) -> PathBuf {
    cwd.join(OPENCRAB_DIR)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Ensure the user-layer directory + per-agent template skeleton exists at
/// `~/.opencrab/`. Idempotent; never overwrites an existing file.
pub(crate) fn ensure_user_layer(roster: TeamRoster<'_>) -> Result<(), BootstrapError> {
    let root = user_data_dir()?;
    ensure_user_layer_at(&root, roster)
}

/// Ensure the project-layer directory + per-agent + per-team-id skeleton
/// exists at `<cwd>/.opencrab/`. Idempotent; never overwrites an existing
/// file. Does **not** create or touch `team.json` or `state.sqlite` — those
/// belong to earlier phases (`team_config` / `tasks::store::open_and_init`).
pub(crate) fn ensure_project_layer(
    cwd: &Path,
    team_id: &str,
    roster: TeamRoster<'_>,
) -> Result<(), BootstrapError> {
    let root = project_data_dir(cwd);
    ensure_project_layer_at(&root, team_id, roster)
}

// ---------------------------------------------------------------------------
// Path-driven helpers (used by tests with a tempdir)
// ---------------------------------------------------------------------------

pub(crate) fn ensure_user_layer_at(
    root: &Path,
    roster: TeamRoster<'_>,
) -> Result<(), BootstrapError> {
    create_dir_all(root)?;
    let agents_dir = root.join("agents");
    create_dir_all(&agents_dir)?;

    for agent in roster {
        let agent_dir = agents_dir.join(&agent.id);
        create_dir_all(&agent_dir)?;
        write_template_if_missing(&agent_dir.join("SOUL.md"), SOUL_TEMPLATE)?;
        write_template_if_missing(&agent_dir.join("IDENTITY.md"), IDENTITY_TEMPLATE)?;
        write_template_if_missing(&agent_dir.join("USER.md"), USER_TEMPLATE)?;
        write_template_if_missing(&agent_dir.join("MEMORY.md"), MEMORY_TEMPLATE)?;

        // Phase 4 Step 1-patch (2026-05-18): the per-agent
        // `codex-home/sessions/` subtree is intentionally NOT created here.
        // Decision flipped from the original "per-agent CODEX_HOME" plan to
        // a single shared `~/.opencrab/` as `CODEX_HOME` (auth.json /
        // config.toml managed by codex-cli itself). Rollouts will go to
        // `~/.opencrab/agents/<id>/team_sessions/<team_id>/<project_hash>/`
        // via codex-cli's `--session-dir` flag, with that subtree
        // `mkdir_p`-ed per-spawn in Step 8 (NOT here). Pre-existing
        // `codex-home/` directories from earlier Step 1 runs are harmless
        // orphans — no migration / cleanup is performed.
    }

    Ok(())
}

pub(crate) fn ensure_project_layer_at(
    root: &Path,
    team_id: &str,
    roster: TeamRoster<'_>,
) -> Result<(), BootstrapError> {
    create_dir_all(root)?;

    let agents_dir = root.join("agents");
    create_dir_all(&agents_dir)?;
    for agent in roster {
        let agent_dir = agents_dir.join(&agent.id);
        create_dir_all(&agent_dir)?;
        write_template_if_missing(&agent_dir.join("KANBAN.md"), KANBAN_TEMPLATE)?;
        create_dir_all(&agent_dir.join("project-memory"))?;
    }

    let team_dir = root.join("team");
    create_dir_all(&team_dir)?;
    write_template_if_missing(&team_dir.join("decisions.md"), DECISIONS_TEMPLATE)?;

    let teams_dir = root.join("teams").join(team_id);
    create_dir_all(&teams_dir)?;
    touch_empty_file_if_missing(&teams_dir.join("events.jsonl"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// File primitives
// ---------------------------------------------------------------------------

fn create_dir_all(path: &Path) -> Result<(), BootstrapError> {
    fs::create_dir_all(path).map_err(|err| BootstrapError::io(path, err))
}

/// Write `content` to `path` only if it does not already exist. Uses
/// `O_CREAT | O_EXCL` semantics via `create_new(true)` and silently skips
/// when the file already exists.
fn write_template_if_missing(path: &Path, content: &str) -> Result<(), BootstrapError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => f
            .write_all(content.as_bytes())
            .map_err(|err| BootstrapError::io(path, err)),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(BootstrapError::io(path, err)),
    }
}

/// Create `path` as a 0-byte file if it does not already exist. Existing
/// content (any size) is preserved untouched.
fn touch_empty_file_if_missing(path: &Path) -> Result<(), BootstrapError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(BootstrapError::io(path, err)),
    }
}

/// Atomic file write via `<path>.tmp` + `fsync` + `rename`. The rename
/// step is atomic on POSIX and overwrites the target on both Unix and
/// Windows (Rust's `std::fs::rename` maps to `MoveFileEx` with
/// `MOVEFILE_REPLACE_EXISTING` on Windows).
fn atomic_write(path: &Path, content: &[u8]) -> Result<(), BootstrapError> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    // `<path>.tmp` keeps the partial write next to its final home so the
    // rename is intra-directory (the only kind POSIX guarantees atomic).
    let tmp = path.with_extension({
        let original = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        if original.is_empty() {
            "tmp".to_string()
        } else {
            format!("{original}.tmp")
        }
    });
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|err| BootstrapError::io(&tmp, err))?;
        f.write_all(content)
            .map_err(|err| BootstrapError::io(&tmp, err))?;
        f.sync_all().map_err(|err| BootstrapError::io(&tmp, err))?;
    }
    fs::rename(&tmp, path).map_err(|err| BootstrapError::io(path, err))
}

// ===========================================================================
// Step 2 — team.json migration (project layer → user layer)
// ===========================================================================

const TEAM_JSON: &str = "team.json";
const TEAM_JSON_LEGACY: &str = "team.json.legacy";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TeamJsonMigrationOutcome {
    /// `~/.opencrab/team.json` already existed; no copy happened. If the
    /// legacy project-layer file was also present, it was renamed to
    /// `team.json.legacy` (overwriting any prior `.legacy`).
    UsedExistingHomeFile,
    /// Copied `<cwd>/.opencrab/team.json` → `~/.opencrab/team.json` (atomic
    /// temp + rename), then renamed the source to `team.json.legacy`
    /// (overwriting any prior `.legacy`).
    MigratedFromProject,
    /// Neither file existed. No team is configured yet; the home location
    /// stays empty so existing "no team" semantics (read returns `None`,
    /// UI prompts to create one) are preserved.
    NoTeamConfigured,
}

/// Migrate `team.json` from the project layer to the user layer (one-shot,
/// idempotent). See [`TeamJsonMigrationOutcome`] for the branch semantics.
///
/// Constraints:
/// - **Atomic write** (temp + fsync + rename) for the home-layer copy. A
///   crash between the write and the rename leaves no partial home file;
///   the next run sees the same starting state and re-tries cleanly.
/// - **Old file preservation:** the legacy file is renamed to
///   `team.json.legacy`, never deleted. If `.legacy` already exists from a
///   prior migration, it is overwritten (user-chosen policy).
/// - **Corrupt JSON aborts** the migration with an error. The original
///   file stays untouched; the user can inspect or fix it manually.
pub(crate) fn migrate_team_json(cwd: &Path) -> Result<TeamJsonMigrationOutcome, BootstrapError> {
    let home = user_data_dir()?;
    migrate_team_json_at(&home, cwd)
}

pub(crate) fn migrate_team_json_at(
    home: &Path,
    cwd: &Path,
) -> Result<TeamJsonMigrationOutcome, BootstrapError> {
    let new_path = home.join(TEAM_JSON);
    let old_path = cwd.join(OPENCRAB_DIR).join(TEAM_JSON);

    if new_path.exists() {
        if old_path.exists() {
            move_to_legacy(&old_path)?;
        }
        return Ok(TeamJsonMigrationOutcome::UsedExistingHomeFile);
    }

    if !old_path.exists() {
        return Ok(TeamJsonMigrationOutcome::NoTeamConfigured);
    }

    // Old exists, new doesn't — migrate.
    let raw = fs::read(&old_path).map_err(|err| BootstrapError::io(&old_path, err))?;
    // JSON-shape validation only (parses as JSON). Strict TeamConfig shape
    // is the loader's job; we just guarantee we're not promoting garbage.
    let _: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|err| BootstrapError::CorruptJson {
            path: old_path.clone(),
            source: err,
        })?;

    create_dir_all(home)?;
    atomic_write(&new_path, &raw)?;
    move_to_legacy(&old_path)?;
    Ok(TeamJsonMigrationOutcome::MigratedFromProject)
}

/// Rename `<dir>/team.json` → `<dir>/team.json.legacy`, overwriting any
/// pre-existing `.legacy` (Step-2 collision policy).
fn move_to_legacy(old_path: &Path) -> Result<(), BootstrapError> {
    let legacy = old_path.with_file_name(TEAM_JSON_LEGACY);
    fs::rename(old_path, &legacy).map_err(|err| BootstrapError::io(&legacy, err))
}

// ===========================================================================
// Path helpers for Step-2 callers (read/write through the user layer)
// ===========================================================================

/// `~/.opencrab/team.json`.
pub(crate) fn team_json_path() -> Result<PathBuf, BootstrapError> {
    Ok(user_data_dir()?.join(TEAM_JSON))
}

/// Atomic write of a team.json payload to the user-layer location. Used by
/// `team_config::commands` for the create/template-instantiate path; kept
/// here so the atomic-write primitive stays a single source of truth.
pub(crate) fn write_team_json_atomic(content: &[u8]) -> Result<(), BootstrapError> {
    let path = team_json_path()?;
    atomic_write(&path, content)
}

/// Tear down OpenCrab state when a workspace is removed: the project-layer
/// `<cwd>/.opencrab/` tree and the user-layer `~/.opencrab/team.json`.
///
/// `team.json` is a single machine-wide file (Phase 4 Step 2), so deleting
/// it here resets team creation for *every* workspace — intentional, since
/// without this the team picker never reappears once a team exists.
///
/// Best-effort: each failure is logged but never returned. Workspace
/// removal has already succeeded by the time this runs, so a partial
/// cleanup must not surface as a removal error.
pub(crate) fn cleanup_workspace_state(cwd: &Path) {
    let project_dir = project_data_dir(cwd);
    if project_dir.is_dir() {
        if let Err(err) = fs::remove_dir_all(&project_dir) {
            eprintln!(
                "[opencrab] cleanup: failed to remove {}: {err}",
                project_dir.display()
            );
        }
    }

    match team_json_path() {
        Ok(team_json) => {
            if team_json.exists() {
                if let Err(err) = fs::remove_file(&team_json) {
                    eprintln!(
                        "[opencrab] cleanup: failed to remove {}: {err}",
                        team_json.display()
                    );
                }
            }
        }
        Err(err) => {
            eprintln!("[opencrab] cleanup: cannot resolve team.json path: {err}");
        }
    }
}

#[cfg(test)]
mod tests;
