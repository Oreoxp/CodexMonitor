// P7-UI-3a — lightweight thread-items pipeline for the team-mode DM view.
//
// Reuses the existing DATA path (not the shared Messages/MessageRows renderer):
//   - history: resumeThread → extractThreadFromResponse → buildItemsFromThread
//   - live:    useAppServerEvents item events → buildConversationItem (upsert by id)
//              + agentMessage deltas accumulated into the streaming message item
// Both converters are pure (src/utils/threadItems.ts). This avoids the heavy
// useThreads (20 sub-hooks). send_message tool calls are already converted to
// assistant message bubbles by buildConversationItem, so no special handling.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { useAppServerEvents } from "@app/hooks/useAppServerEvents";
import { resumeThread } from "@services/tauri";
import { extractThreadFromResponse } from "@threads/utils/threadSummary";
import { buildConversationItem, buildItemsFromThread } from "@utils/threadItems";
import type { ConversationItem } from "@/types";

export type ThreadItems = {
  items: ConversationItem[];
  /** Optimistically append a user bubble on send (the live stream skips
   *  echoed user items to avoid a duplicate; resume reloads the real history). */
  appendLocalUser: (text: string) => void;
};

export function useThreadItems(
  workspaceId: string | null,
  threadId: string | null,
): ThreadItems {
  const [items, setItems] = useState<ConversationItem[]>([]);
  const localSeq = useRef(0);

  // Resume history when the thread changes.
  useEffect(() => {
    if (!workspaceId || !threadId) {
      setItems([]);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const response = await resumeThread(workspaceId, threadId);
        if (cancelled) return;
        const thread = extractThreadFromResponse(response);
        setItems(thread ? buildItemsFromThread(thread as Record<string, unknown>) : []);
      } catch {
        if (!cancelled) setItems([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [workspaceId, threadId]);

  const threadIdRef = useRef(threadId);
  threadIdRef.current = threadId;

  const upsert = useCallback((built: ConversationItem | null) => {
    if (!built) return;
    // User messages are shown optimistically on send; skip the echoed live copy.
    if (built.kind === "message" && built.role === "user") return;
    setItems((prev) => {
      const idx = prev.findIndex((p) => p.id === built.id);
      if (idx === -1) return [...prev, built];
      const next = prev.slice();
      next[idx] = built;
      return next;
    });
  }, []);

  const handlers = useMemo(
    () => ({
      onItemStarted: (_ws: string, tid: string, item: Record<string, unknown>) => {
        if (tid !== threadIdRef.current) return;
        upsert(buildConversationItem(item));
      },
      onItemCompleted: (_ws: string, tid: string, item: Record<string, unknown>) => {
        if (tid !== threadIdRef.current) return;
        upsert(buildConversationItem(item));
      },
      onAgentMessageDelta: (e: { threadId: string; itemId: string; delta: string }) => {
        if (e.threadId !== threadIdRef.current) return;
        setItems((prev) => {
          const idx = prev.findIndex((p) => p.id === e.itemId);
          if (idx === -1) {
            return [...prev, { id: e.itemId, kind: "message", role: "assistant", text: e.delta }];
          }
          const cur = prev[idx]!;
          if (cur.kind !== "message") return prev;
          const next = prev.slice();
          next[idx] = { ...cur, text: cur.text + e.delta };
          return next;
        });
      },
    }),
    [upsert],
  );

  useAppServerEvents(handlers);

  const appendLocalUser = useCallback((text: string) => {
    localSeq.current += 1;
    const id = `local-user-${localSeq.current}`;
    setItems((prev) => [...prev, { id, kind: "message", role: "user", text }]);
  }, []);

  return { items, appendLocalUser };
}
