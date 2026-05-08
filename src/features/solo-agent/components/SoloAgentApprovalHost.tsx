import { useCallback, useRef, useState } from "react";
import { useLangGraphEvents } from "../hooks/useLangGraphEvents";
import { SoloPlanReviewCard } from "./SoloPlanReviewCard";
import { langGraphSidecarInvoke, langGraphSidecarResume } from "@services/tauri";

type Props = {
  workspaceCwd?: string | null;
};

let testCounter = 0;

export function SoloAgentApprovalHost({ workspaceCwd }: Props) {
  const { pendingApprovals, dismissApproval } = useLangGraphEvents();
  const [testBusy, setTestBusy] = useState(false);
  const [testError, setTestError] = useState<string | null>(null);
  const threadIdRef = useRef<string | null>(null);

  const handleTest = useCallback(async () => {
    setTestBusy(true);
    setTestError(null);
    testCounter += 1;
    const threadId = `ui-test-${testCounter}`;
    threadIdRef.current = threadId;
    try {
      await langGraphSidecarInvoke({
        goal: "verify tauri langgraph approval flow",
        threadId,
        workspaceCwd: workspaceCwd ?? null,
      });
    } catch (err) {
      setTestError(err instanceof Error ? err.message : String(err));
    } finally {
      setTestBusy(false);
    }
  }, [workspaceCwd]);

  const handleApprove = useCallback(
    async (threadId: string) => {
      await langGraphSidecarResume({
        threadId,
        action: "approve",
        workspaceCwd: workspaceCwd ?? null,
      });
      dismissApproval(threadId);
    },
    [dismissApproval, workspaceCwd],
  );

  const handleReject = useCallback(
    async (threadId: string, feedback: string) => {
      await langGraphSidecarResume({
        threadId,
        action: "reject",
        feedback: feedback.trim() || undefined,
        workspaceCwd: workspaceCwd ?? null,
      });
    },
    [workspaceCwd],
  );

  return (
    <>
      <div className="solo-agent-test-trigger">
        <button
          type="button"
          className="ghost solo-agent-test-button"
          onClick={handleTest}
          disabled={testBusy}
          title="Trigger a mock Solo Agent run to test the plan approval UI"
        >
          {testBusy ? "Running…" : "Solo Agent Test"}
        </button>
        {testError ? (
          <div className="solo-agent-test-error">{testError}</div>
        ) : null}
      </div>
      <SoloPlanReviewCard
        approvals={pendingApprovals}
        workspaceCwd={workspaceCwd}
        onApprove={handleApprove}
        onReject={handleReject}
      />
    </>
  );
}
