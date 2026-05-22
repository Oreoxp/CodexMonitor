// Team Mode types — manually mirrored from:
//   - Rust:    src-tauri/src/team_config/types.rs (+ commands.rs::TemplateInfo)
//   - sidecar: ../../sidecar/src/team/types.ts
// Three places must stay in sync. When you change a field, change all three.

export type ToolsPreset = "readonly" | "readwrite" | "full";

export type AgentConfig = {
  id: string;
  name: string;
  role: string;                       // free-form label; do NOT branch on it
  model: string;
  systemPromptTemplate: string;
  toolsPreset: ToolsPreset;
  // NOTE: no `threadId`. A Codex thread is workspace-scoped; the agent→thread
  // binding lives per-workspace and is read via `readWorkspaceThreads`, not
  // from this machine-global team config.
};

export type Subscription = {
  publisher: string;                  // agent id
  subscribers: string[];              // agent ids
  channels: string[];                 // metadata tags; do NOT branch on them
};

export type TeamConfig = {
  schemaVersion: 1;
  id: string;
  name: string;
  createdAt: string;                  // ISO timestamp
  templateId: string;
  agents: AgentConfig[];
  subscriptions: Subscription[];
};

export type TemplateInfo = {
  id: string;
  displayName: string;
  description: string;
};

// Per-workspace agent→Codex-thread bindings, read from
// `<cwd>/.opencrab/threads.json` via `readWorkspaceThreads`. Keyed by agent
// id; value is the bound Codex thread id. NOT part of the machine-global team
// config — a Codex thread is workspace-scoped.
export type WorkspaceThreads = Record<string, string>;
