// @vitest-environment jsdom
//
// Phase 3 Step 4 polish (Section C) — coverage for the five hook paths
// the Step 4 ship couldn't reach via the modal alone:
//
//   1. workspace mismatch → drop event, don't update queue, don't open
//   2. duplicate task.id arriving via event → dedup, queue length stable
//   3. first event → queue populated + isOpen=true
//   4. autoCloseOnEmpty: drain via `remove()` → isOpen flips false
//   5. refetch() / open({refreshOnOpen:true}) → listTasks called +
//      queue REPLACED (not appended); per-id deduping still works
//
// Mocking strategy: stub `subscribeTasksProposed` at the events factory
// to capture its listener so the test can fire synthetic events; stub
// `listTasks` at the tauri service to control the refetch result. This
// matches the pattern in useRemoteThreadLiveConnection.test.tsx.

import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Task, TasksProposedEvent } from "../types/tasks";

// -- Event subscription mock ------------------------------------------------

type EventListener = (event: TasksProposedEvent) => void;
const eventListeners = new Set<EventListener>();

function emitTasksProposed(event: TasksProposedEvent) {
  for (const listener of eventListeners) {
    listener(event);
  }
}

vi.mock("@services/events", () => ({
  subscribeTasksProposed: (listener: EventListener) => {
    eventListeners.add(listener);
    return () => {
      eventListeners.delete(listener);
    };
  },
}));

// -- Tauri command mock -----------------------------------------------------

const listTasksMock = vi.fn();
vi.mock("@services/tauri", () => ({
  listTasks: (...args: unknown[]) => listTasksMock(...args),
}));

// Import AFTER mocks so the hook picks up the stubbed modules.
import { useTaskApprovalQueue } from "./useTaskApprovalQueue";

// -- Fixtures ---------------------------------------------------------------

const baseTask: Task = {
  id: "task-1",
  workspaceId: "ws-1",
  teamId: "team-1",
  assigneeAgentId: null,
  proposedByAgentId: "pm-alice",
  status: "proposed",
  title: "Task 1",
  body: "",
  approvedAt: null,
  completedAt: null,
  createdAt: "2026-05-17T00:00:00Z",
  updatedAt: "2026-05-17T00:00:00Z",
  planId: "plan_test_1",
  feedback: null,
};

function makeTask(overrides: Partial<Task>): Task {
  return { ...baseTask, ...overrides };
}

function makeEvent(
  overrides: Partial<TasksProposedEvent> & { tasks: Task[] },
): TasksProposedEvent {
  return {
    schemaVersion: 1,
    workspaceId: "ws-1",
    teamId: "team-1",
    proposedByAgentId: "pm-alice",
    ...overrides,
  };
}

