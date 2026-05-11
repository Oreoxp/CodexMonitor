import type { WorkspaceInfo } from "@/types";
import { SoloEmptyState } from "./SoloEmptyState";
import { SoloNewRunForm } from "./SoloNewRunForm";
import { SoloRunDetail } from "./SoloRunDetail";
import type { PendingPlanApproval, SoloRun, SoloStepDetail, SoloTimelineStageId } from "./types";

type SoloMainPanelProps = {
  activeWorkspace: WorkspaceInfo | null;
  mode: "empty" | "new" | "detail";
  activeRun: SoloRun | null;
  activeApproval: PendingPlanApproval | null;
  goal: string;
  isSubmitting: boolean;
  error: string | null;
  onGoalChange: (goal: string) => void;
  onStart: () => void;
  onCancel: () => void;
  onApprovePlan: (threadId: string) => Promise<void>;
  onRejectPlan: (threadId: string, feedback: string) => Promise<void>;
  onContinueRun: (threadId: string) => Promise<void>;
  onRefreshState: (threadId: string) => Promise<void>;
  onRetryStep: (threadId: string, node: string) => Promise<void>;
  onViewLogs: (threadId: string) => void;
  onViewReport: (threadId: string) => void;
  onNewSolo: () => void;
  selectedStepId: SoloTimelineStageId | null;
  stepDetail: SoloStepDetail | null;
  stepDetailLoading: boolean;
  stepDetailError: string | null;
  onOpenStep: (stageId: SoloTimelineStageId) => void;
  onCloseStep: () => void;
};

export function SoloMainPanel({
  activeWorkspace,
  mode,
  activeRun,
  activeApproval,
  goal,
  isSubmitting,
  error,
  onGoalChange,
  onStart,
  onCancel,
  onApprovePlan,
  onRejectPlan,
  onContinueRun,
  onRefreshState,
  onRetryStep,
  onViewLogs,
  onViewReport,
  onNewSolo,
  selectedStepId,
  stepDetail,
  stepDetailLoading,
  stepDetailError,
  onOpenStep,
  onCloseStep,
}: SoloMainPanelProps) {
  if (mode === "new" && activeWorkspace) {
    return (
      <main className="solo-main-panel">
        <SoloNewRunForm
          workspace={activeWorkspace}
          goal={goal}
          isSubmitting={isSubmitting}
          error={error}
          onGoalChange={onGoalChange}
          onStart={onStart}
          onCancel={onCancel}
        />
      </main>
    );
  }

  if (mode === "detail" && activeRun) {
    return (
      <SoloRunDetail
        run={activeRun}
        approval={activeApproval}
        onApprovePlan={onApprovePlan}
        onRejectPlan={onRejectPlan}
        onContinueRun={onContinueRun}
        onRetryStep={onRetryStep}
        onRefreshState={onRefreshState}
        onViewLogs={onViewLogs}
        onViewReport={onViewReport}
        onNewSolo={onNewSolo}
        selectedStepId={selectedStepId}
        stepDetail={stepDetail}
        stepDetailLoading={stepDetailLoading}
        stepDetailError={stepDetailError}
        onOpenStep={onOpenStep}
        onCloseStep={onCloseStep}
      />
    );
  }

  return (
    <main className="solo-main-panel">
      <SoloEmptyState hasWorkspace={Boolean(activeWorkspace)} />
    </main>
  );
}
