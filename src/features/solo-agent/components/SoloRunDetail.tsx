import type { SoloRun, SoloStepDetail, SoloTimelineStageId } from "./types";
import type { PendingPlanApproval } from "./types";
import { SoloPlanReviewCard } from "./SoloPlanReviewCard";
import { SoloStepDrawer } from "./SoloStepDrawer";
import { SoloTimeline } from "./SoloTimeline";

type SoloRunDetailProps = {
  run: SoloRun;
  approval: PendingPlanApproval | null;
  onApprovePlan: (threadId: string) => Promise<void>;
  onRejectPlan: (threadId: string, feedback: string) => Promise<void>;
  onContinueRun: (threadId: string) => Promise<void>;
  onRetryStep: (threadId: string, node: string) => Promise<void>;
  onRefreshState: (threadId: string) => Promise<void>;
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

function readableStatus(run: SoloRun) {
  if (run.status === "failed" || run.status === "paused_on_error") {
    return "Solo 遇到问题，需要检查错误信息";
  }
  if (
    run.status === "done" ||
    run.status === "delivered" ||
    run.status === "delivered_with_warnings"
  ) {
    return "已完成";
  }
  if (run.status === "waiting_approval") {
    return "等待你审批计划";
  }
  if (run.status === "stale_running" || run.status === "interrupted") {
    return "Solo 可以从上次进度继续";
  }
  if (run.status === "unknown") {
    return "需要刷新状态";
  }

  const runningStage = run.timeline.find((stage) => stage.status === "running");
  const waitingStage = run.timeline.find((stage) => stage.status === "waiting");
  const stageId = waitingStage?.id ?? runningStage?.id;

  switch (stageId) {
    case "intake":
      return "Solo 正在准备任务";
    case "understand":
      return "Solo 正在理解需求";
    case "context_scan":
      return "Solo 正在扫描项目上下文";
    case "plan":
      return "Solo 正在生成计划";
    case "approval":
      return "等待你审批计划";
    case "execute":
      return "Solo 正在执行";
    case "inspect":
      return "Solo 正在检查结果";
    case "diagnose":
      return "Solo 正在诊断问题";
    case "repair":
      return "Solo 正在修复问题";
    case "recheck":
      return "Solo 正在复查";
    case "deliver":
      return "Solo 正在生成报告";
    default:
      return run.status === "planning" ? "Solo 正在生成计划" : "Solo 正在准备下一步";
  }
}

function SoloRunActionBar({
  run,
  approval,
  onApprovePlan,
  onRejectPlan,
  onContinueRun,
  onRetryStep,
  onRefreshState,
  onViewLogs,
  onViewReport,
  onNewSolo,
  onOpenStep,
}: SoloRunDetailProps) {
  const canApprove = Boolean(approval);

  if (run.status === "waiting_approval") {
    return (
      <section className="solo-action-bar" aria-label="Solo actions">
        <button
          className="oc-button oc-button-primary"
          disabled={!canApprove}
          onClick={() => void onApprovePlan(run.threadId)}
        >
          Approve
        </button>
        <button
          className="oc-button"
          disabled={!canApprove}
          onClick={() => void onRejectPlan(run.threadId, "")}
        >
          Reject
        </button>
        <button className="oc-button" onClick={() => void onRefreshState(run.threadId)}>
          Refresh State
        </button>
      </section>
    );
  }

  if (
    run.status === "running" ||
    run.status === "stale_running" ||
    run.status === "interrupted"
  ) {
    return (
      <section className="solo-action-bar" aria-label="Solo actions">
        <button
          className="oc-button oc-button-primary"
          onClick={() => void onContinueRun(run.threadId)}
        >
          Continue
        </button>
        <button className="oc-button" onClick={() => void onRefreshState(run.threadId)}>
          Refresh State
        </button>
      </section>
    );
  }

  if (run.status === "paused_on_error") {
    const node = run.failedNode ?? run.currentNode ?? run.lastNode ?? "execute";
    const failedStageId = run.timeline.find((stage) => stage.status === "failed")?.id ?? "execute";
    return (
      <section className="solo-action-bar" aria-label="Solo actions">
        <button
          className="oc-button oc-button-primary"
          onClick={() => void onRetryStep(run.threadId, node)}
        >
          Retry Current Step
        </button>
        <button className="oc-button" onClick={() => void onContinueRun(run.threadId)}>
          Continue Anyway
        </button>
        <button className="oc-button" onClick={() => onOpenStep(failedStageId)}>
          View Details
        </button>
      </section>
    );
  }

  if (run.status === "failed") {
    return (
      <section className="solo-action-bar" aria-label="Solo actions">
        <button
          className="oc-button oc-button-primary"
          onClick={() => void onContinueRun(run.threadId)}
        >
          Retry from checkpoint
        </button>
        <button className="oc-button" onClick={() => onViewLogs(run.threadId)}>
          View Logs
        </button>
      </section>
    );
  }

  if (
    run.status === "delivered" ||
    run.status === "delivered_with_warnings" ||
    run.status === "done"
  ) {
    return (
      <section className="solo-action-bar" aria-label="Solo actions">
        <button
          className="oc-button oc-button-primary"
          onClick={() => onViewReport(run.threadId)}
        >
          View Report
        </button>
        <button className="oc-button" onClick={onNewSolo}>
          New Solo
        </button>
      </section>
    );
  }

  return (
    <section className="solo-action-bar" aria-label="Solo actions">
      <button className="oc-button" onClick={() => void onRefreshState(run.threadId)}>
        Refresh State
      </button>
    </section>
  );
}

export function SoloRunDetail({
  run,
  approval,
  onApprovePlan,
  onRejectPlan,
  onContinueRun,
  onRetryStep,
  onRefreshState,
  onViewLogs,
  onViewReport,
  onNewSolo,
  selectedStepId,
  stepDetail,
  stepDetailLoading,
  stepDetailError,
  onOpenStep,
  onCloseStep,
}: SoloRunDetailProps) {
  const selectedStage =
    run.timeline.find((stage) => stage.id === selectedStepId) ?? null;

  return (
    <main className="solo-main-panel">
      <section className="solo-run-detail">
        <div className="solo-empty-state-kicker">Solo 任务</div>
        <h1>{run.goal || run.title}</h1>
        <div className={`solo-run-status-pill is-${run.status}`}>
          {readableStatus(run)}
        </div>
        <dl className="solo-run-meta">
          <div>
            <dt>Workspace</dt>
            <dd>{run.workspaceName}</dd>
          </div>
        </dl>
        <SoloRunActionBar
          run={run}
          approval={approval}
          onApprovePlan={onApprovePlan}
          onRejectPlan={onRejectPlan}
          onContinueRun={onContinueRun}
          onRetryStep={onRetryStep}
          onRefreshState={onRefreshState}
          onViewLogs={onViewLogs}
          onViewReport={onViewReport}
          onNewSolo={onNewSolo}
          onOpenStep={onOpenStep}
        />
        <section className="solo-current-activity">
          <div className="solo-detail-section-header">当前动态</div>
          {run.recentActivities.length > 0 ? (
            <ul className="solo-activity-list">
              {run.recentActivities.map((activity, index) => (
                <li key={`${activity}-${index}`}>{activity}</li>
              ))}
            </ul>
          ) : (
            <div className="solo-activity-empty">正在等待下一步事件……</div>
          )}
        </section>
        <details className="solo-debug-info">
          <summary>Debug Info</summary>
          <dl className="solo-run-meta solo-run-meta-debug">
            <div>
              <dt>thread_id</dt>
              <dd>{run.threadId}</dd>
            </div>
            <div>
              <dt>workspace_id</dt>
              <dd>{run.workspaceId}</dd>
            </div>
            <div>
              <dt>codex_thread_id</dt>
              <dd>{run.codexThreadId ?? "unknown"}</dd>
            </div>
            <div>
              <dt>current_node</dt>
              <dd>{run.currentNode ?? run.lastNode ?? "unknown"}</dd>
            </div>
            <div>
              <dt>current_phase</dt>
              <dd>{run.currentPhase ?? "unknown"}</dd>
            </div>
          </dl>
        </details>
        <SoloTimeline stages={run.timeline} onSelectStage={onOpenStep} />
        {run.finalReportAvailable || run.artifacts.length > 0 ? (
          <section className="solo-run-recovery-summary">
            <div className="solo-detail-section-header">已恢复内容</div>
            <div className="solo-run-recovery-row">
              报告：{run.finalReportAvailable ? "已生成" : "未找到"}
            </div>
            {run.artifacts.length > 0 ? (
              <div className="solo-run-recovery-row">
                产物：{run.artifacts.join(", ")}
              </div>
            ) : null}
          </section>
        ) : null}
        {approval ? (
          <section className="solo-plan-review-section">
            <div className="solo-detail-section-header">计划审批</div>
            <SoloPlanReviewCard
              approvals={[approval]}
              embedded
              onApprove={onApprovePlan}
              onReject={onRejectPlan}
            />
          </section>
        ) : null}
        {run.loadError ? <div className="solo-form-error">{run.loadError}</div> : null}
        {run.error ? <div className="solo-form-error">{run.error}</div> : null}
      </section>
      <SoloStepDrawer
        stage={selectedStage}
        detail={stepDetail}
        isLoading={stepDetailLoading}
        error={stepDetailError}
        onClose={onCloseStep}
        onRetryStep={(node) => void onRetryStep(run.threadId, node)}
        onContinueRun={() => void onContinueRun(run.threadId)}
      />
    </main>
  );
}