describe("useTaskApprovalQueue", () => {
  beforeEach(() => {
    eventListeners.clear();
    listTasksMock.mockReset();
    // Default: refetch returns empty so the initial mount has no
    // hydration noise; tests opt in to non-empty via mockResolvedValueOnce.
    listTasksMock.mockResolvedValue([]);
  });
  afterEach(() => {
    eventListeners.clear();
  });

  it("drops events whose workspaceId does not match the active workspace", async () => {
    const { result } = renderHook(() =>
      useTaskApprovalQueue({ workspaceId: "ws-1", teamId: "team-1" }),
    );

    // Let the initial refetch settle.
    await waitFor(() => {
      expect(listTasksMock).toHaveBeenCalledTimes(1);
    });
    expect(result.current.queue).toHaveLength(0);
    expect(result.current.isOpen).toBe(false);

    // Event for a different workspace — must be ignored.
    act(() => {
      emitTasksProposed(
        makeEvent({
          workspaceId: "ws-other",
          tasks: [makeTask({ id: "stray" })],
        }),
      );
    });
    expect(result.current.queue).toHaveLength(0);
    expect(result.current.isOpen).toBe(false);
  });

  it("dedups task.ids that are already in the queue", async () => {
    listTasksMock.mockResolvedValueOnce([makeTask({ id: "task-A" })]);
    const { result } = renderHook(() =>
      useTaskApprovalQueue({ workspaceId: "ws-1", teamId: "team-1" }),
    );

    await waitFor(() => {
      expect(result.current.queue).toHaveLength(1);
    });

    // Event delivers the same id (e.g. router restarted and re-emitted)
    // plus one genuinely new id. Only the new one should append.
    act(() => {
      emitTasksProposed(
        makeEvent({
          tasks: [makeTask({ id: "task-A" }), makeTask({ id: "task-B" })],
        }),
      );
    });
    expect(result.current.queue).toHaveLength(2);
    expect(result.current.queue.map((t) => t.id).sort()).toEqual(["task-A", "task-B"]);
  });

  it("first event populates the queue and flips isOpen=true", async () => {
    const { result } = renderHook(() =>
      useTaskApprovalQueue({ workspaceId: "ws-1", teamId: "team-1" }),
    );
    await waitFor(() => {
      expect(listTasksMock).toHaveBeenCalled();
    });
    expect(result.current.isOpen).toBe(false);

    act(() => {
      emitTasksProposed(
        makeEvent({
          tasks: [makeTask({ id: "task-new" })],
        }),
      );
    });
    expect(result.current.queue).toHaveLength(1);
    expect(result.current.queue[0].id).toBe("task-new");
    expect(result.current.isOpen).toBe(true);
  });

  it("autoCloseOnEmpty flips isOpen=false when remove() drains the queue", async () => {
    const { result } = renderHook(() =>
      useTaskApprovalQueue({
        workspaceId: "ws-1",
        teamId: "team-1",
        autoCloseOnEmpty: true,
      }),
    );
    await waitFor(() => {
      expect(listTasksMock).toHaveBeenCalled();
    });

    // Populate two rows so we can verify the close fires only on the last.
    act(() => {
      emitTasksProposed(
        makeEvent({
          tasks: [makeTask({ id: "task-A" }), makeTask({ id: "task-B" })],
        }),
      );
    });
    expect(result.current.isOpen).toBe(true);

    act(() => {
      result.current.remove("task-A");
    });
    expect(result.current.queue).toHaveLength(1);
    expect(result.current.isOpen).toBe(true);

    act(() => {
      result.current.remove("task-B");
    });
    expect(result.current.queue).toHaveLength(0);
    expect(result.current.isOpen).toBe(false);
  });

  it("refetch replaces the queue authoritatively (not appends)", async () => {
    // Mount with a pre-existing event-driven queue, then drive `refetch`
    // to a smaller authoritative set; the queue should reflect the
    // refetch, not append-merge.
    listTasksMock.mockResolvedValueOnce([]); // initial mount
    const { result } = renderHook(() =>
      useTaskApprovalQueue({ workspaceId: "ws-1", teamId: "team-1" }),
    );
    await waitFor(() => {
      expect(listTasksMock).toHaveBeenCalledTimes(1);
    });

    act(() => {
      emitTasksProposed(
        makeEvent({
          tasks: [
            makeTask({ id: "stale-A" }),
            makeTask({ id: "stale-B" }),
            makeTask({ id: "still-here" }),
          ],
        }),
      );
    });
    expect(result.current.queue).toHaveLength(3);

    // Refetch returns only "still-here" plus a new row.
    listTasksMock.mockResolvedValueOnce([
      makeTask({ id: "still-here" }),
      makeTask({ id: "newly-arrived" }),
    ]);
    await act(async () => {
      await result.current.refetch();
    });
    expect(listTasksMock).toHaveBeenCalledTimes(2);
    // Replaced, not merged: stale-A and stale-B are gone.
    const idsAfter = result.current.queue.map((t) => t.id).sort();
    expect(idsAfter).toEqual(["newly-arrived", "still-here"]);
  });

  // Bonus: open({ refreshOnOpen: true }) is the badge's manual-reopen
  // path; verify it triggers the refetch + opens the modal.
  it("open({ refreshOnOpen: true }) triggers refetch and opens the modal", async () => {
    const { result } = renderHook(() =>
      useTaskApprovalQueue({ workspaceId: "ws-1", teamId: "team-1" }),
    );
    await waitFor(() => {
      expect(listTasksMock).toHaveBeenCalledTimes(1);
    });
    listTasksMock.mockResolvedValueOnce([makeTask({ id: "from-refresh" })]);
    act(() => {
      result.current.open({ refreshOnOpen: true });
    });
    expect(result.current.isOpen).toBe(true);
    await waitFor(() => {
      expect(listTasksMock).toHaveBeenCalledTimes(2);
    });
    await waitFor(() => {
      expect(result.current.queue.map((t) => t.id)).toEqual(["from-refresh"]);
    });
  });
});
