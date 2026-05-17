// Phase 3 Step 4 — approval queue + modal-open state for the active team.
//
// Responsibilities:
//   - Subscribe to the `tasks-proposed` Tauri event for the active workspace
//     and append each event's tasks onto an in-memory queue, dedup'd by id.
//   - Provide actions for the modal to remove a task from the queue after
//     approve / reject / archived-by-edit (caller decides when to remove).
//   - Provide an explicit `open()` action so the manual "待审批 (N)" badge
//     can re-open the modal after the user closed it.
//   - Refresh the queue from `list_tasks(workspace, team, ["proposed"])` on
//     workspace / team change. This covers the hydration path: pending rows
//     in the DB from a prior session land in the queue without waiting for
//     a fresh `<propose_plan>` emit.
//
// Append semantics: every `tasks-proposed` event extends the queue; we do
// NOT replace. If the user already had the modal open with one plan's
// rows mid-edit, a new event from PM appends new rows without dropping the
// in-progress edits. (Spec: "已开模态再来 event → append 到队列, 不覆盖,
// 不重置编辑状态.")
//
// Workspace scoping: we filter events by `workspaceId` so a backgrounded
// workspace's plan emissions don't pop a modal in front of the user who
// switched away.

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { listTasks } from "@services/tauri";
import { subscribeTasksProposed } from "@services/events";
import type {
  Task,
  TasksProposedEvent,
} from "../types/tasks";

export type TaskApprovalQueue = {
  /** Tasks waiting for the user's approve/reject decision. Order = arrival. */
  queue: Task[];
  /** Modal open flag. The modal auto-opens on event arrival and on a
   *  manual `open()`; the caller closes via `close()`. */
  isOpen: boolean;
  /** Open the modal. If the queue is empty AND `refreshOnOpen` is true,
   *  triggers an explicit refetch first (the manual-reopen badge path). */
  open: (options?: { refreshOnOpen?: boolean }) => void;
  /** Close the modal. Queue contents are NOT discarded; the user can
   *  re-open via the badge. */
  close: () => void;
  /** Remove a task from the queue (approve / reject / archived-by-edit).
   *  If the queue becomes empty AND `autoCloseOnEmpty` is set in opts,
   *  also closes the modal. */
  remove: (taskId: string) => void;
  /** Replace an existing task in the queue (post-`update_task` edit). */
  patchInPlace: (task: Task) => void;
  /** Manually trigger a refetch from `list_tasks(... [Proposed])`. Used
   *  on workspace/team change and by `open({ refreshOnOpen: true })`. */
  refetch: () => Promise<void>;
};

export type UseTaskApprovalQueueArgs = {
  workspaceId: string | null;
  teamId: string | null;
  /** When `true`, the modal auto-closes once the queue drains. */
  autoCloseOnEmpty?: boolean;
};

export function useTaskApprovalQueue(
  args: UseTaskApprovalQueueArgs,
): TaskApprovalQueue {
  const { workspaceId, teamId, autoCloseOnEmpty = true } = args;
  const [queue, setQueue] = useState<Task[]>([]);
  const [isOpen, setIsOpen] = useState(false);

  // Ref-mirror of workspaceId so the event handler can filter without
  // re-subscribing on every workspace flicker.
  const workspaceIdRef = useRef<string | null>(workspaceId);
  useEffect(() => {
    workspaceIdRef.current = workspaceId;
  }, [workspaceId]);

  const refetch = useCallback(async () => {
    if (!workspaceId || !teamId) {
      setQueue([]);
      return;
    }
    try {
      const rows = await listTasks(workspaceId, teamId, ["proposed"]);
      // De-dup by id: a fresh refetch is authoritative for the proposed
      // set; replace the queue rather than merge. Existing rows that are
      // still proposed stay (same id → same task object semantically); the
      // modal's per-row edit state keys off task.id so the user's draft
      // edits survive the refetch as long as the row is still proposed.
      setQueue(rows);
    } catch (err) {
      console.error("[team-mode] list_tasks for proposed queue failed", err);
    }
  }, [workspaceId, teamId]);

  // Initial / workspace-or-team-change refetch.
  useEffect(() => {
    refetch();
  }, [refetch]);

  // Subscribe to live `tasks-proposed` events; append rows scoped to the
  // active workspace + team.
  useEffect(() => {
    if (!workspaceId || !teamId) return;
    const unsubscribe = subscribeTasksProposed((event: TasksProposedEvent) => {
      if (event.workspaceId !== workspaceIdRef.current) {
        // Backgrounded workspace — do NOT pop a modal in front of the
        // user. The badge's manual-reopen flow + the next refetch will
        // pick this up when they switch back.
        return;
      }
      if (event.teamId !== teamId) {
        return;
      }
      setQueue((prev) => {
        const knownIds = new Set(prev.map((t) => t.id));
        const fresh = event.tasks.filter((t) => !knownIds.has(t.id));
        if (fresh.length === 0) {
          return prev;
        }
        return [...prev, ...fresh];
      });
      setIsOpen(true);
    });
    return unsubscribe;
  }, [workspaceId, teamId]);

  const open = useCallback(
    (options?: { refreshOnOpen?: boolean }) => {
      setIsOpen(true);
      if (options?.refreshOnOpen) {
        void refetch();
      }
    },
    [refetch],
  );

  const close = useCallback(() => {
    setIsOpen(false);
  }, []);

  const remove = useCallback(
    (taskId: string) => {
      setQueue((prev) => {
        const next = prev.filter((t) => t.id !== taskId);
        if (autoCloseOnEmpty && next.length === 0) {
          // schedule via state setter — closing inside the same setQueue
          // call would tear-down event subscription effects too early.
          setIsOpen(false);
        }
        return next;
      });
    },
    [autoCloseOnEmpty],
  );

  const patchInPlace = useCallback((task: Task) => {
    setQueue((prev) =>
      prev.map((existing) => (existing.id === task.id ? task : existing)),
    );
  }, []);

  return useMemo(
    () => ({ queue, isOpen, open, close, remove, patchInPlace, refetch }),
    [queue, isOpen, open, close, remove, patchInPlace, refetch],
  );
}
