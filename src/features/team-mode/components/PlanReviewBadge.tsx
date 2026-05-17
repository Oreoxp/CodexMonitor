// Phase 3 Step 4 — manual re-open entry for the plan-review modal.
//
// Renders a small "待审批 (N)" badge button. The count comes from the
// approval queue's current length; the click handler re-opens the modal
// (and refetches the proposed-row list as a belt-and-suspenders sync so
// the count survives a closed-and-reopened app session).

import type { Task } from "../types/tasks";

type PlanReviewBadgeProps = {
  queue: Task[];
  onOpen: () => void;
};

export function PlanReviewBadge({ queue, onOpen }: PlanReviewBadgeProps) {
  const count = queue.length;
  if (count === 0) return null;
  return (
    <button
      type="button"
      className="team-mode-approval-badge"
      onClick={onOpen}
      aria-label={`Open plan review (${count} pending)`}
    >
      <span>Plan review</span>
      <span className="team-mode-approval-badge-count">{count}</span>
    </button>
  );
}
