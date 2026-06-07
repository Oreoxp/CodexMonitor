// P7-UI-2b — lightweight per-agent LIVE pipeline.
//
// Each team agent === one Codex thread. We subscribe to the app-server event
// hub directly via `useAppServerEvents` (the same primitive useThreads uses
// internally) with a MINIMAL handler set — no need for the heavy useThreads
// machinery. Thread events are attributed back to agents through the
// agent→thread map (`readWorkspaceThreads`).
//
// Wired now: status (running via turn-state, else idle) + activity one-liner
// (live agentMessage text, fallback coarse item label). NOT wired (placeholder):
// today's usage (only per-thread CUMULATIVE tokens exist — no daily rollup
// source) and memory counts (no read path); waiting/paused statuses (no signal
// beyond turn-active yet).
import { useEffect, useMemo, useRef, useState } from "react";

import { useAppServerEvents } from "@app/hooks/useAppServerEvents";
import { readWorkspaceThreads } from "@services/tauri";

export type AgentStatus = "running" | "idle" | "waiting" | "paused";
export type AgentLive = { status: AgentStatus; activity: string | null };

export type TeamLive = {
  /** agentId → live status/activity. Absent agent = no live data yet. */
  byAgentId: Record<string, AgentLive>;
  /** number of agents currently running a turn. */
  runningCount: number;
  /** agentId → bound Codex thread id (from readWorkspaceThreads). */
  threadByAgentId: Record<string, string>;
};

const POLL_ATTEMPTS = 30;
const POLL_DELAY_MS = 500;

function oneLine(text: string): string | null {
  const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
  const last = lines[lines.length - 1];
  if (!last) return null;
  return last.length > 72 ? `${last.slice(0, 72)}…` : last;
}

// Coarse activity label from an item/started payload (untyped Record) — used
// only as a fallback before any agentMessage text streams.
function coarseItemLabel(item: Record<string, unknown>): string | null {
  const type = typeof item.type === "string" ? item.type : typeof item.item_type === "string" ? (item.item_type as string) : "";
  if (type.includes("command")) return "执行命令…";
  if (type.includes("fileChange") || type.includes("file_change")) return "编辑文件…";
  if (type.includes("reasoning")) return "思考中…";
  if (type.includes("agentMessage") || type.includes("agent_message")) return "回复中…";
  return type ? "工作中…" : null;
}

type ThreadState = { running: boolean; activity: string | null };

export function useTeamLive(
  workspaceId: string | null,
  provisioned: boolean,
): TeamLive {
  const [threadToAgent, setThreadToAgent] = useState<Record<string, string>>({});
  const [threadByAgentId, setThreadByAgentId] = useState<Record<string, string>>({});
  const [byThread, setByThread] = useState<Record<string, ThreadState>>({});
  // Per-thread current-message accumulation buffer (reset each turn).
  const bufRef = useRef<Record<string, string>>({});

  // Poll readWorkspaceThreads until the agent→thread map is populated (sidecar
  // writes it during provisioning). Re-run when provisioning completes.
  useEffect(() => {
    if (!workspaceId) {
      setThreadToAgent({});
      setThreadByAgentId({});
      return;
    }
    let cancelled = false;
    void (async () => {
      for (let attempt = 0; attempt < POLL_ATTEMPTS && !cancelled; attempt++) {
        try {
          const map = await readWorkspaceThreads(workspaceId); // agentId → threadId
          if (cancelled) return;
          const inverse: Record<string, string> = {};
          for (const [agentId, threadId] of Object.entries(map)) {
            if (threadId) inverse[threadId] = agentId;
          }
          if (Object.keys(inverse).length > 0) {
            setThreadToAgent(inverse);
            setThreadByAgentId(map);
            return;
          }
        } catch {
          // ignore; retry
        }
        await new Promise((r) => setTimeout(r, POLL_DELAY_MS));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [workspaceId, provisioned]);

  // Subscribe to thread turn/item events. Handlers are stable (state setters +
  // refs), so a single subscription lives for the hook's lifetime.
  const handlers = useMemo(
    () => ({
      onTurnStarted: (_ws: string, threadId: string) => {
        bufRef.current[threadId] = "";
        setByThread((prev) => ({
          ...prev,
          [threadId]: { running: true, activity: prev[threadId]?.activity ?? null },
        }));
      },
      onAgentMessageDelta: (e: { threadId: string; delta: string }) => {
        bufRef.current[e.threadId] = (bufRef.current[e.threadId] ?? "") + e.delta;
        const activity = oneLine(bufRef.current[e.threadId] ?? "");
        setByThread((prev) => ({
          ...prev,
          [e.threadId]: { running: prev[e.threadId]?.running ?? true, activity },
        }));
      },
      onItemStarted: (_ws: string, threadId: string, item: Record<string, unknown>) => {
        setByThread((prev) => {
          // Only fill a coarse label if we have no streamed message text yet.
          if (prev[threadId]?.activity) return prev;
          return {
            ...prev,
            [threadId]: { running: prev[threadId]?.running ?? true, activity: coarseItemLabel(item) },
          };
        });
      },
      onTurnCompleted: (_ws: string, threadId: string) => {
        setByThread((prev) => ({
          ...prev,
          [threadId]: { running: false, activity: prev[threadId]?.activity ?? null },
        }));
      },
      onTurnError: (_ws: string, threadId: string) => {
        setByThread((prev) => ({
          ...prev,
          [threadId]: { running: false, activity: prev[threadId]?.activity ?? null },
        }));
      },
    }),
    [],
  );

  useAppServerEvents(handlers);

  return useMemo<TeamLive>(() => {
    const byAgentId: Record<string, AgentLive> = {};
    let runningCount = 0;
    for (const [threadId, st] of Object.entries(byThread)) {
      const agentId = threadToAgent[threadId];
      if (!agentId) continue;
      const status: AgentStatus = st.running ? "running" : "idle";
      if (st.running) runningCount += 1;
      byAgentId[agentId] = { status, activity: st.activity };
    }
    return { byAgentId, runningCount, threadByAgentId };
  }, [byThread, threadToAgent, threadByAgentId]);
}
