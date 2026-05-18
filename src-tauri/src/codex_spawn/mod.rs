// Phase 4 Step 8 + Step 8-fix — codex-cli rollout-dir override wiring.
//
// OpenCrab spawns ONE `codex app-server` child per workspace
// (`StdioTransport::spawn` — see `codex_transport/stdio.rs`). The
// forked codex-cli (this repo's `codex-cli/` submodule, branch
// `opencrab-team-session-dir`) accepts:
//   * `--team-session-dir <path>` CLI flag (Step 8) — sets the
//     app-server-level default for all threads in the process. Used
//     by `StdioTransport::spawn` for the stdio fallback path.
//   * `ThreadStartParams.session_dir` (Step 8-fix) — per-thread
//     override that takes precedence over the CLI flag. Used by the
//     team-mode reverse-RPC path (`sidecar_session::inbound_ops::
//     handle_codex_start_thread`) so each agent's rollouts land in
//     its own `<agent_id>/team_sessions/<team_id>/<project_hash>/`.
//
// Two final paths:
//   * Workspace-scoped (CLI flag, stdio fallback):
//     `~/.opencrab/team_sessions/<project_hash>/`
//     Built via `team_session_dir_for_workspace` /
//     `ensure_team_session_dir_for_cwd`.
//   * Per-agent (Step 8-fix, thread/start path):
//     `~/.opencrab/agents/<agent_id>/team_sessions/<team_id>/<project_hash>/`
//     Built via `agent_team_session_dir` /
//     `ensure_agent_team_session_dir{_for_cwd}`.
//
// Per-thread separation within either dir comes from the rollout
// filename, which already embeds the thread_id (codex-cli's existing
// `rollout-<date>-<thread_id>.jsonl`).
//
// `project_hash` = SHA-256(canonical(cwd))[:12 hex chars]. canonical
// resolution prevents path drift across relative-vs-absolute /
// symlink-vs-realpath inputs. `agent_id` + `team_id` are validated
// against path-injection vectors before being joined.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const PROJECT_HASH_LEN_HEX: usize = 12;
const TEAM_SESSIONS_DIR: &str = "team_sessions";
const OPENCRAB_HOME_DIR_NAME: &str = ".opencrab";

#[derive(Debug)]
pub(crate) enum CodexSpawnError {
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    Mkdir {
        path: PathBuf,
        source: std::io::Error,
    },
    HomeUnresolved,
    /// Step 8-fix — `agent_id` / `team_id` fed into the per-agent path
    /// helpers failed validation (empty, path separator, `.` / `..`).
    /// `kind` is `"agent_id"` or `"team_id"`; `value` is the rejected
    /// raw string (logged for triage, not interpolated into a path).
    InvalidId {
        kind: &'static str,
        value: String,
        reason: AgentIdValidationError,
    },
}

impl std::fmt::Display for CodexSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexSpawnError::Canonicalize { path, source } => write!(
                f,
                "codex_spawn: canonicalize {} failed: {source}",
                path.display()
            ),
            CodexSpawnError::Mkdir { path, source } => {
                write!(f, "codex_spawn: mkdir {} failed: {source}", path.display())
            }
            CodexSpawnError::HomeUnresolved => {
                f.write_str("codex_spawn: $HOME unresolved; cannot place team_session_dir")
            }
            CodexSpawnError::InvalidId {
                kind,
                value,
                reason,
            } => write!(f, "codex_spawn: invalid {kind}={value:?}: {reason}"),
        }
    }
}

impl std::error::Error for CodexSpawnError {}

/// Resolve `$HOME` (or `$USERPROFILE` on Windows) and return
/// `<home>/.opencrab/`. Standalone variant: duplicates a small slice of
/// `bootstrap::user_data_dir` so this module stays daemon-binary
/// friendly (the daemon binary does NOT compile the `bootstrap`
/// module). Kept in sync with `crate::codex::home::resolve_home_dir`
/// by relying on the same env-var precedence (`HOME` then
/// `USERPROFILE`).
fn user_data_dir() -> Result<PathBuf, CodexSpawnError> {
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home);
        if !path.as_os_str().is_empty() {
            return Ok(path.join(OPENCRAB_HOME_DIR_NAME));
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let path = PathBuf::from(profile);
        if !path.as_os_str().is_empty() {
            return Ok(path.join(OPENCRAB_HOME_DIR_NAME));
        }
    }
    Err(CodexSpawnError::HomeUnresolved)
}

