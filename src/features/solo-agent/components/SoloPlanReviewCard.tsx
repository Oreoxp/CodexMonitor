import { useState } from "react";
import type { PendingPlanApproval } from "../hooks/useLangGraphEvents";
import {
  ToastActions,
  ToastCard,
  ToastHeader,
  ToastTitle,
  ToastViewport,
} from "@/features/design-system/components/toast/ToastPrimitives";

type Props = {
  approvals: PendingPlanApproval[];
  workspaceCwd?: string | null;
  onApprove: (threadId: string) => Promise<void>;
  onReject: (threadId: string, feedback: string) => Promise<void>;
};

type CardState = {
  rejecting: boolean;
  feedback: string;
  busy: boolean;
  error: string | null;
};

function useCardState() {
  const [state, setState] = useState<CardState>({
    rejecting: false,
    feedback: "",
    busy: false,
    error: null,
  });
  return { state, setState };
}

type SingleCardProps = {
  approval: PendingPlanApproval;
  onApprove: (threadId: string) => Promise<void>;
  onReject: (threadId: string, feedback: string) => Promise<void>;
};

function PlanReviewCardItem({ approval, onApprove, onReject }: SingleCardProps) {
  const { state, setState } = useCardState();

  const handleApprove = async () => {
    setState((s) => ({ ...s, busy: true, error: null }));
    try {
      await onApprove(approval.thread_id);
    } catch (err) {
      setState((s) => ({
        ...s,
        busy: false,
        error: err instanceof Error ? err.message : String(err),
      }));
    }
  };

  const handleRejectClick = () => {
    setState((s) => ({ ...s, rejecting: true, error: null }));
  };

  const handleRejectCancel = () => {
    setState((s) => ({ ...s, rejecting: false, feedback: "", error: null }));
  };

  const handleRejectSubmit = async () => {
    setState((s) => ({ ...s, busy: true, error: null }));
    try {
      await onReject(approval.thread_id, state.feedback);
    } catch (err) {
      setState((s) => ({
        ...s,
        busy: false,
        error: err instanceof Error ? err.message : String(err),
      }));
    }
  };

  const { draft } = approval;

  return (
    <ToastCard className="solo-plan-review-toast" role="alert">
      <ToastHeader className="approval-toast-header">
        <ToastTitle className="approval-toast-title">Plan Review</ToastTitle>
        <div className="approval-toast-workspace">
          {approval.node} · revision {draft.revision + 1}
        </div>
      </ToastHeader>

      <div className="approval-toast-method">{draft.summary}</div>

      <div className="approval-toast-details">
        <ol className="solo-plan-review-steps">
          {draft.steps.map((step, i) => (
            <li key={i} className="solo-plan-review-step">
              {step}
            </li>
          ))}
        </ol>
      </div>

      {state.rejecting ? (
        <div className="solo-plan-review-reject-form">
          <textarea
            className="solo-plan-review-feedback"
            placeholder="Feedback for the agent (optional)"
            value={state.feedback}
            onChange={(e) => setState((s) => ({ ...s, feedback: e.target.value }))}
            rows={3}
            disabled={state.busy}
            autoFocus
          />
          <ToastActions className="approval-toast-actions">
            <button
              className="secondary"
              onClick={handleRejectCancel}
              disabled={state.busy}
            >
              Cancel
            </button>
            <button
              className="primary"
              onClick={handleRejectSubmit}
              disabled={state.busy}
            >
              {state.busy ? "Sending…" : "Send feedback"}
            </button>
          </ToastActions>
        </div>
      ) : (
        <ToastActions className="approval-toast-actions">
          <button
            className="secondary"
            onClick={handleRejectClick}
            disabled={state.busy}
          >
            Reject
          </button>
          <button
            className="primary"
            onClick={handleApprove}
            disabled={state.busy}
          >
            {state.busy ? "Approving…" : "Approve"}
          </button>
        </ToastActions>
      )}

      {state.error ? (
        <div className="solo-plan-review-error">{state.error}</div>
      ) : null}
    </ToastCard>
  );
}

export function SoloPlanReviewCard({ approvals, onApprove, onReject }: Props) {
  if (!approvals.length) {
    return null;
  }

  return (
    <ToastViewport className="approval-toasts solo-plan-review-viewport" role="region" ariaLive="assertive">
      {approvals.map((approval) => (
        <PlanReviewCardItem
          key={`${approval.thread_id}-${approval.draft.revision}`}
          approval={approval}
          onApprove={onApprove}
          onReject={onReject}
        />
      ))}
    </ToastViewport>
  );
}
