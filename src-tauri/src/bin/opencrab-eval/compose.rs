// Bridge to the Node `compose.mts` script — single source of truth for
// prompt assembly is the sidecar TypeScript. The Rust harness shells out
// to `npx tsx` from the sidecar directory so module resolution sees
// sidecar/node_modules.
//
// Returns the bytes that `provisionAndStartRouter` would have sent to
// `thread/start` as `developerInstructions`, plus the kickoff text
// `composeFirstUserMessage` builds for the first user-role turn.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

/// Embedded compose script. Written to a tempfile at runtime; tsx
/// reads it from disk + does dynamic `import(...)` against sidecar source.
const COMPOSE_MTS: &str = include_str!("compose.mts");

/// Resolve the sidecar source root. The harness shells out from this dir
/// so npx finds `node_modules/.bin/tsx` and dynamic imports resolve.
/// Order: `OPENCRAB_EVAL_SIDECAR_ROOT` env override → walk up from the
/// running binary's path looking for a sibling `sidecar/` dir.
pub fn resolve_sidecar_root() -> Result<PathBuf, String> {
    if let Ok(value) = std::env::var("OPENCRAB_EVAL_SIDECAR_ROOT") {
        let path = PathBuf::from(value.trim());
        if !path.join("package.json").exists() {
            return Err(format!(
                "OPENCRAB_EVAL_SIDECAR_ROOT={} has no package.json",
                path.display()
            ));
        }
        return Ok(path);
    }
    // Default: <repo>/sidecar/, located by walking up from current_exe.
    let exe = std::env::current_exe().map_err(|err| format!("current_exe: {err}"))?;
    let mut cursor = exe.as_path();
    while let Some(parent) = cursor.parent() {
        let candidate = parent.join("sidecar");
        if candidate.join("package.json").exists() {
            return Ok(candidate);
        }
        cursor = parent;
    }
    Err(
        "could not locate sidecar/ — set OPENCRAB_EVAL_SIDECAR_ROOT to the absolute path"
            .to_string(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeOutput {
    pub developer_instructions: String,
    pub kickoff_prompt: String,
}

/// Run compose.mts. Writes the embedded script to `script_dir` (caller-owned;
/// usually the per-fixture tempdir) so its lifetime tracks the fixture.
///
/// `tools_mode = true` adds `--tools-mode` so compose.mts post-processes
/// the developer instructions to swap the production text-tag comm guide
/// for the Step 18 spike tool-call variant. The kickoff prompt and the
/// rest of the stable prefix are unchanged either way.
pub async fn compose(
    sidecar_root: &Path,
    script_dir: &Path,
    user_data_dir: &Path,
    project_data_dir: &Path,
    agent_id: &str,
    tools_mode: bool,
) -> Result<ComposeOutput, String> {
    std::fs::create_dir_all(script_dir)
        .map_err(|err| format!("mkdir {}: {err}", script_dir.display()))?;
    let script_path = script_dir.join("eval-compose.mts");
    std::fs::write(&script_path, COMPOSE_MTS)
        .map_err(|err| format!("write {}: {err}", script_path.display()))?;

    let mut cmd = Command::new("npx");
    cmd.arg("--yes")
        .arg("tsx")
        .arg(&script_path)
        .arg("--sidecar-root")
        .arg(sidecar_root)
        .arg("--user-data")
        .arg(user_data_dir)
        .arg("--project-data")
        .arg(project_data_dir)
        .arg("--agent-id")
        .arg(agent_id)
        .current_dir(sidecar_root);
    if tools_mode {
        cmd.arg("--tools-mode");
    }
    let output = timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| "compose.mts timed out after 30s".to_string())?
        .map_err(|err| format!("spawn npx tsx: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "compose.mts exited with {} — stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<ComposeOutput>(stdout.trim()).map_err(|err| {
        format!(
            "compose.mts stdout is not parseable JSON ({err}):\n{stdout}\n--- stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