/// `SHA-256(canonical(cwd))[:12 hex chars]`.
///
/// `canonical` resolves symlinks + relative paths so two equivalent
/// workspace paths hash to the same value (no rollout split). Errors
/// propagate as `CodexSpawnError::Canonicalize` — caller must decide
/// whether to abort the spawn or fall back to no override.
pub(crate) fn project_hash(cwd: &Path) -> Result<String, CodexSpawnError> {
    let canonical = std::fs::canonicalize(cwd).map_err(|err| CodexSpawnError::Canonicalize {
        path: cwd.to_path_buf(),
        source: err,
    })?;
    let mut hasher = Sha256::new();
    // Hash the raw bytes of the canonical path. `to_string_lossy()`
    // would risk Unicode-replacement collisions on non-UTF-8 paths
    // (rare but real on Linux). On unix `OsStr::as_bytes()` is direct;
    // on Windows we fall back to `to_string_lossy` since OsStr is UTF-16
    // there and SHA over the lossy form is acceptable for our purposes
    // (Windows paths are practically always valid UTF-8 when produced
    // by `canonicalize`).
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(canonical.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        hasher.update(canonical.to_string_lossy().as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(PROJECT_HASH_LEN_HEX);
    for byte in digest.iter().take(PROJECT_HASH_LEN_HEX / 2) {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Compose `<user_data_dir>/team_sessions/<project_hash>/`. Pure path
/// construction over `project_hash`. No I/O beyond the canonicalize
/// inside `project_hash`.
pub(crate) fn team_session_dir_for_workspace(
    user_data_dir: &Path,
    cwd: &Path,
) -> Result<PathBuf, CodexSpawnError> {
    let hash = project_hash(cwd)?;
    Ok(user_data_dir.join(TEAM_SESSIONS_DIR).join(hash))
}

/// `team_session_dir_for_workspace` + `mkdir -p`. Call this immediately
/// before spawning `codex app-server`, then pass the returned path via
/// the `--team-session-dir <path>` CLI flag.
pub(crate) fn ensure_team_session_dir(
    user_data_dir: &Path,
    cwd: &Path,
) -> Result<PathBuf, CodexSpawnError> {
    let dir = team_session_dir_for_workspace(user_data_dir, cwd)?;
    std::fs::create_dir_all(&dir).map_err(|err| CodexSpawnError::Mkdir {
        path: dir.clone(),
        source: err,
    })?;
    Ok(dir)
}

/// One-shot convenience: resolve `$HOME`, build the per-workspace
/// `~/.opencrab/team_sessions/<project_hash>/`, mkdir -p, return the
/// path. Used at the `codex app-server` spawn site so callers don't
/// have to thread `user_data_dir` through every layer.
///
/// Logs to stderr + returns `None` on any failure (best-effort —
/// spawn proceeds without `--team-session-dir`, falling back to
/// upstream `<CODEX_HOME>/sessions/YYYY/MM/DD/`).
pub(crate) fn ensure_team_session_dir_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let user_dir = match user_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("[codex_spawn] {err}");
            return None;
        }
    };
    match ensure_team_session_dir(&user_dir, cwd) {
        Ok(dir) => Some(dir),
        Err(err) => {
            eprintln!("[codex_spawn] {err}; spawning without --team-session-dir");
            None
        }
    }
}

// ===========================================================================
// Phase 4 Step 8-fix — per-agent rollout dir (used by the thread/start path)
// ===========================================================================
//
// Workspace-scoped helpers above stay in place for the stdio fallback +
// the initial `codex app-server --team-session-dir <path>` spawn. The
// **per-agent** path below is invoked AT `thread/start` time, when both
// `agent_id` and `team_id` are known (team-mode reverse-RPC from the
// sidecar). The path goes one level deeper than the workspace-scoped
// version and gets carried into the codex-cli fork via the new
// `ThreadStartParams.session_dir` field.
//
// Final shape:
//   ~/.opencrab/agents/<agent_id>/team_sessions/<team_id>/<project_hash>/
//
// `agent_id` + `team_id` are validated to reject path-injection vectors
// (`/`, `\`, `.`, `..`, empty). They flow in from team.json so attacks
// are unlikely, but the file system gets the strict surface here.

#[derive(Debug)]
pub(crate) enum AgentIdValidationError {
    Empty,
    PathSeparator(char),
    DotOrDotDot,
}

impl std::fmt::Display for AgentIdValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentIdValidationError::Empty => f.write_str("id is empty"),
            AgentIdValidationError::PathSeparator(c) => {
                write!(f, "id contains path separator '{c}'")
            }
            AgentIdValidationError::DotOrDotDot => {
                f.write_str("id is `.` or `..` (path-traversal vector)")
            }
        }
    }
}

