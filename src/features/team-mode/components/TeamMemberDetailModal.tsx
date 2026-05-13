import { ModalShell } from "../../design-system/components/modal/ModalShell";
import type { AgentConfig } from "../types";

type TeamMemberDetailModalProps = {
  agent: AgentConfig;
  onClose: () => void;
};

export function TeamMemberDetailModal({ agent, onClose }: TeamMemberDetailModalProps) {
  return (
    <ModalShell
      ariaLabel={`Agent ${agent.name} details`}
      onBackdropClick={onClose}
      cardClassName="team-mode-detail-card"
    >
      <header className="team-mode-detail-header">
        <h2 className="team-mode-detail-name">{agent.name}</h2>
        <span className="team-mode-member-role-badge">{agent.role}</span>
      </header>
      <dl className="team-mode-detail-body">
        <dt>Model</dt>
        <dd>{agent.model}</dd>

        <dt>Tools preset</dt>
        <dd>{agent.toolsPreset}</dd>

        <dt>Agent id</dt>
        <dd className="team-mode-detail-mono">{agent.id}</dd>

        <dt>System prompt</dt>
        <dd>
          <pre className="team-mode-detail-prompt">{agent.systemPromptTemplate}</pre>
        </dd>
      </dl>
      <footer className="team-mode-detail-footer">
        <button type="button" className="team-mode-button" onClick={onClose}>
          Close
        </button>
      </footer>
    </ModalShell>
  );
}
