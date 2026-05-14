import type { TeamConfig } from "../types";

type TeamMemberStripProps = {
  team: TeamConfig;
};

// Thin horizontal roster strip above the chat. Phase 1.1 has one agent (the
// PM); later phases add dev/qa chips. Not interactive yet — agent switching is
// post-Phase-1.2. `role` is shown purely as a label (§5.8: never branched on).
export function TeamMemberStrip({ team }: TeamMemberStripProps) {
  return (
    <div className="team-mode-strip">
      <span className="team-mode-strip-team-name">{team.name}</span>
      <div className="team-mode-strip-agents">
        {team.agents.map((agent) => (
          <span key={agent.id} className="team-mode-strip-chip">
            <span className="team-mode-strip-chip-name">{agent.name}</span>
            <span className="team-mode-strip-chip-role">{agent.role}</span>
          </span>
        ))}
      </div>
    </div>
  );
}
