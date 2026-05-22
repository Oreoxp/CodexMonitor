import type { ReactNode } from "react";

import type { TeamConfig, WorkspaceThreads } from "../types";

type TeamMemberStripProps = {
  team: TeamConfig;
  // Phase 2 pivot: highlighted chip + click handler. Each chip switches
  // `activeThreadId` to that agent's bound Codex thread (normal-mode
  // setActiveThreadId path); the composer/messages stack re-renders for
  // the new thread automatically. No idle/busy state — that lives in
  // normal-mode thread state and will be surfaced when we add a per-chip
  // processing indicator post-Phase-2.
  activeAgentId: string | null;
  onSelectAgent: (agentId: string) => void;
  // Per-workspace agent→Codex-thread bindings (`<cwd>/.opencrab/threads.json`).
  // An agent with no entry has not been provisioned in this workspace yet —
  // its chip stays disabled ("Provisioning…") until the binding lands.
  threadsByAgentId: WorkspaceThreads;
  // Phase 3 Step 4: optional slot at the right edge of the strip for the
  // "Plan review (N)" badge. Keeps the strip the layout owner; consumers
  // (TeamHome) decide what lives there.
  rightSlot?: ReactNode;
};

// Thin horizontal roster strip above the chat. One chip per `team.agents[]`.
// `role` is rendered purely as a label (§5.8: never branched on).
export function TeamMemberStrip({
  team,
  activeAgentId,
  onSelectAgent,
  threadsByAgentId,
  rightSlot,
}: TeamMemberStripProps) {
  return (
    <div className="team-mode-strip">
      <span className="team-mode-strip-team-name">{team.name}</span>
      <div className="team-mode-strip-agents">
        {team.agents.map((agent) => {
          const isActive = agent.id === activeAgentId;
          const ready = Boolean(threadsByAgentId[agent.id]);
          return (
            <button
              type="button"
              key={agent.id}
              className={
                "team-mode-strip-chip" +
                (isActive ? " team-mode-strip-chip-active" : "")
              }
              aria-pressed={isActive}
              disabled={!ready}
              title={ready ? agent.role : "Provisioning Codex thread…"}
              onClick={() => onSelectAgent(agent.id)}
            >
              <span className="team-mode-strip-chip-name">{agent.name}</span>
              <span className="team-mode-strip-chip-role">{agent.role}</span>
            </button>
          );
        })}
      </div>
      {rightSlot ? <div className="team-mode-strip-right">{rightSlot}</div> : null}
    </div>
  );
}
