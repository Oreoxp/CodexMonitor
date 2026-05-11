import { useCallback } from "react";
import { useLangGraphEvents } from "../hooks/useLangGraphEvents";
import { SoloPlanReviewCard } from "./SoloPlanReviewCard";
import { langGraphSidecarResume } from "@services/tauri";

type Props = {
  workspaceId?: string | null;
  workspaceCwd?: string | null;
};

export function SoloAgentApprovalHost({ workspaceId, workspaceCwd }: Props) {
  const { pendingApprovals, dismissApproval } = useLangGraphEvents();

  const handleApprove = useCallback(
    async (threadId: string) => {
      await langGraphSidecarResume({
        threadId,
        action: "approve",
        workspaceId: workspaceId ?? null,
        workspaceCwd: workspaceCwd ?? null,
      });
      dismissApproval(threadId);
    },
    [dismissApproval, workspaceId, workspaceCwd],
  );

  const handleReject = useCallback(
    async (threadId: string, feedback: string) => {
      await langGraphSidecarResume({
        threadId,
        action: "reject",
        feedback: feedback.trim() || undefined,
        workspaceId: workspaceId ?? null,
        workspaceCwd: workspaceCwd ?? null,
      });
    },
    [workspaceId, workspaceCwd],
  );

  return (
    <SoloPlanReviewCard
      approvals={pendingApprovals}
      workspaceCwd={workspaceCwd}
      onApprove={handleApprove}
      onReject={handleReject}
    />
  );
}
