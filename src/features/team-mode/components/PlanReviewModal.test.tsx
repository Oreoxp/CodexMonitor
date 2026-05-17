// @vitest-environment jsdom
//
// Phase 3 Step 4 smoke for PlanReviewModal.
// Coverage:
//   1. Renders one card per queue entry, with title/body/assignee defaults
//      sourced from the Task row.
//   2. Edit-then-approve path: dirty title => update_task is called first
//      with a `{ title }`-only patch; approve_task fires after update
//      resolves, with the actor="user". The card is then removed from the
//      queue.
//   3. pm_notified=false renders the warning chip text and does NOT remove
//      the card before the chip has a chance to render.
//
// Mocking strategy: mock the `@services/tauri` wrapper directly (not the
// raw @tauri-apps/api/core invoke). This isolates the modal from Tauri
// runtime details — same pattern other test files in this codebase use.

import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ApprovalResult, Task } from "../types/tasks";
import type { AgentConfig } from "../types";

const approveTaskMock = vi.fn();
const rejectTaskMock = vi.fn();
const updateTaskMock = vi.fn();

vi.mock("@services/tauri", () => ({
  approveTask: (...args: unknown[]) => approveTaskMock(...args),
  rejectTask: (...args: unknown[]) => rejectTaskMock(...args),
  updateTask: (...args: unknown[]) => updateTaskMock(...args),
}));

import { PlanReviewModal } from "./PlanReviewModal";

const baseTask: Task = {
  id: "task-1",
  workspaceId: "ws-1",
  teamId: "team-1",
  assigneeAgentId: null,
  proposedByAgentId: "pm-alice",
  status: "proposed",
  title: "Original title",
  body: "Original body",
  approvedAt: null,
  completedAt: null,
  createdAt: "2026-05-17T00:00:00Z",
  updatedAt: "2026-05-17T00:00:00Z",
  planId: "plan_test_1",
  feedback: null,
};

const agents: AgentConfig[] = [
  {
    id: "pm-alice",
    name: "Alice",
    role: "pm",
    model: "gpt-5",
    systemPromptTemplate: "",
    toolsPreset: "readonly",
  },
  {
    id: "dev-bob",
    name: "Bob",
    role: "dev",
    model: "gpt-5",
    systemPromptTemplate: "",
    toolsPreset: "readwrite",
  },
];

function approvalResult(overrides: Partial<ApprovalResult> = {}): ApprovalResult {
  return {
    task: { ...baseTask, status: "ready" },
    pmNotified: true,
    ...overrides,
  };
}

