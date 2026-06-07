// P7-UI-2b — team runtime: re-wires the sidecar lifecycle that P7-UI-1 took
// offline when it retired TeamMainApp. Makes the team actually RUN from the new
// shell: resolve a workspace → connect → start sidecar → provision agent threads.
//
// All four steps are plain Tauri commands (connect_workspace / start_sidecar /
// sidecar_provision / stop_sidecar), and connect_workspace_core is idempotent —
// so this needs NONE of the normal-mode app bootstrap. Port of the old
// TeamMainApp lifecycle effect (TeamMainApp.tsx:225-257) + an explicit connect.
//
// Workspace resolution (the honest middle ground): we anchor on a RESOLVED
// workspace (prefer a connected one, else the first listed) rather than the
// user's live workspace *selection*. True live-follow of the active selection is
// owned by the normal-mode bootstrap (`useWorkspaceController` → `useWorkspaces`,
// activeWorkspaceId starts null and is bootstrap-driven); hoisting that above the
// mode switch would touch normal mode, so it's deliberately out of scope here.
import { useCallback, useEffect, useState } from "react";

import {
  connectWorkspace,
  listWorkspaces,
  readTeamConfig,
  sidecarProvision,
  startSidecar,
  stopSidecar,
} from "@services/tauri";

export type TeamRuntime = {
  /** The resolved workspace the team runs in (null = none available). */
  workspaceId: string | null;
  /** Sidecar start / provision failure, surfaced as a banner. */
  provisionError: string | null;
  dismissError: () => void;
  /** True once connect → start → provision completed for workspaceId. */
  provisioned: boolean;
  /** True while no team.json exists for the resolved workspace (create flow
   *  deferred — see report). */
  noTeam: boolean;
};

export function useTeamRuntime(): TeamRuntime {
  const [workspaceId, setWorkspaceId] = useState<string | null>(null);
  const [provisionError, setProvisionError] = useState<string | null>(null);
  const [provisioned, setProvisioned] = useState(false);
  const [noTeam, setNoTeam] = useState(false);

  // Resolve the target workspace once on mount.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const workspaces = await listWorkspaces();
        if (cancelled) return;
        const pick = workspaces.find((w) => w.connected) ?? workspaces[0] ?? null;
        setWorkspaceId(pick?.id ?? null);
      } catch {
        if (!cancelled) setWorkspaceId(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Lifecycle: connect → start sidecar → provision. Mirrors the old
  // TeamMainApp effect; cleanup stops the sidecar on workspace change / unmount.
  useEffect(() => {
    if (!workspaceId) return;
    const wsId = workspaceId;
    let cancelled = false;
    setProvisionError(null);
    setProvisioned(false);
    setNoTeam(false);
    void (async () => {
      try {
        const team = await readTeamConfig(wsId);
        if (cancelled) return;
        if (!team) {
          // No team.json for this workspace — nothing to run. The create flow
          // (TeamCreateModal + teamReadyVersion bump) is deferred; show a hint.
          setNoTeam(true);
          return;
        }
        await connectWorkspace(wsId);
        if (cancelled) return;
        await startSidecar(wsId);
        if (cancelled) return;
        // Provision any agent missing a Codex thread in this workspace (writes
        // agent→thread bindings to <cwd>/.opencrab/threads.json) + starts the
        // Tauri team router. Internally retries the connect_workspace race.
        await sidecarProvision(wsId);
        if (cancelled) return;
        setProvisioned(true);
      } catch (err) {
        if (!cancelled) {
          const msg = err instanceof Error ? err.message : String(err);
          console.error("[team-mode] sidecar connect / start / provision failed", err);
          setProvisionError(msg);
        }
      }
    })();
    return () => {
      cancelled = true;
      void stopSidecar(wsId).catch(() => {
        // No sidecar was running for this workspace — fine.
      });
    };
  }, [workspaceId]);

  const dismissError = useCallback(() => setProvisionError(null), []);

  return { workspaceId, provisionError, dismissError, provisioned, noTeam };
}
