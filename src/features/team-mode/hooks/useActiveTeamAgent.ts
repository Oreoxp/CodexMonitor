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
//      the user correspondent's Codex thread via `setActiveThreadId` ONCE,
//      guarded by `initialActivationDoneRef` so that any later effect re-run
//      (which shouldn't happen with our deps list, but belt-and-suspenders)
//      cannot snap the user's chip selection back to the correspondent
//   4. exposes `selectAgent(agentId)` for chip clicks — re-points the chat
//      at the named agent's Codex thread
//
// Send / hydration / streaming all go through normal-mode `useThreads`
// unchanged; this hook only twiddles `activeThreadId`.
//
// IMPORTANT — why `setActiveThreadId` is in a ref and NOT in deps:
// `useThreads.setActiveThreadId` is built with `useCallback` whose deps
// include `state.activeThreadIdByWorkspace`. Every active-thread switch
// (chip click, sidecar repoint, etc.) rebuilds that callback. If we listed
// `setActiveThreadId` in the polling effect's deps, the chip click would
// rebuild the callback → effect re-fires → poller wakes up → calls
// `setActiveThreadId(correspondent.threadId, ws)` and clobbers the user's
// chip selection. Symptom: chip click looks like a no-op (briefly switches
// to the clicked Dev, then snaps back to PM). We mirror the latest setter
// into a ref so the effect can read it without subscribing.

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

  // Always-fresh mirror of `setActiveThreadId`; read via `.current` from the
  // polling effect / selectAgent so neither has to list the prop in deps.
  // Updated on every render (no deps array) so it lags by at most one render.
  const setActiveThreadIdRef = useRef(setActiveThreadId);
  useEffect(() => {
    setActiveThreadIdRef.current = setActiveThreadId;
  });

  // Once-per-(workspaceId, teamReadyVersion) latch. Reset at the top of each
  // effect run; flipped to `true` immediately after the correspondent is
  // activated. If the polling effect ever re-fires within the same
  // (workspaceId, teamReadyVersion) generation, the latch short-circuits the
  // automatic activation so the user's chip selection sticks.
  const initialActivationDoneRef = useRef(false);

  useEffect(() => {
    if (!workspaceId) {
      setActiveAgentId(null);
      teamRef.current = null;
      initialActivationDoneRef.current = false;
      return;
    }
    // New (workspaceId, teamReadyVersion) generation: re-allow exactly one
    // automatic activation. selectAgent is unaffected by this latch.
    initialActivationDoneRef.current = false;
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
          if (!initialActivationDoneRef.current) {
            setActiveAgentId(userAgent.id);
            setActiveThreadIdRef.current(userAgent.threadId, workspaceId);
            initialActivationDoneRef.current = true;
          }
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
  }, [workspaceId, teamReadyVersion]);

  // Stable callback (empty deps) — reads workspaceId / team / setter through
  // refs, so chip clicks never invalidate downstream callbacks that bind us.
  const selectAgent = useCallback((agentId: string) => {
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
    setActiveThreadIdRef.current(target.threadId, wsId);
  }, []);

  return { activeAgentId, selectAgent };
}
