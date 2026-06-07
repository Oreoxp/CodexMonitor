// P7 Roster (组织架构) — static config data source.
//
// Reads the REAL user-global roster (`~/.opencrab/team.json`) and shapes it
// into row view-models for RosterScreen. team.json is user-layer / global
// (Phase 4 Step 2), but the `read_team_config` command still needs a
// workspaceId as a migrate-precondition anchor — so we anchor on the first
// listed workspace. (TODO 2b: use the real ACTIVE workspace once the
// workspace + sidecar lifecycle is re-wired into the new shell — block 1
// deferred that plumbing.)
//
// Real fields wired here: name / role / model / toolsPreset (+ avatar color,
// autonomy badge, permission toggles — all DERIVED from toolsPreset, the only
// per-agent capability field). LIVE fields (status / activity / today usage /
// memory count / runtime) have no read source yet → placeholders, marked
// TODO(2b). Falls back to a design demo seed when no team/workspace exists so
// the screen is always demoable.
import { useEffect, useState } from "react";

import { readTeamConfig } from "@services/tauri";
import type { AgentConfig, Subscription, TeamConfig, ToolsPreset } from "../types";

const USER_PUBLISHER = "user";

// 5 design gradients; avatar color is a stable hash of the role string
// (per spec — the design's per-agent colors are placeholder examples).
const AVATAR_GRADIENTS = [
  "linear-gradient(135deg,#ff9f0a,#ff7a00)", // orange
  "linear-gradient(135deg,#0a84ff,#0060df)", // blue
  "linear-gradient(135deg,#a972ff,#7d3cff)", // purple
  "linear-gradient(135deg,#30c659,#1f9c45)", // green
  "linear-gradient(135deg,#ff453a,#d70015)", // red
];

function avatarGradient(role: string): string {
  let h = 0;
  for (let i = 0; i < role.length; i++) h = (h * 31 + role.charCodeAt(i)) >>> 0;
  return AVATAR_GRADIENTS[h % AVATAR_GRADIENTS.length]!;
}

export type Autonomy = { cls: "auto-1" | "auto-2" | "auto-3"; label: string; seg: 0 | 1 | 2 };

// toolsPreset → autonomy badge. The design's "access_mode" autonomy is not a
// stored per-agent field; toolsPreset (readonly|readwrite|full) is the real
// capability signal we map from. Display-only this block.
function autonomyFromPreset(p: ToolsPreset): Autonomy {
  switch (p) {
    case "readonly":
      return { cls: "auto-1", label: "手动批准", seg: 0 };
    case "readwrite":
      return { cls: "auto-2", label: "半自动", seg: 1 };
    case "full":
      return { cls: "auto-3", label: "全自动", seg: 2 };
  }
}

export type Permissions = { fs: boolean; exec: boolean; net: boolean; git: boolean };

// toolsPreset → 4 permission toggles (derived, read-only display).
// NOTE: independent 4-toggle permissions are explicitly OUT OF P7 SCOPE — these
// reflect the preset, they are not separately editable here. TODO(2b+).
function permsFromPreset(p: ToolsPreset): Permissions {
  switch (p) {
    case "readonly":
      return { fs: false, exec: false, net: false, git: false };
    case "readwrite":
      return { fs: true, exec: false, net: false, git: false };
    case "full":
      return { fs: true, exec: true, net: true, git: true };
  }
}

export type RosterAgent = {
  id: string;
  name: string;
  role: string;
  model: string;
  toolsPreset: ToolsPreset;
  avatar: string; // css gradient
  letter: string;
  autonomy: Autonomy;
  perms: Permissions;
  // PM/correspondent, derived from the { publisher: "user" } subscription.
  reportsTo: { name: string; letter: string; avatar: string } | null;
  workspacePath: string; // derived from the storage convention (placeholder-ish)
  // ---- LIVE placeholders (TODO 2b) ----
  status: "running" | "waiting" | "idle";
  activity: string | null;
};

export type RosterData = {
  teamName: string | null;
  agents: RosterAgent[];
  source: "config" | "demo";
  loading: boolean;
};

