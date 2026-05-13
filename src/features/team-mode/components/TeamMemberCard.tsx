import type { AgentConfig } from "../types";

type TeamMemberCardProps = {
  agent: AgentConfig;
  onClick: (agent: AgentConfig) => void;
};

export function TeamMemberCard({ agent, onClick }: TeamMemberCardProps) {
  return (
    <button
      type="button"
      className="team-mode-member-card"
      onClick={() => onClick(agent)}
    >
      <div className="team-mode-member-card-header">
        <span className="team-mode-member-name">{agent.name}</span>
        <span className="team-mode-member-role-badge">{agent.role}</span>
      </div>
      <div className="team-mode-member-model">{agent.model}</div>
    </button>
  );
}