/// Reject ids that would let a malicious `team.json` break out of the
/// expected subdirectory. Empty is also rejected — an empty segment in
/// `join` would silently collapse and we want a loud error.
fn validate_path_segment(id: &str) -> Result<(), AgentIdValidationError> {
    if id.is_empty() {
        return Err(AgentIdValidationError::Empty);
    }
    if id == "." || id == ".." {
        return Err(AgentIdValidationError::DotOrDotDot);
    }
    for c in id.chars() {
        if c == '/' || c == '\\' || c == '\0' {
            return Err(AgentIdValidationError::PathSeparator(c));
        }
    }
    Ok(())
}

/// Compose
/// `<user_data_dir>/agents/<agent_id>/team_sessions/<team_id>/<project_hash>/`.
/// Pure path construction modulo the `project_hash` canonicalize. No
/// directory is created.
pub(crate) fn agent_team_session_dir(
    user_data_dir: &Path,
    agent_id: &str,
    team_id: &str,
    cwd: &Path,
) -> Result<PathBuf, CodexSpawnError> {
    validate_path_segment(agent_id).map_err(|err| CodexSpawnError::InvalidId {
        kind: "agent_id",
        value: agent_id.to_string(),
        reason: err,
    })?;
    validate_path_segment(team_id).map_err(|err| CodexSpawnError::InvalidId {
        kind: "team_id",
        value: team_id.to_string(),
        reason: err,
    })?;
    let hash = project_hash(cwd)?;
    Ok(user_data_dir
        .join("agents")
        .join(agent_id)
        .join(TEAM_SESSIONS_DIR)
        .join(team_id)
        .join(hash))
}

/// `agent_team_session_dir` + `mkdir -p`. Call before constructing the
/// `thread/start` request body so the codex-cli fork can write the
/// rollout file into a directory that already exists.
pub(crate) fn ensure_agent_team_session_dir(
    user_data_dir: &Path,
    agent_id: &str,
    team_id: &str,
    cwd: &Path,
) -> Result<PathBuf, CodexSpawnError> {
    let dir = agent_team_session_dir(user_data_dir, agent_id, team_id, cwd)?;
    std::fs::create_dir_all(&dir).map_err(|err| CodexSpawnError::Mkdir {
        path: dir.clone(),
        source: err,
    })?;
    Ok(dir)
}

/// One-shot: resolve `$HOME`, build the per-agent path, `mkdir -p`,
/// return the absolute path. Used by `handle_codex_start_thread` in
/// `sidecar_session/inbound_ops.rs` so the team-mode reverse-RPC can
/// fill in `ThreadStartParams.session_dir` without threading
/// `user_data_dir` through every call site.
///
/// Returns `Err` on validation failure or unresolved `$HOME`. Callers
/// MAY choose to log + fall back to the workspace-scoped CLI-flag
/// value, but the error type leaves that choice to them (unlike the
/// workspace-scoped `ensure_team_session_dir_for_cwd` which always
/// returns `Option`).
pub(crate) fn ensure_agent_team_session_dir_for_cwd(
    agent_id: &str,
    team_id: &str,
    cwd: &Path,
) -> Result<PathBuf, CodexSpawnError> {
    let user_dir = user_data_dir()?;
    ensure_agent_team_session_dir(&user_dir, agent_id, team_id, cwd)
}

#[cfg(test)]
mod tests;
