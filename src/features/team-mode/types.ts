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
  // Phase 2 pivot: bound normal-mode Codex thread id, populated by sidecar
  // provisioning. Frontend treats `agents[].threadId` as the only source of
  // truth for "which Codex thread is this agent" — no parallel agent-<id>
  // LangGraph thread anymore.
  threadId?: string;
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
