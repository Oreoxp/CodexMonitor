// team.json schema — Rust mirror of sidecar/src/team/types.ts.
//
// Convention (must match the sidecar side):
//   - `role` is a free-form label (pm/dev/qa/...). Code MUST NOT branch on
//     role. Identity and capability come from subscriptions.
//   - `Subscription.channels` is metadata for UI / prompt hints. Code MUST
//     NOT branch on a channel name. Routing is publisher → subscribers.
//
// Phase 0a only enforces field-level validation via serde. Business rules
// (unique agent ids, subscription references existing agents, no
// self-subscription) are TODO for Phase 1 — sidecar's zod layer enforces
// them when reading; we may add a Rust-side check before write later.

use serde::{Deserialize, Serialize};

/// The literal pseudo-publisher representing the human user in the topology.
/// Mirrors `USER_PUBLISHER` in `sidecar/src/team/types.ts`. Subscriptions may
/// use it as `publisher` or as a `subscribers` entry; it is never an agent id.
pub(crate) const USER_PUBLISHER: &str = "user";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ToolsPreset {
    Readonly,
    Readwrite,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentConfig {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) model: String,
    pub(crate) system_prompt_template: String,
    pub(crate) tools_preset: ToolsPreset,
    // NOTE: no `thread_id` field. A Codex thread is workspace-scoped; the
    // agent→thread binding lives per-workspace at
    // `<cwd>/.opencrab/threads.json`, not in this machine-global team.json.
    // serde ignores the legacy `threadId` key on old files (no
    // `deny_unknown_fields`); the next write drops it.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Subscription {
    pub(crate) publisher: String,
    pub(crate) subscribers: Vec<String>,
    pub(crate) channels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamConfig {
    pub(crate) schema_version: u32,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) created_at: String,
    pub(crate) template_id: String,
    pub(crate) agents: Vec<AgentConfig>,
    pub(crate) subscriptions: Vec<Subscription>,
}

// TODO(phase1): add validate_team_config(): unique agent ids,
// subscriptions reference existing agents, no self-subscription,
// schema_version == 1. Sidecar's zod schema enforces these on read.
