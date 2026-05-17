// Phase 3 Step 4 — plan-review modal.
//
// FORKED from the normal-mode modal pattern (`ModalShell`) — not shared.
// Cross-phase shared-extraction (CLAUDE.md §5.1) collapses duplicates
// later; do not pre-share with TeamCreateModal.
//
// Responsibilities:
//   - Render the in-memory approval queue (one card per task).
//   - Per card: inline edits for title (input) / body (textarea) /
//     assignee (select). Dirty state tracked per task.
//   - Approve: if dirty, call `update_task(patch)` first; then
//     `approve_task`. Pass `pm_notified` warning chip through.
//   - Reject: expand a feedback textarea; submit → `reject_task(feedback)`.
//   - On any successful command, remove the task from the queue.
//   - Auto-close when the queue drains (driven by the hook).
//
// What this modal does NOT do (out of scope per Step 4 spec):
//   - bulk approve / reject (Phase 4 polish)
//   - dependency editing (Phase 4+)
//   - dragging / multi-modal instances (Phase 4 polish)
//   - ready → running dispatch (Step 5)

import { useCallback, useEffect, useMemo, useState } from "react";

import { ModalShell } from "../../design-system/components/modal/ModalShell";
import {
  approveTask,
  rejectTask,
  updateTask,
} from "@services/tauri";
import type { ApprovalResult, AssigneeUpdate, Task } from "../types/tasks";
import type { AgentConfig } from "../types";

type PlanReviewModalProps = {
  workspaceId: string;
  teamId: string;
  agents: AgentConfig[];
  queue: Task[];
  /** Remove a task from the queue. Called either immediately on a
   *  pmNotified=true success, or later on user-driven Dismiss for a
   *  pmNotified=false "resolved-with-warning" card. */
  onTaskResolved: (taskId: string) => void;
  /** Hook-level row mirror after `update_task` succeeds. */
  onTaskPatched: (task: Task) => void;
  /** Called by the per-card "Refresh" affordance that shows up when the
   *  task was resolved elsewhere ([ERR_ILLEGAL_TRANSITION]). Hook wires
   *  this to `approval.refetch()`. Optional so the modal stays usable
   *  without an explicit refresh path. */
  onRefresh?: () => void;
  onClose: () => void;
};

// Step 4 polish — stable error-code prefixes the Rust `TaskError::Display`
// emits. The friendly-rendering layer pattern-matches on these tags; raw
// strings only leak through for unknown variants.
const ERR_ILLEGAL_TRANSITION = "[ERR_ILLEGAL_TRANSITION]";
const ERR_NOT_FOUND = "[ERR_NOT_FOUND]";

type FriendlyError =
  | {
      code: "illegal-transition";
      raw: string;
      message: string;
      offerRefresh: true;
    }
  | {
      code: "not-found";
      raw: string;
      message: string;
      offerRefresh: true;
    }
  | { code: "unknown"; raw: string; message: string; offerRefresh: false };

function classifyError(raw: string): FriendlyError {
  if (raw.startsWith(ERR_ILLEGAL_TRANSITION)) {
    return {
      code: "illegal-transition",
      raw,
      message:
        "This task was resolved elsewhere (maybe in another window, or by a manual transition). Refresh to see the latest state.",
      offerRefresh: true,
    };
  }
  if (raw.startsWith(ERR_NOT_FOUND)) {
    return {
      code: "not-found",
      raw,
      message:
        "This task no longer exists. Refresh to update the queue.",
      offerRefresh: true,
    };
  }
  return { code: "unknown", raw, message: raw, offerRefresh: false };
}

// Per-card editing state. Kept out of `queue` so refetches from the hook
// don't clobber the user's in-progress draft.
type DraftMap = Record<string, Draft | undefined>;
type Draft = {
  title: string;
  body: string;
  // `null` means "Unassigned" (write NULL). `undefined` means "leave alone"
  // (don't include in patch).
  assignee: string | null | undefined;
  // Whether the user has opened the reject feedback area. Kept here so
  // each card's reject affordance stays independent.
  rejectOpen: boolean;
  feedback: string;
};

// Step 4 polish — resolved-with-warning state. After a successful
// approve/reject whose system-reply dispatch failed (pmNotified=false),
// the card stays mounted in this state until the user clicks Dismiss.
// `pmNotified=true` cards skip this entirely and call `onTaskResolved`
// immediately.
type ResolvedState = {
  kind: "approve" | "reject";
  pmNotified: boolean; // always false in practice — true cards auto-resolve
};
type ResolvedMap = Record<string, ResolvedState | undefined>;

