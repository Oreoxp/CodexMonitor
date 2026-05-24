// Phase 5 Step 2 — locate the `opencrab-memory-mcp` binary.
//
// The Tauri host registers a per-agent memory-search MCP server in each
// agent's `thread/start` config; that registration needs the absolute path
// of the `opencrab-memory-mcp` executable to put in the server's `command`.
//
// Resolution mirrors `daemon_binary.rs`: an explicit env override first,
// then a sibling of the running executable (the dev `cargo` target dir —
// the supported path for now). Packaged-app placement (macOS `Resources/`,
// system bin dirs) is a follow-up; see the Phase 5 Step 2 close-out note.

use std::path::PathBuf;

/// Env var that overrides binary resolution. Used by tests, and available as
/// an escape hatch for deployments that place the binary off the exe dir.
const PATH_OVERRIDE_ENV: &str = "OPENCRAB_MEMORY_MCP_PATH";

pub(crate) fn memory_mcp_binary_candidates() -> &'static [&'static str] {
    if cfg!(windows) {
        &["opencrab-memory-mcp.exe", "opencrab_memory_mcp.exe"]
    } else {
        &["opencrab-memory-mcp", "opencrab_memory_mcp"]
    }
}

/// Resolve the absolute path of the `opencrab-memory-mcp` executable.
///
/// Order: `OPENCRAB_MEMORY_MCP_PATH` (if it points at a file) → a candidate
/// name next to the current executable. `Err` if nothing is found — callers
/// treat that as "skip memory-search registration", not a hard failure.
pub(crate) fn resolve_memory_mcp_binary_path() -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var(PATH_OVERRIDE_ENV) {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            let path = PathBuf::from(explicit);
            if path.is_file() {
                return Ok(path);
            }
            return Err(format!(
                "{PATH_OVERRIDE_ENV} is set but not a file: {}",
                path.display()
            ));
        }
    }

    let current_exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let dir = current_exe
        .parent()
        .ok_or_else(|| "cannot resolve the executable directory".to_string())?;

    let mut attempted = Vec::new();
    for name in memory_mcp_binary_candidates() {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
        attempted.push(candidate.display().to_string());
    }
    Err(format!(
        "opencrab-memory-mcp binary not found (tried: {})",
        attempted.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_prefer_the_hyphenated_name() {
        assert_eq!(memory_mcp_binary_candidates()[0], {
            if cfg!(windows) {
                "opencrab-memory-mcp.exe"
            } else {
                "opencrab-memory-mcp"
            }
        });
    }
}
