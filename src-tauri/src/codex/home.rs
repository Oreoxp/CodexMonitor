use std::env;
use std::path::PathBuf;

use crate::types::WorkspaceEntry;

/// Default name of the OpenCrab home directory under the user's HOME.
///
/// 注意:OpenCrab 不再读取 `CODEX_HOME` / `OPENCRAB_HOME` 环境变量,
/// 始终使用此默认目录,避免与上游 codex 工具的环境冲突。
pub(crate) const OPENCRAB_HOME_DIR_NAME: &str = ".opencrab";

pub(crate) fn resolve_workspace_codex_home(
    _entry: &WorkspaceEntry,
    _parent_entry: Option<&WorkspaceEntry>,
) -> Option<PathBuf> {
    resolve_default_codex_home()
}

pub(crate) fn resolve_default_codex_home() -> Option<PathBuf> {
    // 不再读取 CODEX_HOME / OPENCRAB_HOME 环境变量,统一使用默认目录 ~/.opencrab
    resolve_home_dir().map(|home| home.join(OPENCRAB_HOME_DIR_NAME))
}

pub(crate) fn resolve_home_dir() -> Option<PathBuf> {
    if let Ok(value) = env::var("HOME") {
        if !value.trim().is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    if let Ok(value) = env::var("USERPROFILE") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{WorkspaceKind, WorkspaceSettings, WorktreeInfo};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        let _guard = ENV_LOCK.lock().expect("lock env");

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
        let _guard = ENV_LOCK.lock().expect("lock env");
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
