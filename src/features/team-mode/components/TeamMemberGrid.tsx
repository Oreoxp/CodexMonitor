import { useState } from "react";
import type { AgentConfig, TeamConfig } from "../types";
import { TeamMemberCard } from "./TeamMemberCard";
import { TeamMemberDetailModal } from "./TeamMemberDetailModal";

type TeamMemberGridProps = {
  team: TeamConfig;
};

export function TeamMemberGrid({ team }: TeamMemberGridProps) {
  const [selected, setSelected] = useState<AgentConfig | null>(null);

  return (
    <div className="team-mode-grid-wrapper">
      <header className="team-mode-grid-header">
        <h1 className="team-mode-grid-title">{team.name}</h1>
        <div className="team-mode-grid-subtitle">
          {team.agents.length} {team.agents.length === 1 ? "member" : "members"}
        </div>
      </header>
      <div className="team-mode-grid">
        {team.agents.map((agent) => (
          <TeamMemberCard key={agent.id} agent={agent} onClick={setSelected} />
        ))}
      </div>
      {selected ? (
        <TeamMemberDetailModal agent={selected} onClose={() => setSelected(null)} />
      ) : null}
    </div>
  );
}
