use std::path::PathBuf;

use crate::types::WorkspaceEntry;

// Step 4: the `.opencrab` literal + home resolution moved to `crate::paths`.
// This name is kept test-only so the in-module tests below keep compiling
// without re-introducing the literal into production code.
#[cfg(test)]
const OPENCRAB_HOME_DIR_NAME: &str = crate::paths::OPENCRAB_DIR;

/// Resolve the codex-home directory for a workspace. OpenCrab ignores the
/// `CODEX_HOME` / `OPENCRAB_HOME` env vars and always uses the shared
/// `~/.opencrab/` user layer — see [`crate::paths`].
pub(crate) fn resolve_workspace_codex_home(
    _entry: &WorkspaceEntry,
    _parent_entry: Option<&WorkspaceEntry>,
) -> Option<PathBuf> {
    resolve_default_codex_home()
}

/// `~/.opencrab/` — the default codex-home. Thin wrapper over
/// [`crate::paths::user_root`]; retained because several callers reference
/// this name.
pub(crate) fn resolve_default_codex_home() -> Option<PathBuf> {
    crate::paths::user_root()
}

/// Resolve the user's home directory. Thin wrapper over
/// [`crate::paths::home_dir`]; retained because `shared::workspaces_core`
/// callers reference this name.
pub(crate) fn resolve_home_dir() -> Option<PathBuf> {
    crate::paths::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{WorkspaceKind, WorkspaceSettings, WorktreeInfo};

    fn workspace_entry(kind: WorkspaceKind, path: &str) -> WorkspaceEntry {
        let worktree = if kind.is_worktree() {
            Some(WorktreeInfo {
                branch: "feature/test".to_string(),
            })
        } else {
            None
        };
        WorkspaceEntry {
            id: "workspace-id".to_string(),
            name: "workspace".to_string(),
            path: path.to_string(),
            kind,
            parent_id: None,
            worktree,
            settings: WorkspaceSettings::default(),
        }
    }

    #[test]
    fn workspace_codex_home_resolves_to_opencrab_default() {
        let entry = workspace_entry(WorkspaceKind::Main, "/repo");
        let _guard = crate::paths::ENV_LOCK.lock().expect("lock env");

        // 即使设置了 CODEX_HOME / OPENCRAB_HOME,我们也忽略它们。
        let prev_codex_home = std::env::var("CODEX_HOME").ok();
        let prev_opencrab_home = std::env::var("OPENCRAB_HOME").ok();
        std::env::set_var("CODEX_HOME", "/tmp/codex-global");
        std::env::set_var("OPENCRAB_HOME", "/tmp/opencrab-global");

        let home_dir = std::env::temp_dir().join("opencrab-home-test");
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home_dir);

        let resolved = resolve_workspace_codex_home(&entry, None);
        assert_eq!(resolved, Some(home_dir.join(OPENCRAB_HOME_DIR_NAME)));

        match prev_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match prev_opencrab_home {
            Some(value) => std::env::set_var("OPENCRAB_HOME", value),
            None => std::env::remove_var("OPENCRAB_HOME"),
        }
        match prev_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn default_codex_home_uses_opencrab_subdirectory() {
        let _guard = crate::paths::ENV_LOCK.lock().expect("lock env");
        let home_dir = std::env::temp_dir().join("opencrab-default-test");
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home_dir);

        let resolved = resolve_default_codex_home();
        assert_eq!(resolved, Some(home_dir.join(".opencrab")));

        match prev_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
