import { useCallback, useEffect, useState, type ReactNode } from "react";
import { readTeamConfig } from "../../../services/tauri";
import type { TeamConfig } from "../types";
import { TeamEmptyState } from "./TeamEmptyState";
import { TeamMemberStrip } from "./TeamMemberStrip";
import { PlanReviewModal } from "./PlanReviewModal";
import { PlanReviewBadge } from "./PlanReviewBadge";
import { useTaskApprovalQueue } from "../hooks/useTaskApprovalQueue";

type TeamHomeProps = {
  workspaceId: string | null;
  // The reused normal-mode <Messages> element. Rendered as the main chat body
  // once a team exists; unused in the no-workspace / loading / empty states.
  messagesSlot: ReactNode;
  // Phase 2.B: which roster chip is highlighted, and the click handler that
  // lazy-bootstraps a non-correspondent agent's Codex thread + repoints chat.
  activeAgentId: string | null;
  onSelectAgent: (agentId: string) => void | Promise<void>;
  // Phase 2.C composer fix: signal up when the user has just created a team
  // via the empty-state modal so TeamMainApp's sidecar-start effect re-fires.
  onTeamCreated?: () => void;
  // Phase 2 pivot — sidecar startup / provision error surfaced as a banner
  // above the chat. `null` hides the banner; otherwise rendered with a
  // dismiss button. Cleared on workspace switch / teamReadyVersion bump
  // upstream.
  provisionError?: string | null;
  onDismissProvisionError?: () => void;
};

export function TeamHome({
  workspaceId,
  messagesSlot,
  activeAgentId,
  onSelectAgent,
  onTeamCreated,
  provisionError,
  onDismissProvisionError,
}: TeamHomeProps) {
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
        onCreated={(created) => {
          setTeam(created);
          onTeamCreated?.();
        }}
      />
    );
  }

  // Team exists: thin agent roster strip on top, the reused normal-mode chat
  // body (messages + streaming + history, all driven by activeThreadId) below.
  return (
    <TeamHomeReady
      workspaceId={workspaceId}
      team={team}
      activeAgentId={activeAgentId}
      onSelectAgent={onSelectAgent}
      messagesSlot={messagesSlot}
      provisionError={provisionError}
      onDismissProvisionError={onDismissProvisionError}
    />
  );
}

// Split out so we can call hooks (useTaskApprovalQueue) under the guard that
// `team` is non-null. Without this split, the hook order in the outer
// `TeamHome` would change between renders (team === undefined / null vs.
// team-loaded), violating React's rules-of-hooks.
type TeamHomeReadyProps = {
  workspaceId: string;
  team: TeamConfig;
  activeAgentId: string | null;
  onSelectAgent: (agentId: string) => void | Promise<void>;
  messagesSlot: ReactNode;
  provisionError?: string | null;
  onDismissProvisionError?: () => void;
};

function TeamHomeReady({
  workspaceId,
  team,
  activeAgentId,
  onSelectAgent,
  messagesSlot,
  provisionError,
  onDismissProvisionError,
}: TeamHomeReadyProps) {
  const approval = useTaskApprovalQueue({
    workspaceId,
    teamId: team.id,
    autoCloseOnEmpty: true,
  });

  return (
    <div className="team-mode-chat-layout">
      <TeamMemberStrip
        team={team}
        activeAgentId={activeAgentId}
        onSelectAgent={onSelectAgent}
        rightSlot={
          <PlanReviewBadge
            queue={approval.queue}
            onOpen={() => approval.open({ refreshOnOpen: true })}
          />
        }
      />
      {approval.isOpen ? (
        <PlanReviewModal
          workspaceId={workspaceId}
          teamId={team.id}
          agents={team.agents}
          queue={approval.queue}
          onTaskResolved={approval.remove}
          onTaskPatched={approval.patchInPlace}
          onRefresh={approval.refetch}
          onClose={approval.close}
        />
      ) : null}
      {provisionError ? (
        <div className="team-mode-error-banner" role="alert">
          <div className="team-mode-error-banner-body">
            <strong>Sidecar provisioning failed.</strong>
            <span> {provisionError}</span>
            <span className="team-mode-error-banner-hint">
              {" "}
              Check the dev terminal (sidecar stderr) or DevTools console for
              details, then switch workspaces or reload to retry.
            </span>
          </div>
          {onDismissProvisionError ? (
            <button
              type="button"
              className="team-mode-error-banner-dismiss"
              onClick={onDismissProvisionError}
              aria-label="Dismiss error"
            >
              ×
            </button>
          ) : null}
        </div>
      ) : null}
      <div className="team-mode-chat-main">{messagesSlot}</div>
    </div>
  );
}
