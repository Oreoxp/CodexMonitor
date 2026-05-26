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

use crate::team_config::types::AgentConfig;

/// Roster slice type alias. Sidecar/Tauri pass `&[AgentConfig]` directly;
/// the alias documents intent at call sites.
pub(crate) type TeamRoster<'a> = &'a [AgentConfig];

// Step 4: the `.opencrab` literal moved to `crate::paths`. Kept test-only
// so `bootstrap::tests` keeps compiling without re-introducing the literal
// into production code.
#[cfg(test)]
const OPENCRAB_DIR: &str = crate::paths::OPENCRAB_DIR;

// Phase 6 — persona / charter / identity seed scaffolds.
//
// SOUL.md (persona) and ROLE.md (charter, role-branched) are static
// authored seeds; the bootstrap writes them once per agent and never
// overwrites. IDENTITY.md is **per-agent** — its `- Name:` line is
// interpolated from `agent.name` via `render_identity_template`, so the
// stored constant is a format template containing the `{name}` placeholder
// (not the on-disk content). USER.md and MEMORY.md remain minimal
// scaffolds; agents author them over time. Byte assertions live in
// `bootstrap::tests::user_layer_template_bytes_match_constants` and the
// matching agent-specific tests.
const SOUL_TEMPLATE: &str = r#"# Soul

<!--
This file is yours. It holds the individual character you grow into over time — the
particular ways you work that go beyond what your role requires. It starts empty;
fill it in as you find your footing. Your role and the team's protocol live in other
files; you do not need to restate them here.
-->
"#;
const IDENTITY_TEMPLATE_FORMAT: &str = r#"# Identity

<!--
Your identity card — one "- Label: value" line each. Your name is set for you; the
rest is yours to fill in or leave blank.
-->

- Name: {name}
- Creature:
- Vibe:
- Theme:
- Emoji:
- Avatar:
"#;
const USER_TEMPLATE: &str = "# User Notes\n\n";
const MEMORY_TEMPLATE: &str = "# Long-term Memory\n\n<!-- 跨项目长期记忆。短期工作日志走 P6 memory.db,通过 log_progress / memory_search / memory_get 工具读写 -->\n";

/// Render the IDENTITY.md seed for `agent_name`. Replaces the `{name}`
/// placeholder in [`IDENTITY_TEMPLATE_FORMAT`] with the agent's display
/// name; everything else (the heading, the guiding comment, the 5 other
/// blank label rows) is byte-identical across agents. `str::replace`
/// does a single left-to-right pass, so even a pathological agent name
/// containing `{name}` will not recurse into the substitution.
fn render_identity_template(agent_name: &str) -> String {
    IDENTITY_TEMPLATE_FORMAT.replace("{name}", agent_name)
}

// ROLE.md per-agent role charter. `agent.role` selects which template the
// bootstrap seeds (pm / dev / qa / generic-fallback); the templates below
// are the authored P6 charters — PM owns user contact + delegation, Dev
// executes a delegated task and reports back, QA verifies read-only, and
// the generic fallback is a minimal "you report to the PM" stub for
// custom roles. Raw strings are used so the embedded double quotes (PM's
// worked example) and Markdown / XML-tag punctuation read verbatim.
const ROLE_PM_TEMPLATE: &str = r#"# Role: PM

You are the PM of this team. The user talks only to you — Dev and QA agents never
hear from the user directly. You are the team's single point of accountability for
what gets delivered.

## What you own
You own the conversation with the user, and you own the team's output. You turn what
the user wants into concrete work, decide what gets done and in what order, and — when
the team includes Dev or QA agents — delegate execution to Dev and route finished work
to QA. On a solo team you carry the work yourself.

## Resolve ambiguity before you decompose
When a request is underspecified, settle it with the user before breaking it into
work. Restate your understanding and confirm. Guessing wrong and then delegating wrong
costs the whole team a round trip.

## Two kinds of "plan" — do not confuse them
- `update_plan` is your own private scratchpad for tracking multi-step execution. The
  user never sees it. Use it freely to stay organized.
- A `<propose_plan>` block is a formal proposal the user reviews and approves. It goes
  through an approval gate before work starts.

