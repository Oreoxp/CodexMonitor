import { useEffect, useRef } from "react";
import { ensureAgentThread } from "@services/tauri";

// Phase 1.1: resolves the active agent's (the PM's) Codex thread id from the
// sidecar on mount / workspace switch, and points the reused normal-mode chat
// stack at it via `setActiveThreadId`. Generic on purpose — the "active agent"
// is the PM today, any focused agent later. No agent-switching UI in Phase 1.2.
//
// Phase 1.3: after pointing at the thread, hydrate its history via the
// normal-mode resume path (`refreshThread` → invoke("resume_thread", {
// workspaceId, threadId })). That payload carries no prompt-shaping fields, so
// it is Resume-Discipline compliant (docs/architecture/opencrab-3.0-prompt-
// strategy.md). Fresh thread / resume failure → messages just stay empty.
//
// The sidecar is started in parallel by TeamMainApp's lifecycle effect, so
// `agent_ensure_thread` fails until that spawn + init completes — hence the
// bounded retry below.

const MAX_ATTEMPTS = 12;
const RETRY_DELAY_MS = 400;

export function useActiveAgentThreadId(
  workspaceId: string | null,
  setActiveThreadId: (threadId: string | null, workspaceId?: string) => void,
  refreshThread: (
    workspaceId: string,
    threadId: string,
  ) => Promise<string | null>,
): void {
  // Codex thread ids we've already kicked off history hydration for, for this
  // hook's lifetime. Prevents a duplicate resume on React StrictMode's
  // double-mount and on a workspace switch that lands back on the same thread.
  const hydratedRef = useRef<Set<string>>(new Set());

  useEffect(() => {
    if (!workspaceId) return;
    let cancelled = false;
    void (async () => {
      for (let attempt = 0; attempt < MAX_ATTEMPTS && !cancelled; attempt++) {
        try {
          const { codexThreadId } = await ensureAgentThread(workspaceId);
          if (cancelled || !codexThreadId) return;
          setActiveThreadId(codexThreadId, workspaceId);

          // Hydrate history once per thread id. An empty / fresh thread or a
          // resume failure is a valid state — leave messages empty, don't
          // surface an error.
          if (!cancelled && !hydratedRef.current.has(codexThreadId)) {
            hydratedRef.current.add(codexThreadId);
            try {
              await refreshThread(workspaceId, codexThreadId);
            } catch (err) {
              console.error(
                "[team-mode] thread history resume failed",
                err,
              );
            }
          }
          return;
        } catch (err) {
          if (attempt === MAX_ATTEMPTS - 1) {
            console.error(
              "[team-mode] ensureAgentThread failed after retries",
              err,
            );
            return;
          }
          await new Promise((resolve) => setTimeout(resolve, RETRY_DELAY_MS));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [workspaceId, setActiveThreadId, refreshThread]);
}
