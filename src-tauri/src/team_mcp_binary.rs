// Phase 7 S6-3b — locate the `opencrab-team-mcp` stub binary.
//
// The Tauri host registers the `opencrab-team` MCP server (hosting the
// `send_message` / `propose_plan` tools) in each TEAM agent's `thread/start`
// config; that registration needs the absolute path of the `opencrab-team-mcp`
// executable for the server's `command`. Resolution mirrors
// `memory_mcp_binary.rs`: an explicit env override first, then a sibling of the
// running executable (the dev `cargo` target dir).
//
// The eval harness has its own private `resolve_team_mcp_binary` in
// `bin/opencrab-eval/main.rs`; binaries don't share with the lib crate, so the
// production resolver lives here.

use std::path::PathBuf;

/// Env var that overrides binary resolution. Used by tests, and an escape
/// hatch for deployments that place the binary off the exe dir.
const PATH_OVERRIDE_ENV: &str = "OPENCRAB_TEAM_MCP_PATH";

pub(crate) fn team_mcp_binary_candidates() -> &'static [&'static str] {
    if cfg!(windows) {
        &["opencrab-team-mcp.exe", "opencrab_team_mcp.exe"]
    } else {
        &["opencrab-team-mcp", "opencrab_team_mcp"]
    }
}

/// Resolve the absolute path of the `opencrab-team-mcp` executable.
///
/// Order: `OPENCRAB_TEAM_MCP_PATH` (if it points at a file) → a candidate name
/// next to the current executable. `Err` if nothing is found — callers treat
/// that as "skip team-tool registration", not a hard failure.
pub(crate) fn resolve_team_mcp_binary_path() -> Result<PathBuf, String> {
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
    for name in team_mcp_binary_candidates() {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
        attempted.push(candidate.display().to_string());
    }
    Err(format!(
        "opencrab-team-mcp binary not found (tried: {})",
        attempted.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_prefer_the_hyphenated_name() {
        assert_eq!(team_mcp_binary_candidates()[0], {
            if cfg!(windows) {
                "opencrab-team-mcp.exe"
            } else {
                "opencrab-team-mcp"
            }
        });
    }
}