// Resolve the user-correspondent (PM) name — the subscriber of the
// { publisher: "user" } subscription, fallback agents[0]. Used for "汇报给".
function resolveCorrespondentId(team: TeamConfig): string | null {
  for (const sub of team.subscriptions) {
    if (sub.publisher === USER_PUBLISHER && sub.subscribers.length > 0) {
      return sub.subscribers[0]!;
    }
  }
  return team.agents[0]?.id ?? null;
}

function shape(team: TeamConfig): RosterAgent[] {
  const correspondentId = resolveCorrespondentId(team);
  const correspondent = team.agents.find((a) => a.id === correspondentId) ?? null;
  const reportsTo = correspondent
    ? {
        name: correspondent.name,
        letter: (correspondent.name.trim()[0] ?? "?").toUpperCase(),
        avatar: avatarGradient(correspondent.role),
      }
    : null;
  return team.agents.map((a) => ({
    id: a.id,
    name: a.name,
    role: a.role,
    model: a.model,
    toolsPreset: a.toolsPreset,
    avatar: avatarGradient(a.role),
    letter: (a.name.trim()[0] ?? "?").toUpperCase(),
    autonomy: autonomyFromPreset(a.toolsPreset),
    perms: permsFromPreset(a.toolsPreset),
    reportsTo: a.id === correspondentId ? null : reportsTo,
    workspacePath: `.opencrab/agents/${a.id}/worktree`,
    // LIVE — no source yet (TODO 2b): show neutral idle + em-dash.
    status: "idle",
    activity: null,
  }));
}

// Design demo seed — mirrors the handoff (Atlas/Pixel/Forge/Sentry/Warden),
// coarse roles so group-by-role yields 主管(1) / 开发(3) / 审查(1) like the
// design. Used only when no real team.json is found.
const DEMO_AGENTS: AgentConfig[] = [
  { id: "atlas", name: "Atlas", role: "主管", model: "gpt-5-codex", systemPromptTemplate: "", toolsPreset: "readwrite" },
  { id: "pixel", name: "Pixel", role: "开发", model: "gpt-5-codex", systemPromptTemplate: "", toolsPreset: "readwrite" },
  { id: "forge", name: "Forge", role: "开发", model: "gpt-5-codex", systemPromptTemplate: "", toolsPreset: "full" },
  { id: "sentry", name: "Sentry", role: "开发", model: "gpt-5-mini", systemPromptTemplate: "", toolsPreset: "full" },
  { id: "warden", name: "Warden", role: "审查", model: "gpt-5-codex", systemPromptTemplate: "", toolsPreset: "readonly" },
];
const DEMO_SUBS: Subscription[] = [
  { publisher: "user", subscribers: ["atlas"], channels: [] },
  { publisher: "atlas", subscribers: ["pixel", "forge", "sentry", "warden"], channels: [] },
];
const DEMO_TEAM: TeamConfig = {
  schemaVersion: 1,
  id: "demo",
  name: "hpio",
  createdAt: "",
  templateId: "demo",
  agents: DEMO_AGENTS,
  subscriptions: DEMO_SUBS,
};

// workspaceId is the resolved team workspace (from useTeamRuntime). When null
// or no team.json exists, falls back to the design demo seed so the screen is
// always demoable.
export function useTeamRoster(workspaceId: string | null): RosterData {
  const [data, setData] = useState<RosterData>({
    teamName: null,
    agents: [],
    source: "demo",
    loading: true,
  });

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      let team: TeamConfig | null = null;
      if (workspaceId) {
        try {
          team = await readTeamConfig(workspaceId);
        } catch {
          // Non-Tauri runtime / read error → demo seed below.
        }
      }
      if (cancelled) return;
      const cfg = team ?? DEMO_TEAM;
      setData({
        teamName: cfg.name,
        agents: shape(cfg),
        source: team ? "config" : "demo",
        loading: false,
      });
    })();
    return () => {
      cancelled = true;
    };
  }, [workspaceId]);

  return data;
}
