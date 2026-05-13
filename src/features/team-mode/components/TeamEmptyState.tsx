import { useState } from "react";
import type { TeamConfig } from "../types";
import { TeamCreateModal } from "./TeamCreateModal";

type TeamEmptyStateProps = {
  workspaceId: string;
  onCreated: (team: TeamConfig) => void;
};

export function TeamEmptyState({ workspaceId, onCreated }: TeamEmptyStateProps) {
  const [showModal, setShowModal] = useState(false);

  return (
    <div className="team-mode-empty">
      <div className="team-mode-empty-card">
        <h2 className="team-mode-empty-title">No team yet for this workspace</h2>
        <p className="team-mode-empty-body">
          A team is a named set of agents (PM, Dev, QA…) that collaborate on this
          project. Pick a template to get started.
        </p>
        <button
          type="button"
          className="team-mode-button team-mode-button-primary"
          onClick={() => setShowModal(true)}
        >
          Create Team
        </button>
      </div>
      {showModal ? (
        <TeamCreateModal
          workspaceId={workspaceId}
          onClose={() => setShowModal(false)}
          onCreated={onCreated}
        />
      ) : null}
    </div>
  );
}
