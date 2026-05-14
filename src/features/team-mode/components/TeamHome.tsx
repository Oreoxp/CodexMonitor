import { useCallback, useEffect, useState, type ReactNode } from "react";
import { readTeamConfig } from "../../../services/tauri";
import type { TeamConfig } from "../types";
import { TeamEmptyState } from "./TeamEmptyState";
import { TeamMemberStrip } from "./TeamMemberStrip";

type TeamHomeProps = {
  workspaceId: string | null;
  // The reused normal-mode <Messages> element. Rendered as the main chat body
  // once a team exists; unused in the no-workspace / loading / empty states.
  messagesSlot: ReactNode;
};

export function TeamHome({ workspaceId, messagesSlot }: TeamHomeProps) {
  const [team, setTeam] = useState<TeamConfig | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);

  const fetchTeam = useCallback((wsId: string) => {
    setError(null);
    setTeam(undefined);
    readTeamConfig(wsId)
      .then((result) => setTeam(result))
      .catch((err: unknown) => {
        setError(String(err));
        setTeam(null);
      });
  }, []);

  useEffect(() => {
    if (!workspaceId) {
      setTeam(undefined);
      setError(null);
      return;
    }
    fetchTeam(workspaceId);
  }, [workspaceId, fetchTeam]);

  if (!workspaceId) {
    return (
      <div className="team-mode-prompt">
        <div className="team-mode-prompt-card">
          <h2>Select a workspace</h2>
          <p>Pick a workspace from the sidebar to view or create its team.</p>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="team-mode-prompt">
        <div className="team-mode-prompt-card">
          <h2>Failed to load team</h2>
          <p className="team-mode-create-error">{error}</p>
          <button
            type="button"
            className="team-mode-button"
            onClick={() => fetchTeam(workspaceId)}
          >
            Retry
          </button>
        </div>
      </div>
    );
  }

  if (team === undefined) {
    return (
      <div className="team-mode-prompt">
        <div className="team-mode-prompt-card">
          <p>Loading…</p>
        </div>
      </div>
    );
  }

  if (team === null) {
    return (
      <TeamEmptyState
        workspaceId={workspaceId}
        onCreated={(created) => setTeam(created)}
      />
    );
  }

  // Team exists: thin agent roster strip on top, the reused normal-mode chat
  // body (messages + streaming + history, all driven by activeThreadId) below.
  return (
    <div className="team-mode-chat-layout">
      <TeamMemberStrip team={team} />
      <div className="team-mode-chat-main">{messagesSlot}</div>
    </div>
  );
}