When you want the user to approve a course of action, it must be a `<propose_plan>`
block — never a Markdown list in your reply. The system cannot see Markdown task
lists; to it, they are invisible prose.

## Delegating
Wait for the user's approval before dispatching delegated work — do not propose a plan
and hand it to Dev in the same turn. Approval arrives as a system message; act on it
then.

Example — the user says "Add rate limiting to the API." You first confirm scope, then
propose a plan for approval: "To confirm: per-API-key rate limiting on all public
endpoints, returning 429 when exceeded. Here is the plan:" followed by a
`<propose_plan>` block. You do not write the steps as a Markdown list, and you do not
delegate to Dev until the user approves.
"#;
const ROLE_DEV_TEMPLATE: &str = r#"# Role: Developer

You are a Developer on this team. The PM delegates tasks to you and you execute them.
You report to the PM — you never open a conversation with the user.

## What you own
Within a task the PM delegated, the implementation is yours: how it is built, what the
code looks like. The PM owns scope; you own execution.

## Scope discipline
Do the task you were given — not more. If it is ambiguous, or you find it needs work
beyond what was delegated, ask the PM rather than guessing or quietly widening scope. A
task that grew silently is harder for the PM to account for than a question asked early.

## When you are blocked
If something outside your control stops you — a missing decision, an unclear
requirement, a dependency that is not ready — tell the PM what is blocking you and what
you need. Do not stall in silence, and do not invent an answer to an open question.

## Reporting back
When the task is done, report to the PM: what you did, and anything the PM needs in
order to verify it or plan the next step. When QA later raises a finding on your work,
treat it as a report to act on.
"#;
const ROLE_QA_TEMPLATE: &str = r#"# Role: QA

You are QA on this team. You verify the Dev's output and report what you find to the
PM. Your access is read-only — you inspect and test, you do not modify code.

## What you do
You check whether delivered work actually does what it was meant to — against the
task's intent and any acceptance criteria the PM set. You find problems; you do not fix
them. Fixing is the Dev's job, on the PM's call.

## Reporting findings
Report findings to the PM, most serious first. For each one, be concrete: what is
wrong, where, and how you observed it — enough for the Dev to act without rediscovering
it. If something held up under verification, say so plainly; a clean pass is a useful
result.

## Boundaries
You report to the PM — not to the user, not directly to the Dev. If verification is
blocked — you cannot reproduce something, or the task's intent is unclear — raise that
with the PM.
"#;
const ROLE_GENERIC_TEMPLATE: &str = r#"# Role: Team Member

You are an agent on this team. The PM coordinates the team and is its only contact
with the user. Work under the PM's direction, report your progress and results to the
PM, and raise anything unclear or blocked with the PM. Do not open a conversation with
the user directly.
"#;

const KANBAN_TEMPLATE: &str = "# Project Kanban\n\n";
const DECISIONS_TEMPLATE: &str = "<!-- team/decisions.md -->\n# Team Decisions\n\n";

/// Pick the ROLE.md seed template for `role`. Case-insensitive on the
/// canonical labels `pm` / `dev` / `qa`; anything else (or empty) maps to
/// the generic charter so a custom role still gets a non-empty file.
fn role_template_for(role: &str) -> &'static str {
    match role.trim().to_ascii_lowercase().as_str() {
        "pm" => ROLE_PM_TEMPLATE,
        "dev" => ROLE_DEV_TEMPLATE,
        "qa" => ROLE_QA_TEMPLATE,
        _ => ROLE_GENERIC_TEMPLATE,
    }
}

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

/// Resolve `~/.opencrab/` (the user layer root). Errors if no home
/// directory can be resolved. Thin wrapper over [`crate::paths::user_root`].
pub(crate) fn user_data_dir() -> Result<PathBuf, BootstrapError> {
    crate::paths::user_root().ok_or(BootstrapError::HomeUnresolved)
}