const UNASSIGNED_SENTINEL = "__unassigned__";

function makeDraft(task: Task): Draft {
  return {
    title: task.title,
    body: task.body,
    assignee: undefined,
    rejectOpen: false,
    feedback: "",
  };
}

function buildPatch(task: Task, draft: Draft): {
  title?: string;
  body?: string;
  assignee?: AssigneeUpdate;
} | null {
  const patch: {
    title?: string;
    body?: string;
    assignee?: AssigneeUpdate;
  } = {};
  if (draft.title !== task.title) patch.title = draft.title;
  if (draft.body !== task.body) patch.body = draft.body;
  if (draft.assignee !== undefined) {
    if (draft.assignee === null) {
      if (task.assigneeAgentId !== null) patch.assignee = { op: "clear" };
    } else if (draft.assignee !== task.assigneeAgentId) {
      patch.assignee = { op: "set", value: draft.assignee };
    }
  }
  if (
    patch.title === undefined &&
    patch.body === undefined &&
    patch.assignee === undefined
  ) {
    return null;
  }
  return patch;
}

export function PlanReviewModal({
  workspaceId,
  teamId,
  agents,
  queue,
  onTaskResolved,
  onTaskPatched,
  onRefresh,
  onClose,
}: PlanReviewModalProps) {
  const [drafts, setDrafts] = useState<DraftMap>({});
  const [busy, setBusy] = useState<Record<string, "approve" | "reject" | undefined>>({});
  const [errors, setErrors] = useState<Record<string, string | undefined>>({});
  const [resolved, setResolved] = useState<ResolvedMap>({});

  // Seed drafts for newly-arrived queue entries; preserve in-progress
  // drafts for tasks still in the queue.
  useEffect(() => {
    setDrafts((prev) => {
      const next: DraftMap = {};
      for (const task of queue) {
        next[task.id] = prev[task.id] ?? makeDraft(task);
      }
      return next;
    });
  }, [queue]);

  const agentOptions = useMemo(
    () =>
      agents.map((a) => ({
        id: a.id,
        label: a.name || a.id,
      })),
    [agents],
  );

  const setDraft = useCallback((taskId: string, updater: (d: Draft) => Draft) => {
    setDrafts((prev) => {
      const current = prev[taskId];
      if (!current) return prev;
      return { ...prev, [taskId]: updater(current) };
    });
  }, []);

  const handleApprove = useCallback(
    async (task: Task) => {
      const draft = drafts[task.id];
      if (!draft) return;
      setBusy((b) => ({ ...b, [task.id]: "approve" }));
      setErrors((e) => ({ ...e, [task.id]: undefined }));
      try {
        const patch = buildPatch(task, draft);
        if (patch) {
          // If the user edited the row, save first. The Rust side's
          // update_task refuses non-proposed rows (state-machine guard);
          // approve_task that follows enforces the same gate at the
          // status transition — so a concurrent approve-elsewhere race
          // surfaces as `[ERR_ILLEGAL_TRANSITION]` on either call. The
          // error path is handled below.
          const patched = await updateTask(workspaceId, task.id, patch, "user");
          onTaskPatched(patched);
        }
        const result: ApprovalResult = await approveTask(workspaceId, task.id, "user");
        if (result.pmNotified) {
          // Happy path: PM has the system note; the card has no reason
          // to stay on screen. Remove immediately.
          onTaskResolved(task.id);
        } else {
          // Resolved-with-warning. Keep the card mounted in a read-only
          // state until the user clicks Dismiss — spec change from the
          // Step 4 ship: the user must be able to *see* the warning
          // long enough to act on it.
          setResolved((r) => ({
            ...r,
            [task.id]: { kind: "approve", pmNotified: false },
          }));
        }
      } catch (err) {
        setErrors((e) => ({ ...e, [task.id]: String(err) }));
      } finally {
        setBusy((b) => ({ ...b, [task.id]: undefined }));
      }
    },
    [drafts, workspaceId, onTaskPatched, onTaskResolved],
  );

  const handleReject = useCallback(
    async (task: Task) => {
      const draft = drafts[task.id];
      if (!draft) return;
      setBusy((b) => ({ ...b, [task.id]: "reject" }));
      setErrors((e) => ({ ...e, [task.id]: undefined }));
      try {
        const feedback = draft.feedback.trim() === "" ? null : draft.feedback.trim();
        const result: ApprovalResult = await rejectTask(
          workspaceId,
          task.id,
          feedback,
          "user",
        );
        if (result.pmNotified) {
          onTaskResolved(task.id);
        } else {
          setResolved((r) => ({
            ...r,
            [task.id]: { kind: "reject", pmNotified: false },
          }));
        }
      } catch (err) {
        setErrors((e) => ({ ...e, [task.id]: String(err) }));
      } finally {
        setBusy((b) => ({ ...b, [task.id]: undefined }));
      }
    },
    [drafts, workspaceId, onTaskResolved],
  );

  const handleDismiss = useCallback(
    (taskId: string) => {
      // Pull both resolved and error state down before unmounting the
      // card; the queue removal triggers the card's React unmount via
      // the parent's setQueue.
      setResolved((r) => {
        const next = { ...r };
        delete next[taskId];
        return next;
      });
      onTaskResolved(taskId);
    },
    [onTaskResolved],
  );

  const handleRefresh = useCallback(
    (taskId: string) => {
      // Clear the per-card error before refresh — the user explicitly
      // asked for a fresh view, no point keeping the stale message.
      setErrors((e) => {
        const next = { ...e };
        delete next[taskId];
        return next;
      });
      onRefresh?.();
    },
    [onRefresh],
  );

  if (queue.length === 0) {
    // The hook auto-closes on drain; this is a defensive empty-state for
    // the case where the modal is briefly mounted with an empty queue
    // (e.g. user opened the badge with no pending rows).
    return (
      <ModalShell
        ariaLabel="Plan review"
        onBackdropClick={onClose}
        cardClassName="team-mode-plan-review-card"
      >
        <header className="team-mode-plan-review-header">
          <h2>No plans waiting</h2>
        </header>
        <footer className="team-mode-plan-review-footer">
          <button type="button" className="team-mode-button" onClick={onClose}>
            Close
          </button>
        </footer>
      </ModalShell>
    );
  }

  return (
    <ModalShell
      ariaLabel="Plan review"
      onBackdropClick={onClose}
      cardClassName="team-mode-plan-review-card"
    >
      <header className="team-mode-plan-review-header">
        <h2>Plan review</h2>
        <p className="team-mode-plan-review-subtitle">
          {queue.length} task{queue.length === 1 ? "" : "s"} awaiting your decision.
        </p>
      </header>

      <ul
        className="team-mode-plan-review-list"
        // teamId is not visible per-card, but we pin it here in the data
        // attribute so the Phase-4 e2e smoke test can scope its assertions
        // to the right team without scraping props.
        data-team-id={teamId}
      >
        {queue.map((task) => {
          const draft = drafts[task.id];
          if (!draft) return null;
          const taskBusy = busy[task.id];
          const rawError = errors[task.id];
          const friendlyError = rawError ? classifyError(rawError) : null;
          const resolvedState = resolved[task.id];
          // Read-only mode: either we're mid-RPC (busy) or the row is
          // in resolved-with-warning state pending the user's Dismiss.
          const fieldsLocked = taskBusy !== undefined || resolvedState !== undefined;
          const assigneeValue =
            draft.assignee === undefined
              ? task.assigneeAgentId ?? UNASSIGNED_SENTINEL
              : draft.assignee ?? UNASSIGNED_SENTINEL;
          const cardClassName =
            "team-mode-plan-review-card" +
            (resolvedState ? " team-mode-plan-review-card-resolved-warning" : "");
          return (
            <li
              key={task.id}
              className={cardClassName}
              data-task-id={task.id}
              data-state={resolvedState ? "resolved-warning" : "pending"}
            >
              <div className="team-mode-plan-review-card-row">
                <label className="team-mode-plan-review-label">Title</label>
                <input
                  type="text"
                  className="team-mode-plan-review-input"
                  value={draft.title}
                  onChange={(e) =>
                    setDraft(task.id, (d) => ({ ...d, title: e.target.value }))
                  }
                  disabled={fieldsLocked}
                />
              </div>
              <div className="team-mode-plan-review-card-row">
                <label className="team-mode-plan-review-label">Body</label>
                <textarea
                  className="team-mode-plan-review-textarea"
                  value={draft.body}
                  rows={3}
                  onChange={(e) =>
                    setDraft(task.id, (d) => ({ ...d, body: e.target.value }))
                  }
                  disabled={fieldsLocked}
                />
              </div>
              <div className="team-mode-plan-review-card-row">
                <label className="team-mode-plan-review-label">Assignee</label>
                <select
                  className="team-mode-plan-review-select"
                  value={assigneeValue}
                  onChange={(e) => {
                    const value = e.target.value;
                    setDraft(task.id, (d) => ({
                      ...d,
                      assignee: value === UNASSIGNED_SENTINEL ? null : value,
                    }));
                  }}
                  disabled={fieldsLocked}
                >
                  <option value={UNASSIGNED_SENTINEL}>Unassigned</option>
                  {agentOptions.map((opt) => (
                    <option key={opt.id} value={opt.id}>
                      {opt.label}
                    </option>
                  ))}
                </select>
              </div>

              {draft.rejectOpen && !resolvedState ? (
                <div className="team-mode-plan-review-card-row">
                  <label className="team-mode-plan-review-label">
                    Reject feedback (optional)
                  </label>
                  <textarea
                    className="team-mode-plan-review-textarea"
                    value={draft.feedback}
                    rows={2}
                    placeholder="Tell PM why this isn't going forward…"
                    onChange={(e) =>
                      setDraft(task.id, (d) => ({ ...d, feedback: e.target.value }))
                    }
                    disabled={fieldsLocked}
                  />
                </div>
              ) : null}

              {resolvedState ? (
                // Resolved-with-warning footer: warning chip + Dismiss.
                // Approve / Reject affordances are hidden so the user
                // can't accidentally re-trigger the RPC on a resolved row.
                <div
                  className="team-mode-plan-review-card-resolved-footer"
                  role="group"
                  aria-label={
                    resolvedState.kind === "approve"
                      ? "Approval result"
                      : "Rejection result"
                  }
                >
                  <div
                    className="team-mode-plan-review-warning-chip"
                    role="status"
                    aria-live="polite"
                  >
                    {resolvedState.kind === "approve"
                      ? "✓ Approved — PM didn't receive the system note. Consider @-mentioning PM manually."
                      : "✓ Rejected — PM didn't receive the system note. Consider @-mentioning PM manually."}
                  </div>
                  <button
                    type="button"
                    className="team-mode-button"
                    onClick={() => handleDismiss(task.id)}
                  >
                    Got it
                  </button>
                </div>
              ) : (
                <div className="team-mode-plan-review-card-actions">
                  <button
                    type="button"
                    className="team-mode-button team-mode-button-primary"
                    onClick={() => handleApprove(task)}
                    disabled={taskBusy !== undefined}
                  >
                    {taskBusy === "approve" ? "Approving…" : "Approve"}
                  </button>
                  {draft.rejectOpen ? (
                    <>
                      <button
                        type="button"
                        className="team-mode-button team-mode-button-danger"
                        onClick={() => handleReject(task)}
                        disabled={taskBusy !== undefined}
                      >
                        {taskBusy === "reject" ? "Rejecting…" : "Confirm reject"}
                      </button>
                      <button
                        type="button"
                        className="team-mode-button"
                        onClick={() =>
                          setDraft(task.id, (d) => ({
                            ...d,
                            rejectOpen: false,
                            feedback: "",
                          }))
                        }
                        disabled={taskBusy !== undefined}
                      >
                        Cancel
                      </button>
                    </>
                  ) : (
                    <button
                      type="button"
                      className="team-mode-button"
                      onClick={() =>
                        setDraft(task.id, (d) => ({ ...d, rejectOpen: true }))
                      }
                      disabled={taskBusy !== undefined}
                    >
                      Reject…
                    </button>
                  )}
                </div>
              )}

              {friendlyError && !resolvedState ? (
                <div
                  className="team-mode-plan-review-error"
                  data-error-code={friendlyError.code}
                >
                  <div className="team-mode-plan-review-error-message">
                    {friendlyError.message}
                  </div>
                  {friendlyError.offerRefresh && onRefresh ? (
                    <div className="team-mode-plan-review-error-actions">
                      <button
                        type="button"
                        className="team-mode-button"
                        onClick={() => handleRefresh(task.id)}
                      >
                        Refresh
                      </button>
                    </div>
                  ) : null}
                </div>
              ) : null}
            </li>
          );
        })}
      </ul>

      <footer className="team-mode-plan-review-footer">
        <button type="button" className="team-mode-button" onClick={onClose}>
          Close
        </button>
      </footer>
    </ModalShell>
  );
}
