// Phase 2 pivot — picks the active team agent and points the reused
// normal-mode chat stack at that agent's Codex thread.
//
// Each agent IS a normal-mode Codex thread (provisioned by the sidecar at
// init via `sidecar_provision`, with the resulting `threadId` written back
// into `team.json`). This hook:
//   1. polls `read_team_config` until every agent has a `threadId` (waits
//      for sidecar provisioning to complete)
//   2. resolves the user-correspondent agent (the subscriber of the
//      `{ publisher: "user" }` subscription, fallback `agents[0]`)
//   3. on mount / `teamReadyVersion` change, points the normal-mode chat at
//      the user correspondent's Codex thread via `setActiveThreadId`
//   4. exposes `selectAgent(agentId)` for chip clicks — re-points the chat
//      at the named agent's Codex thread
//
// Send / hydration / streaming all go through normal-mode `useThreads`
// unchanged; this hook only twiddles `activeThreadId`.

import { useCallback, useEffect, useRef, useState } from "react";
import { readTeamConfig } from "@services/tauri";
import type { TeamConfig } from "../types";

const POLL_ATTEMPTS = 30;
const POLL_DELAY_MS = 400;
const USER_PUBLISHER = "user";

export type UseActiveTeamAgentResult = {
  activeAgentId: string | null;
  selectAgent: (agentId: string) => void;
};

function resolveUserCorrespondentId(team: TeamConfig): string | null {
  for (const sub of team.subscriptions) {
    if (sub.publisher === USER_PUBLISHER && sub.subscribers.length > 0) {
      return sub.subscribers[0]!;
    }
  }
  return team.agents[0]?.id ?? null;
}

function allAgentsHaveThreadIds(team: TeamConfig): boolean {
  return team.agents.every((a) => typeof a.threadId === "string" && a.threadId.length > 0);
}

export function useActiveTeamAgent(
  workspaceId: string | null,
  setActiveThreadId: (threadId: string | null, workspaceId?: string) => void,
  // Bumped by TeamMainApp when the user creates a team via the empty-state
  // modal so we re-poll team.json after sidecar provisioning kicks in.
  teamReadyVersion: number = 0,
): UseActiveTeamAgentResult {
  const [activeAgentId, setActiveAgentId] = useState<string | null>(null);
  // team.json snapshot from the last successful poll, used by selectAgent to
  // map agentId → threadId without an extra round-trip.
  const teamRef = useRef<TeamConfig | null>(null);
  // Stable workspace ref for selectAgent.
  const workspaceIdRef = useRef<string | null>(workspaceId);
  workspaceIdRef.current = workspaceId;

  useEffect(() => {
    if (!workspaceId) {
      setActiveAgentId(null);
      teamRef.current = null;
      return;
    }
    let cancelled = false;
    void (async () => {
      // Wait for sidecar provisioning to finish writing threadIds into
      // team.json. POLL_ATTEMPTS × POLL_DELAY_MS = 12s upper bound; longer
      // than the original 4.8s budget because sidecar spawn + N codex
      // thread/start round-trips can take a while.
      for (let attempt = 0; attempt < POLL_ATTEMPTS && !cancelled; attempt++) {
        try {
          const team = await readTeamConfig(workspaceId);
          if (cancelled) return;
          if (!team) {
            // No team yet (workspace lacks team.json) — give up; the empty
            // state will handle the create flow and bump teamReadyVersion.
            return;
          }
          if (!allAgentsHaveThreadIds(team)) {
            await new Promise((r) => setTimeout(r, POLL_DELAY_MS));
            continue;
          }
          teamRef.current = team;
          const userAgentId = resolveUserCorrespondentId(team);
          const userAgent = team.agents.find((a) => a.id === userAgentId);
          if (!userAgent || !userAgent.threadId) return;
          setActiveAgentId(userAgent.id);
          setActiveThreadId(userAgent.threadId, workspaceId);
          return;
        } catch (err) {
          if (attempt === POLL_ATTEMPTS - 1) {
            console.error("[team-mode] readTeamConfig failed after retries", err);
            return;
          }
          await new Promise((r) => setTimeout(r, POLL_DELAY_MS));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [workspaceId, setActiveThreadId, teamReadyVersion]);

  const selectAgent = useCallback(
    (agentId: string) => {
      const wsId = workspaceIdRef.current;
      const team = teamRef.current;
      if (!wsId || !team) return;
      const target = team.agents.find((a) => a.id === agentId);
      if (!target || !target.threadId) {
        console.warn(
          `[team-mode] selectAgent: agent ${agentId} has no bound threadId yet`,
        );
        return;
      }
      setActiveAgentId(agentId);
      setActiveThreadId(target.threadId, wsId);
    },
    [setActiveThreadId],
  );

  return { activeAgentId, selectAgent };
}