/// Resolve `<cwd>/.opencrab/` (the project layer root). Pure path join — no
/// IO, no fallibility. Thin wrapper over [`crate::paths::project_root`].
pub(crate) fn project_data_dir(cwd: &Path) -> PathBuf {
    crate::paths::project_root(cwd)
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
    create_dir_all(&crate::paths::user_agents_dir(root))?;

    for agent in roster {
        let agent_dir = crate::paths::user_agent_dir(root, &agent.id);
        create_dir_all(&agent_dir)?;
        write_template_if_missing(
            &crate::paths::user_agent_soul_md(root, &agent.id),
            SOUL_TEMPLATE,
        )?;
        // P6 Step 4 — role charter, seeded by `agent.role`. Slot 25 in
        // CONTEXT_FILE_ORDER, sits between SOUL (slot 20) and IDENTITY
        // (slot 30).
        write_template_if_missing(
            &crate::paths::user_agent_role_md(root, &agent.id),
            role_template_for(&agent.role),
        )?;
        // P6 Step 7 — IDENTITY.md interpolates `agent.name` into the
        // `- Name:` row; everything else is byte-identical across agents.
        let identity = render_identity_template(&agent.name);
        write_template_if_missing(
            &crate::paths::user_agent_identity_md(root, &agent.id),
            &identity,
        )?;
        write_template_if_missing(
            &crate::paths::user_agent_user_md(root, &agent.id),
            USER_TEMPLATE,
        )?;
        write_template_if_missing(
            &crate::paths::user_agent_memory_md(root, &agent.id),
            MEMORY_TEMPLATE,
        )?;

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

    create_dir_all(&crate::paths::project_agents_dir(root))?;
    for agent in roster {
        let agent_dir = crate::paths::project_agent_dir(root, &agent.id);
        create_dir_all(&agent_dir)?;
        write_template_if_missing(
            &crate::paths::project_agent_kanban_md(root, &agent.id),
            KANBAN_TEMPLATE,
        )?;
    }

    create_dir_all(&crate::paths::project_team_dir(root))?;
    write_template_if_missing(
        &crate::paths::project_team_decisions_md(root),
        DECISIONS_TEMPLATE,
    )?;

    let teams_dir = crate::paths::project_team_events_dir(root, team_id);
    create_dir_all(&teams_dir)?;
    touch_empty_file_if_missing(&crate::paths::project_team_events_jsonl(root, team_id))?;

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
    let new_path = crate::paths::team_json(home);
    let old_path = crate::paths::project_team_json(&crate::paths::project_root(cwd));

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
    let legacy = crate::paths::team_json_legacy_sibling(old_path);
    fs::rename(old_path, &legacy).map_err(|err| BootstrapError::io(&legacy, err))
}

// ===========================================================================
// Path helpers for Step-2 callers (read/write through the user layer)
// ===========================================================================

/// `~/.opencrab/team.json`.
pub(crate) fn team_json_path() -> Result<PathBuf, BootstrapError> {
    Ok(crate::paths::team_json(&user_data_dir()?))
}

/// Atomic write of a team.json payload to the user-layer location. Used by
/// `team_config::commands` for the create/template-instantiate path; kept
/// here so the atomic-write primitive stays a single source of truth.
pub(crate) fn write_team_json_atomic(content: &[u8]) -> Result<(), BootstrapError> {
    let path = team_json_path()?;
    atomic_write(&path, content)
}

/// Tear down a workspace's **project-layer** OpenCrab state when that
/// workspace is removed: the `<cwd>/.opencrab/` tree, and only that.
///
/// Scoped to the project layer on purpose. `~/.opencrab/team.json` and
/// `~/.opencrab/agents/` are machine-global identity (Phase 4 Step 2 + the
/// v3.0 "one team per user, agent identity persists across workspaces"
/// rule). Workspace removal is a project-layer event and MUST NOT delete
/// them: doing so re-mints `team_<uuid>` / `agent_<uuid>` ids on the next
/// Team Mode entry and orphans every `~/.opencrab/agents/<id>/` directory.
/// See `docs/scratch/storage-identity-audit.md`.
///
/// Best-effort: a failure is logged but never returned. Workspace removal
/// has already succeeded by the time this runs, so a partial cleanup must
/// not surface as a removal error.
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
}

#[cfg(test)]
mod tests;
