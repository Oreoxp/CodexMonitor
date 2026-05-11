import { useCallback, useState } from "react";
import { useTauriEvent } from "@app/hooks/useTauriEvent";
import {
  subscribeLangGraphEvents,
  type LangGraphEventPayload,
} from "@services/events";
import type { PendingPlanApproval } from "../components/types";

export function extractPlanReview(event: LangGraphEventPayload): PendingPlanApproval | null {
  if (event.kind !== "approval_required") {
    return null;
  }
  const approval = event.approval;
  if (
    typeof approval !== "object" ||
    approval === null ||
    (approval as Record<string, unknown>).kind !== "plan_review"
  ) {
    return null;
  }
  const a = approval as Record<string, unknown>;
  const draft = a.draft as Record<string, unknown> | undefined;
  if (
    typeof a.thread_id !== "string" ||
    typeof a.node !== "string" ||
    !draft ||
    typeof draft.summary !== "string" ||
    !Array.isArray(draft.steps) ||
    typeof draft.revision !== "number"
  ) {
    return null;
  }
  return {
    thread_id: a.thread_id,
    node: a.node,
    draft: {
      summary: draft.summary,
      steps: draft.steps.filter((s): s is string => typeof s === "string"),
      revision: draft.revision,
    },
  };
}

type UseLangGraphEventsOptions = {
  onEvent?: (event: LangGraphEventPayload) => void;
};

export function useLangGraphEvents(options: UseLangGraphEventsOptions = {}) {
  const { onEvent } = options;
  const [pendingApprovals, setPendingApprovals] = useState<PendingPlanApproval[]>([]);

  const handleEvent = useCallback((event: LangGraphEventPayload) => {
    onEvent?.(event);

    const planReview = extractPlanReview(event);
    if (planReview) {
      setPendingApprovals((prev) => {
        const without = prev.filter((p) => p.thread_id !== planReview.thread_id);
        return [...without, planReview];
      });
    }

    if (event.kind === "done") {
      const threadId = event.thread_id;
      setPendingApprovals((prev) => prev.filter((p) => p.thread_id !== threadId));
    }
  }, [onEvent]);

  useTauriEvent(subscribeLangGraphEvents, handleEvent);

  const dismissApproval = useCallback((threadId: string) => {
    setPendingApprovals((prev) => prev.filter((p) => p.thread_id !== threadId));
  }, []);

  return { pendingApprovals, dismissApproval };
}