describe("PlanReviewModal", () => {
  beforeEach(() => {
    approveTaskMock.mockReset();
    rejectTaskMock.mockReset();
    updateTaskMock.mockReset();
  });
  afterEach(() => {
    // The vitest config in this repo doesn't enable @testing-library auto-
    // cleanup globally; without an explicit cleanup the next test's render
    // sees the previous mount's DOM and getByText matches multiple buttons.
    cleanup();
  });

  it("renders one card per queue entry with default values from the task", () => {
    const { getByDisplayValue, getByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={vi.fn()}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    expect(getByDisplayValue("Original title")).toBeTruthy();
    expect(getByDisplayValue("Original body")).toBeTruthy();
    expect(getByText(/Plan review/i)).toBeTruthy();
  });

  it("edit-then-approve calls update_task first then approve_task", async () => {
    updateTaskMock.mockResolvedValue({ ...baseTask, title: "Edited title" });
    approveTaskMock.mockResolvedValue(approvalResult());
    const onTaskResolved = vi.fn();
    const onTaskPatched = vi.fn();

    const { getByDisplayValue, getByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={onTaskPatched}
        onClose={vi.fn()}
      />,
    );

    const titleInput = getByDisplayValue("Original title") as HTMLInputElement;
    fireEvent.change(titleInput, { target: { value: "Edited title" } });

    fireEvent.click(getByText("Approve"));

    await waitFor(() => {
      expect(updateTaskMock).toHaveBeenCalledTimes(1);
    });
    expect(updateTaskMock).toHaveBeenCalledWith(
      "ws-1",
      "task-1",
      { title: "Edited title" },
      "user",
    );
    await waitFor(() => {
      expect(approveTaskMock).toHaveBeenCalledTimes(1);
    });
    expect(approveTaskMock).toHaveBeenCalledWith("ws-1", "task-1", "user");
    await waitFor(() => {
      expect(onTaskResolved).toHaveBeenCalledWith("task-1");
    });
    // update_task's patched return is reported up so the queue can refresh
    // the in-place row before the resolve removes it.
    expect(onTaskPatched).toHaveBeenCalledWith(
      expect.objectContaining({ title: "Edited title" }),
    );
  });

  it("clean approve skips update_task and just calls approve_task", async () => {
    approveTaskMock.mockResolvedValue(approvalResult());
    const onTaskResolved = vi.fn();
    const { getByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    fireEvent.click(getByText("Approve"));

    await waitFor(() => {
      expect(approveTaskMock).toHaveBeenCalledTimes(1);
    });
    expect(updateTaskMock).not.toHaveBeenCalled();
  });

  it("renders the pmNotified=false warning chip after approve", async () => {
    approveTaskMock.mockResolvedValue(approvalResult({ pmNotified: false }));
    // Use a never-resolving onTaskResolved so the card stays mounted long
    // enough for us to inspect the chip. (In production the row is removed
    // after the warning chip first renders; this test pauses the removal
    // so we can assert on the warning DOM.)
    const onTaskResolved = vi.fn();
    const { getByText, queryByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    fireEvent.click(getByText("Approve"));

    await waitFor(() => {
      expect(approveTaskMock).toHaveBeenCalled();
    });
    await waitFor(() => {
      expect(
        queryByText(/PM didn't receive the system note/i),
      ).toBeTruthy();
    });
  });

  it("rejecting with feedback passes the trimmed feedback to reject_task", async () => {
    rejectTaskMock.mockResolvedValue(approvalResult({ pmNotified: true }));
    const onTaskResolved = vi.fn();
    const { getByText, getByPlaceholderText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    fireEvent.click(getByText("Reject…"));
    const feedback = getByPlaceholderText(/Tell PM why/i);
    fireEvent.change(feedback, {
      target: { value: "  not a good fit  " },
    });
    fireEvent.click(getByText("Confirm reject"));

    await waitFor(() => {
      expect(rejectTaskMock).toHaveBeenCalledWith(
        "ws-1",
        "task-1",
        "not a good fit",
        "user",
      );
    });
    await waitFor(() => {
      expect(onTaskResolved).toHaveBeenCalledWith("task-1");
    });
  });

  it("empty-queue mount renders an explicit empty state", () => {
    const { getByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[]}
        onTaskResolved={vi.fn()}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    expect(getByText(/No plans waiting/i)).toBeTruthy();
  });

  // -- Step 4 polish (Section A) --------------------------------------------

  it("pmNotified=true keeps the auto-resolve fast-path (regression)", async () => {
    approveTaskMock.mockResolvedValue(approvalResult({ pmNotified: true }));
    const onTaskResolved = vi.fn();
    const { getByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    fireEvent.click(getByText("Approve"));
    await waitFor(() => {
      expect(onTaskResolved).toHaveBeenCalledWith("task-1");
    });
  });

  it("pmNotified=false keeps the card mounted in resolved-with-warning state", async () => {
    approveTaskMock.mockResolvedValue(approvalResult({ pmNotified: false }));
    const onTaskResolved = vi.fn();
    const { container, getByText, queryByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    fireEvent.click(getByText("Approve"));
    await waitFor(() => {
      expect(approveTaskMock).toHaveBeenCalled();
    });
    // Resolved-with-warning card: warning chip + Dismiss button present;
    // approve_task was NOT followed by onTaskResolved.
    await waitFor(() => {
      expect(queryByText(/Approved — PM didn't receive/i)).toBeTruthy();
    });
    expect(queryByText("Got it")).toBeTruthy();
    expect(onTaskResolved).not.toHaveBeenCalled();
    // Fields disabled — pick the title input explicitly so we don't catch
    // an unrelated disabled control.
    const titleInput = container.querySelector(
      `li[data-task-id="task-1"] input.team-mode-plan-review-input`,
    ) as HTMLInputElement | null;
    expect(titleInput?.disabled).toBe(true);
    // The Approve/Reject buttons must NOT be present any longer.
    expect(queryByText("Approve")).toBeNull();
    expect(queryByText("Reject…")).toBeNull();
    // data-state attribute pinned for e2e scraping.
    const card = container.querySelector(`li[data-task-id="task-1"]`);
    expect(card?.getAttribute("data-state")).toBe("resolved-warning");
  });

  it("Dismiss on a resolved-with-warning card calls onTaskResolved", async () => {
    approveTaskMock.mockResolvedValue(approvalResult({ pmNotified: false }));
    const onTaskResolved = vi.fn();
    const { getByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    fireEvent.click(getByText("Approve"));
    await waitFor(() => {
      expect(getByText("Got it")).toBeTruthy();
    });
    fireEvent.click(getByText("Got it"));
    expect(onTaskResolved).toHaveBeenCalledWith("task-1");
  });

  it("reject + pmNotified=false also persists with Dismiss path", async () => {
    rejectTaskMock.mockResolvedValue(approvalResult({ pmNotified: false }));
    const onTaskResolved = vi.fn();
    const { getByText, queryByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    fireEvent.click(getByText("Reject…"));
    fireEvent.click(getByText("Confirm reject"));
    await waitFor(() => {
      expect(rejectTaskMock).toHaveBeenCalled();
    });
    await waitFor(() => {
      expect(queryByText(/Rejected — PM didn't receive/i)).toBeTruthy();
    });
    expect(onTaskResolved).not.toHaveBeenCalled();
    fireEvent.click(getByText("Got it"));
    expect(onTaskResolved).toHaveBeenCalledWith("task-1");
  });

  // -- Step 4 polish (Section B) --------------------------------------------

  it("IllegalTransition error renders friendly message + Refresh, hides raw text", async () => {
    approveTaskMock.mockRejectedValue(
      "[ERR_ILLEGAL_TRANSITION] illegal task transition: ready → ready",
    );
    const onRefresh = vi.fn();
    const onTaskResolved = vi.fn();
    const { container, getByText, queryByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={onTaskResolved}
        onTaskPatched={vi.fn()}
        onRefresh={onRefresh}
        onClose={vi.fn()}
      />,
    );
    fireEvent.click(getByText("Approve"));
    await waitFor(() => {
      expect(queryByText(/resolved elsewhere/i)).toBeTruthy();
    });
    // Friendly text shown; raw "illegal task transition" must NOT leak.
    expect(queryByText(/illegal task transition/i)).toBeNull();
    // Error block carries the data-error-code for e2e assertions.
    const errBlock = container.querySelector(
      `li[data-task-id="task-1"] .team-mode-plan-review-error`,
    );
    expect(errBlock?.getAttribute("data-error-code")).toBe("illegal-transition");
    // Refresh button is wired to the prop.
    fireEvent.click(getByText("Refresh"));
    expect(onRefresh).toHaveBeenCalled();
  });

  it("unknown error string falls back to raw text (no Refresh button)", async () => {
    approveTaskMock.mockRejectedValue("workspace not connected: foo");
    const { container, getByText, queryByText } = render(
      <PlanReviewModal
        workspaceId="ws-1"
        teamId="team-1"
        agents={agents}
        queue={[baseTask]}
        onTaskResolved={vi.fn()}
        onTaskPatched={vi.fn()}
        onRefresh={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    fireEvent.click(getByText("Approve"));
    await waitFor(() => {
      expect(queryByText(/workspace not connected/i)).toBeTruthy();
    });
    // No Refresh affordance for unknown errors — the user has no
    // confidence-of-recovery story to offer here.
    const errBlock = container.querySelector(
      `li[data-task-id="task-1"] .team-mode-plan-review-error`,
    );
    expect(errBlock?.getAttribute("data-error-code")).toBe("unknown");
    expect(queryByText("Refresh")).toBeNull();
  });
});
