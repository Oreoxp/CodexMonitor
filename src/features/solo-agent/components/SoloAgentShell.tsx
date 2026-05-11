import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { WorkspaceInfo } from "@/types";
import {
  langGraphSidecarContinue,
  langGraphSidecarGetState,
  langGraphSidecarInvoke,
  langGraphSidecarListRuns,
  langGraphSidecarReadRunEvents,
  langGraphSidecarReadStepDetail,
  langGraphSidecarResume,
  type SoloRunEventsResult,
  type SoloRunSummary,
  type SoloStepDetailResult,
} from "@services/tauri";
import type { LangGraphEventPayload } from "@services/events";
import { extractPlanReview, useLangGraphEvents } from "../hooks/useLangGraphEvents";
import type {
  AppMode,
  SoloRun,
  SoloRunStatus,
  SoloStepDetail,
  SoloTimelineStage,
  SoloTimelineStageId,
  SoloTimelineStageStatus,
} from "./types";
import { SoloMainPanel } from "./SoloMainPanel";
import { SoloSidebar } from "./SoloSidebar";

type SoloAgentShellProps = {
  mode: AppMode;
  onModeChange: (mode: AppMode) => void;
  workspaces: WorkspaceInfo[];
  activeWorkspace: WorkspaceInfo | null;
  onSelectWorkspace: (workspaceId: string) => void;
};

type MainMode = "empty" | "new" | "detail";

const TIMELINE_TEMPLATE: SoloTimelineStage[] = [
  { id: "intake", label: "创建任务", status: "pending" },
  { id: "understand", label: "理解需求", status: "pending" },
  { id: "context_scan", label: "扫描上下文", status: "pending" },
  { id: "plan", label: "生成计划", status: "pending" },
  { id: "approval", label: "等待审批", status: "pending" },
  { id: "execute", label: "执行修改", status: "pending" },
  { id: "inspect", label: "检查结果", status: "pending" },
  { id: "diagnose", label: "诊断问题", status: "pending" },
  { id: "repair", label: "修复问题", status: "pending" },
  { id: "recheck", label: "复查", status: "pending" },
  { id: "deliver", label: "生成报告", status: "pending" },
];

const NODE_TO_STAGE: Record<string, SoloTimelineStageId> = {
  intake: "intake",
  understand: "understand",
  understand_task: "understand",
  context_scan: "context_scan",
  plan: "plan",
  plan_node: "plan",
  approval: "approval",
  execute: "execute",
  inspect: "inspect",
  inspect_result: "inspect",
  diagnose: "diagnose",
  repair: "repair",
  recheck: "recheck",
  deliver: "deliver",
};

const STAGE_TO_NODE: Record<SoloTimelineStageId, string | null> = {
  intake: "intake",
  understand: "understand_task",
  context_scan: "context_scan",
  plan: "plan",
  approval: "plan",
  execute: "execute",
  inspect: "inspect_result",
  diagnose: "diagnose",
  repair: "repair",
  recheck: "recheck",
  deliver: "deliver",
};

function createInitialTimeline(): SoloTimelineStage[] {
  return TIMELINE_TEMPLATE.map((stage) => ({ ...stage }));
}

function setTimelineStage(
  timeline: SoloTimelineStage[],
  stageId: SoloTimelineStageId,
  status: SoloTimelineStageStatus,
) {
  return timeline.map((stage) =>
    stage.id === stageId ? { ...stage, status } : stage,
  );
}

function skipPendingRepairLoop(timeline: SoloTimelineStage[]) {
  const repairLoop = new Set<SoloTimelineStageId>(["diagnose", "repair", "recheck"]);
  return timeline.map((stage) =>
    repairLoop.has(stage.id) && stage.status === "pending"
      ? { ...stage, status: "skipped" as const }
      : stage,
  );
}

function timelineStageForNode(node: string | null): SoloTimelineStageId | null {
  if (!node) {
    return null;
  }
  return NODE_TO_STAGE[node] ?? null;
}

function getCurrentPhaseFromUpdate(event: LangGraphEventPayload) {
  const update = event.update;
  if (!update || typeof update !== "object" || Array.isArray(update)) {
    return null;
  }
  for (const nodeUpdate of Object.values(update)) {
    if (!nodeUpdate || typeof nodeUpdate !== "object" || Array.isArray(nodeUpdate)) {
      continue;
    }
    const phase = (nodeUpdate as Record<string, unknown>).phase;
    if (typeof phase === "string" && phase.trim()) {
      return phase;
    }
  }
  return null;
}

function getEventPhase(event: LangGraphEventPayload) {
  const updatePhase = getCurrentPhaseFromUpdate(event);
  if (updatePhase) {
    return updatePhase;
  }
  if (typeof event.phase === "string" && event.phase.trim()) {
    return event.phase;
  }
  return null;
}

function updateTimelineFromEvent(
  timeline: SoloTimelineStage[],
  event: LangGraphEventPayload,
  node: string | null,
): SoloTimelineStage[] {
  let next = timeline;
  const stage = timelineStageForNode(node);

  if (event.kind === "codex_task_started" && stage) {
    next = setTimelineStage(next, stage, "running");
  } else if (event.kind === "codex_task_completed" && stage) {
    next = setTimelineStage(next, stage, "completed");
  } else if (event.kind === "approval_required") {
    next = setTimelineStage(next, "plan", "completed");
    next = setTimelineStage(next, "approval", "waiting");
  } else if (event.kind === "resume") {
    const action = typeof event.action === "string" ? event.action : null;
    if (action === "approve") {
      next = setTimelineStage(next, "approval", "completed");
    } else if (action === "reject") {
      next = setTimelineStage(next, "approval", "completed");
      next = setTimelineStage(next, "plan", "running");
    }
  } else if (event.kind === "final_report_created" || event.kind === "graph_done") {
    next = skipPendingRepairLoop(next);
    next = setTimelineStage(next, "deliver", "completed");
  } else if (event.kind === "done") {
    next = skipPendingRepairLoop(next);
    next = setTimelineStage(next, "deliver", "completed");
  } else if (
    (event.kind === "error" || event.kind === "codex_task_failed" || event.kind === "task_failed") &&
    stage
  ) {
    next = setTimelineStage(next, stage, "failed");
  }

  return next;
}

function makeSoloThreadId() {
  const randomId =
    typeof crypto !== "undefined" && "randomUUID" in crypto
      ? crypto.randomUUID()
      : `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  return `solo-${randomId}`;
}

function makeTitle(goal: string) {
  const normalized = goal.trim().replace(/\s+/g, " ");
  if (!normalized) {
    return "Untitled Solo";
  }
  return normalized.length > 56 ? `${normalized.slice(0, 53)}...` : normalized;
}

function activityForEvent(event: LangGraphEventPayload) {
  const node = getEventNode(event);
  if (event.kind === "approval_required") {
    return "计划已生成，等待你审批";
  }
  if (event.kind === "resume") {
    return event.action === "reject" ? "已提交修改意见，Solo 正在重新生成计划" : "计划已批准，Solo 准备继续执行";
  }
  if (event.kind === "continue") {
    return "已继续执行";
  }
  if (event.kind === "final_report_created") {
    return "最终报告已生成";
  }
  if (event.kind === "graph_done" || event.kind === "done") {
    return "任务已完成";
  }
  if (event.kind === "codex_task_started") {
    switch (node) {
      case "understand_task":
      case "understand":
        return "开始理解需求";
      case "context_scan":
        return "开始扫描项目上下文";
      case "plan":
      case "plan_node":
        return "开始生成计划";
      case "execute":
        return "已开始执行修改";
      case "inspect_result":
      case "inspect":
        return "开始检查结果";
      case "diagnose":
        return "开始诊断问题";
      case "repair":
        return "开始修复问题";
      case "recheck":
        return "开始复查";
      default:
        return node ? `Codex 正在处理 ${node}` : "Solo 正在处理任务";
    }
  }
  if (event.kind === "codex_task_completed") {
    switch (node) {
      case "understand_task":
      case "understand":
        return "需求理解已完成";
      case "context_scan":
        return "项目上下文扫描已完成";
      case "plan":
      case "plan_node":
        return "计划已生成";
      case "execute":
        return "执行已完成";
      case "inspect_result":
      case "inspect":
        return "结果检查已完成";
      case "diagnose":
        return "问题诊断已完成";
      case "repair":
        return "修复已完成";
      case "recheck":
        return "复查已完成";
      default:
        return node ? `${node} 已完成` : "一个步骤已完成";
    }
  }
  if (event.kind === "error" || event.kind === "codex_task_failed" || event.kind === "task_failed") {
    return "任务遇到错误";
  }
  return null;
}

function addActivity(activities: string[], activity: string | null) {
  if (!activity) {
    return activities;
  }
  return [activity, ...activities.filter((item) => item !== activity)].slice(0, 5);
}

function parseTimestamp(value: string | null | undefined) {
  if (!value) {
    return Date.now();
  }
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : Date.now();
}

function normalizeStatus(status: string | null | undefined): SoloRunStatus {
  switch (status) {
    case "waiting_approval":
      return "waiting_approval";
    case "stale_running":
      return "stale_running";
    case "interrupted":
      return "interrupted";
    case "paused_on_error":
      return "paused_on_error";
    case "unknown":
      return "unknown";
    case "completed":
    case "done":
      return "done";
    case "delivered":
      return "delivered";
    case "delivered_with_warnings":
      return "delivered_with_warnings";
    case "failed":
      return "failed";
    case "planning":
      return "planning";
    case "draft":
      return "draft";
    case "running":
    default:
      return "running";
  }
}

function coerceEvent(value: unknown): LangGraphEventPayload | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  const event = value as Record<string, unknown>;
  if (event.type !== "evt" || typeof event.thread_id !== "string" || typeof event.kind !== "string") {
    return null;
  }
  return event as LangGraphEventPayload;
}

function buildRunFromSummary(summary: SoloRunSummary, workspace: WorkspaceInfo): SoloRun {
  const goal = summary.goal || summary.title || "";
  const status = normalizeStatus(summary.status);
  return {
    threadId: summary.thread_id,
    goal,
    title: summary.title || makeTitle(goal),
    status,
    workspaceId: summary.workspace_id || workspace.id,
    workspaceName: workspace.name,
    workspacePath: workspace.path,
    codexThreadId: summary.codex_thread_id ?? null,
    currentNode: summary.current_node ?? null,
    currentPhase: summary.phase ?? summary.current_node ?? null,
    lastNode: null,
    failedNode: null,
    failureReason: null,
    timeline: createInitialTimeline(),
    recentActivities: [],
    pendingApproval: null,
    artifacts: [],
    finalReportAvailable: false,
    loadError: null,
    error: null,
    createdAt: parseTimestamp(summary.created_at),
    updatedAt: parseTimestamp(summary.updated_at),
  };
}

function getEventNode(event: LangGraphEventPayload) {
  if (typeof event.node === "string" && event.node.trim()) {
    return event.node;
  }
  const update = event.update;
  if (update && typeof update === "object" && !Array.isArray(update)) {
    const [node] = Object.keys(update);
    return node || null;
  }
  const approval = event.approval;
  if (approval && typeof approval === "object" && !Array.isArray(approval)) {
    const node = (approval as Record<string, unknown>).node;
    return typeof node === "string" ? node : null;
  }
  return null;
}

function getUpdateStatus(event: LangGraphEventPayload): SoloRunStatus | null {
  const update = event.update;
  if (!update || typeof update !== "object" || Array.isArray(update)) {
    return null;
  }
  for (const nodeUpdate of Object.values(update)) {
    if (!nodeUpdate || typeof nodeUpdate !== "object" || Array.isArray(nodeUpdate)) {
      continue;
    }
    const status = (nodeUpdate as Record<string, unknown>).status;
    const waitingForApproval = (nodeUpdate as Record<string, unknown>).waitingForApproval;
    if (waitingForApproval === true) {
      return "waiting_approval";
    }
    if (status === "waiting_approval") {
      return "waiting_approval";
    }
    if (status === "paused_on_error") {
      return "paused_on_error";
    }
    if (status === "completed" || status === "done") {
      return "done";
    }
    if (status === "delivered") {
      return "delivered";
    }
    if (status === "failed") {
      return "failed";
    }
  }
  return null;
}

function mapEventStatus(event: LangGraphEventPayload): SoloRunStatus | null {
  switch (event.kind) {
    case "approval_required":
      return "waiting_approval";
    case "codex_task_started":
      return "running";
    case "codex_task_completed":
      return "running";
    case "resume":
      return event.action === "reject" ? "planning" : "running";
    case "continue":
      return "running";
    case "task_failed":
      return "paused_on_error";
    case "final_report_created":
      return "delivered";
    case "graph_done":
    case "done":
      return event.status === "failed" ? "failed" : "done";
    case "error":
    case "codex_task_failed":
      return "failed";
    case "update":
      return getUpdateStatus(event) ?? "running";
    default:
      return null;
  }
}

function getEventError(event: LangGraphEventPayload) {
  if (typeof event.error === "string") {
    return event.error;
  }
  const error = event.error;
  if (error && typeof error === "object" && !Array.isArray(error)) {
    const message = (error as Record<string, unknown>).message;
    return typeof message === "string" ? message : null;
  }
  return null;
}

function applyEventToRun(run: SoloRun, event: LangGraphEventPayload): SoloRun {
  const nextStatus = mapEventStatus(event);
  const activity = activityForEvent(event);
  if (!nextStatus) {
    return {
      ...run,
      recentActivities: addActivity(run.recentActivities, activity),
    };
  }
  const node = getEventNode(event);
  const phase = getEventPhase(event);
  const error = getEventError(event);
  const approval = extractPlanReview(event);
  return {
    ...run,
    status: nextStatus,
    currentNode:
      nextStatus === "done" || nextStatus === "delivered" || nextStatus === "failed"
        ? null
        : node ?? run.currentNode,
    currentPhase: phase ?? node ?? run.currentPhase,
    lastNode: node ?? run.lastNode,
    failedNode: nextStatus === "paused_on_error" ? node ?? run.failedNode : run.failedNode,
    failureReason: nextStatus === "paused_on_error" ? error ?? run.failureReason : run.failureReason,
    timeline: updateTimelineFromEvent(run.timeline, event, node),
    recentActivities: addActivity(run.recentActivities, activity),
    pendingApproval:
      approval ??
      (event.kind === "resume" || event.kind === "done" || event.kind === "graph_done"
        ? null
        : run.pendingApproval),
    error: error ?? run.error,
    updatedAt: Date.now(),
  };
}

function rebuildRunFromEvents(run: SoloRun, events: LangGraphEventPayload[]): SoloRun {
  const rebuilt = {
    ...run,
    timeline: createInitialTimeline(),
    pendingApproval: null,
    currentNode: null,
    currentPhase: null,
    lastNode: null,
    failedNode: null,
    failureReason: null,
  };
  const next = events.reduce(applyEventToRun, rebuilt);
  if (events.length === 0) {
    return run;
  }
  return {
    ...next,
    updatedAt: run.updatedAt,
  };
}

function extractApprovalFromSnapshot(snapshot: unknown) {
  if (!snapshot || typeof snapshot !== "object" || Array.isArray(snapshot)) {
    return null;
  }
  const tasks = (snapshot as Record<string, unknown>).tasks;
  if (!Array.isArray(tasks)) {
    return null;
  }
  for (const task of tasks) {
    if (!task || typeof task !== "object" || Array.isArray(task)) {
      continue;
    }
    const interrupts = (task as Record<string, unknown>).interrupts;
    if (!Array.isArray(interrupts)) {
      continue;
    }
    for (const interrupt of interrupts) {
      const value =
        interrupt && typeof interrupt === "object" && !Array.isArray(interrupt)
          ? (interrupt as Record<string, unknown>).value
          : null;
      const event = {
        type: "evt",
        thread_id:
          value && typeof value === "object" && !Array.isArray(value)
            ? (value as Record<string, unknown>).thread_id
            : "",
        kind: "approval_required",
        approval: value,
      };
      const approval = extractPlanReview(event as LangGraphEventPayload);
      if (approval) {
        return approval;
      }
    }
  }
  return null;
}

function extractFailureFromSnapshot(snapshot: unknown) {
  if (!snapshot || typeof snapshot !== "object" || Array.isArray(snapshot)) {
    return null;
  }
  const tasks = (snapshot as Record<string, unknown>).tasks;
  if (!Array.isArray(tasks)) {
    return null;
  }
  for (const task of tasks) {
    if (!task || typeof task !== "object" || Array.isArray(task)) {
      continue;
    }
    const interrupts = (task as Record<string, unknown>).interrupts;
    if (!Array.isArray(interrupts)) {
      continue;
    }
    for (const interrupt of interrupts) {
      const value =
        interrupt && typeof interrupt === "object" && !Array.isArray(interrupt)
          ? (interrupt as Record<string, unknown>).value
          : null;
      if (
        value &&
        typeof value === "object" &&
        !Array.isArray(value) &&
        (value as Record<string, unknown>).kind === "step_failure"
      ) {
        const record = value as Record<string, unknown>;
        return {
          node: typeof record.node === "string" ? record.node : null,
          phase: typeof record.phase === "string" ? record.phase : null,
          error: typeof record.error === "string" ? record.error : null,
        };
      }
    }
  }
  return null;
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function snapshotValues(snapshot: unknown) {
  if (!isPlainRecord(snapshot)) {
    return null;
  }
  const state = snapshot.state;
  if (isPlainRecord(state)) {
    return state;
  }
  const values = snapshot.values;
  return isPlainRecord(values) ? values : null;
}

function snapshotHasNext(snapshot: unknown) {
  if (!isPlainRecord(snapshot)) {
    return false;
  }
  const next = snapshot.next;
  if (Array.isArray(next)) {
    return next.length > 0;
  }
  if (typeof next === "string") {
    return next.trim().length > 0;
  }
  return Boolean(next);
}

function snapshotText(values: Record<string, unknown> | null, keys: string[]) {
  if (!values) {
    return null;
  }
  for (const key of keys) {
    const value = values[key];
    if (typeof value === "string" && value.trim()) {
      return value;
    }
  }
  return null;
}

function updateRunFromSnapshot(run: SoloRun, snapshot: unknown): SoloRun {
  const approval = extractApprovalFromSnapshot(snapshot);
  const failure = extractFailureFromSnapshot(snapshot);
  const values = snapshotValues(snapshot);
  const status = normalizeStatus(snapshotText(values, ["status"]));
  const currentNode = snapshotText(values, ["currentNode", "current_node", "node"]);
  const phase = snapshotText(values, ["phase", "currentPhase", "current_phase"]);
  const failedNode = snapshotText(values, ["failedNode", "failed_node"]);
  const failureReason = snapshotText(values, ["failureReason", "failure_reason", "lastError"]);
  const hasNext = snapshotHasNext(snapshot);

  if (approval) {
    return {
      ...run,
      status: "waiting_approval",
      pendingApproval: approval,
      currentNode: currentNode ?? "plan",
      currentPhase: phase ?? "plan",
      timeline: setTimelineStage(
        setTimelineStage(run.timeline, "plan", "completed"),
        "approval",
        "waiting",
      ),
      recentActivities: addActivity(run.recentActivities, "计划已生成，等待你审批"),
      loadError: null,
      updatedAt: Date.now(),
    };
  }

  if (failure) {
    const node = failure.node ?? currentNode ?? run.failedNode;
    return {
      ...run,
      status: "paused_on_error",
      pendingApproval: null,
      currentNode: node ?? run.currentNode,
      currentPhase: failure.phase ?? phase ?? node ?? run.currentPhase,
      failedNode: node,
      failureReason: failure.error ?? failureReason ?? run.failureReason,
      timeline: node
        ? setTimelineStage(run.timeline, timelineStageForNode(node) ?? "execute", "failed")
        : run.timeline,
      recentActivities: addActivity(run.recentActivities, "任务暂停，等待你决定下一步"),
      loadError: null,
      updatedAt: Date.now(),
    };
  }

  if (status === "paused_on_error") {
    return {
      ...run,
      status: "paused_on_error",
      pendingApproval: null,
      currentNode: failedNode ?? currentNode ?? run.currentNode,
      currentPhase: phase ?? failedNode ?? currentNode ?? run.currentPhase,
      failedNode: failedNode ?? currentNode ?? run.failedNode,
      failureReason: failureReason ?? run.failureReason,
      timeline: failedNode
        ? setTimelineStage(run.timeline, timelineStageForNode(failedNode) ?? "execute", "failed")
        : run.timeline,
      recentActivities: addActivity(run.recentActivities, "任务暂停，等待你决定下一步"),
      loadError: null,
      updatedAt: Date.now(),
    };
  }

  if (hasNext) {
    return {
      ...run,
      status: "stale_running",
      pendingApproval: null,
      currentNode: currentNode ?? run.currentNode,
      currentPhase: phase ?? currentNode ?? run.currentPhase,
      failedNode: null,
      failureReason: null,
      recentActivities: addActivity(run.recentActivities, "Solo 可以从上次进度继续"),
      loadError: null,
      updatedAt: Date.now(),
    };
  }

  if (status === "delivered" || status === "done" || status === "delivered_with_warnings") {
    return {
      ...run,
      status,
      pendingApproval: null,
      currentNode: null,
      currentPhase: phase ?? run.currentPhase,
      failedNode: null,
      failureReason: null,
      timeline: setTimelineStage(skipPendingRepairLoop(run.timeline), "deliver", "completed"),
      recentActivities: addActivity(run.recentActivities, "任务已完成"),
      loadError: null,
      updatedAt: Date.now(),
    };
  }

  if (status === "failed") {
    return {
      ...run,
      status: "failed",
      pendingApproval: null,
      currentNode: currentNode ?? run.currentNode,
      currentPhase: phase ?? run.currentPhase,
      failedNode: failedNode ?? currentNode ?? run.failedNode,
      failureReason: failureReason ?? run.failureReason,
      recentActivities: addActivity(run.recentActivities, "任务遇到错误"),
      loadError: null,
      updatedAt: Date.now(),
    };
  }

  return {
    ...run,
    status: status === "waiting_approval" ? "unknown" : status,
    pendingApproval: null,
    currentNode: currentNode ?? run.currentNode,
    currentPhase: phase ?? run.currentPhase,
    failedNode: failedNode ?? run.failedNode,
    failureReason: failureReason ?? run.failureReason,
    loadError: null,
    updatedAt: Date.now(),
  };
}

function normalizeStepDetail(result: SoloStepDetailResult): SoloStepDetail {
  return {
    thread_id: result.thread_id,
    node: result.node,
    conversation: result.conversation,
    artifacts: result.artifacts,
    logs: result.logs,
    events: result.events,
  };
}

export function SoloAgentShell({
  mode,
  onModeChange,
  workspaces,
  activeWorkspace,
  onSelectWorkspace,
}: SoloAgentShellProps) {
  const [runs, setRuns] = useState<SoloRun[]>([]);
  const [activeRunId, setActiveRunId] = useState<string | null>(null);
  const [mainMode, setMainMode] = useState<MainMode>("empty");
  const [goal, setGoal] = useState("");
  const [startError, setStartError] = useState<string | null>(null);
  const [isSubmitting, setIsSubmitting] = useState(false);
  const [runsLoading, setRunsLoading] = useState(false);
  const [selectedStepId, setSelectedStepId] = useState<SoloTimelineStageId | null>(null);
  const [stepDetail, setStepDetail] = useState<SoloStepDetail | null>(null);
  const [stepDetailLoading, setStepDetailLoading] = useState(false);
  const [stepDetailError, setStepDetailError] = useState<string | null>(null);
  const loadGenerationRef = useRef(0);
  const activeWorkspaceId = activeWorkspace?.id ?? null;

  const activeRun = useMemo(
    () => runs.find((run) => run.threadId === activeRunId) ?? null,
    [activeRunId, runs],
  );
  const activeApproval = activeRun?.pendingApproval ?? null;

  const updateRunFromEvent = useCallback((event: LangGraphEventPayload) => {
    setRuns((current) =>
      current.map((run) =>
        run.threadId === event.thread_id ? applyEventToRun(run, event) : run,
      ),
    );
  }, []);

  useLangGraphEvents({ onEvent: updateRunFromEvent });

  useEffect(() => {
    setRuns([]);
    setActiveRunId(null);
    setMainMode("empty");
    setStartError(null);
    setSelectedStepId(null);
    setStepDetail(null);
    setStepDetailError(null);
  }, [activeWorkspaceId]);

  const refreshRunState = useCallback(
    async (threadId: string) => {
      const run = runs.find((entry) => entry.threadId === threadId);
      const workspaceId = run?.workspaceId ?? activeWorkspace?.id ?? null;
      const workspaceCwd = run?.workspacePath ?? activeWorkspace?.path ?? null;
      if (!workspaceCwd) {
        return;
      }
      try {
        const snapshot = await langGraphSidecarGetState({
          threadId,
          workspaceId,
          workspaceCwd,
        });
        setRuns((current) =>
          current.map((entry) =>
            entry.threadId === threadId ? updateRunFromSnapshot(entry, snapshot) : entry,
          ),
        );
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        setRuns((current) =>
          current.map((entry) =>
            entry.threadId === threadId
              ? {
                  ...entry,
                  status: "unknown",
                  loadError: `状态刷新失败：${message}`,
                  updatedAt: Date.now(),
                }
              : entry,
          ),
        );
      }
    },
    [activeWorkspace, runs],
  );

  const loadRunDetails = useCallback(
    async (threadId: string) => {
      const run = runs.find((entry) => entry.threadId === threadId);
      const workspaceId = run?.workspaceId ?? activeWorkspace?.id ?? null;
      const workspaceCwd = run?.workspacePath ?? activeWorkspace?.path ?? null;
      if (!workspaceCwd) {
        return;
      }
      try {
        const result: SoloRunEventsResult = await langGraphSidecarReadRunEvents({
          threadId,
          workspaceId,
          workspaceCwd,
        });
        const events = result.events.map(coerceEvent).filter((event): event is LangGraphEventPayload => Boolean(event));
        setRuns((current) =>
          current.map((entry) => {
            if (entry.threadId !== threadId) {
              return entry;
            }
            const rebuilt = rebuildRunFromEvents(entry, events);
            return {
              ...rebuilt,
              artifacts: result.artifacts,
              finalReportAvailable: result.final_report_exists,
              loadError: null,
            };
          }),
        );
        await refreshRunState(threadId);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        setRuns((current) =>
          current.map((entry) =>
            entry.threadId === threadId
              ? { ...entry, loadError: message, updatedAt: Date.now() }
              : entry,
          ),
        );
      }
    },
    [activeWorkspace, refreshRunState, runs],
  );

  useEffect(() => {
    if (mode !== "solo" || !activeWorkspace) {
      return;
    }
    const generation = loadGenerationRef.current + 1;
    loadGenerationRef.current = generation;
    setRunsLoading(true);
    langGraphSidecarListRuns({
      workspaceId: activeWorkspace.id,
      workspaceCwd: activeWorkspace.path,
    })
      .then((summaries) => {
        if (loadGenerationRef.current !== generation) {
          return;
        }
        const historicalRuns = summaries
          .map((summary) => buildRunFromSummary(summary, activeWorkspace))
          .sort((a, b) => b.updatedAt - a.updatedAt);
        setRuns((current) => {
          const localById = new Map(current.map((run) => [run.threadId, run]));
          const merged = historicalRuns.map((run) => ({
            ...run,
            ...(localById.get(run.threadId) ?? {}),
          }));
          current.forEach((run) => {
            if (run.workspaceId === activeWorkspace.id && !merged.some((entry) => entry.threadId === run.threadId)) {
              merged.push(run);
            }
          });
          return merged.sort((a, b) => b.updatedAt - a.updatedAt);
        });
      })
      .catch((error) => {
        if (loadGenerationRef.current !== generation) {
          return;
        }
        setStartError(error instanceof Error ? error.message : String(error));
      })
      .finally(() => {
        if (loadGenerationRef.current === generation) {
          setRunsLoading(false);
        }
      });
  }, [activeWorkspace, mode]);

  const handleNewSolo = useCallback(() => {
    setGoal("");
    setStartError(null);
    setMainMode("new");
  }, []);

  const handleCancelNewSolo = useCallback(() => {
    setGoal("");
    setStartError(null);
    setMainMode(activeRunId ? "detail" : "empty");
  }, [activeRunId]);

  const handleSelectRun = useCallback((threadId: string) => {
    setActiveRunId(threadId);
    setStartError(null);
    setMainMode("detail");
    setSelectedStepId(null);
    setStepDetail(null);
    setStepDetailError(null);
    void loadRunDetails(threadId);
  }, [loadRunDetails]);

  const handleOpenStep = useCallback(
    async (stageId: SoloTimelineStageId) => {
      const run = runs.find((entry) => entry.threadId === activeRunId);
      const node = STAGE_TO_NODE[stageId];
      setSelectedStepId(stageId);
      setStepDetail(null);
      setStepDetailError(null);
      if (!run || !node) {
        return;
      }
      setStepDetailLoading(true);
      try {
        const result = await langGraphSidecarReadStepDetail({
          threadId: run.threadId,
          node,
          workspaceId: run.workspaceId,
          workspaceCwd: run.workspacePath,
        });
        setStepDetail(normalizeStepDetail(result));
      } catch (error) {
        setStepDetailError(error instanceof Error ? error.message : String(error));
      } finally {
        setStepDetailLoading(false);
      }
    },
    [activeRunId, runs],
  );

  const handleCloseStep = useCallback(() => {
    setSelectedStepId(null);
    setStepDetail(null);
    setStepDetailError(null);
    setStepDetailLoading(false);
  }, []);

  const handleStartSolo = useCallback(async () => {
    const trimmedGoal = goal.trim();
    if (!trimmedGoal || !activeWorkspace) {
      return;
    }
    const threadId = makeSoloThreadId();
    const now = Date.now();
    const run: SoloRun = {
      threadId,
      goal: trimmedGoal,
      title: makeTitle(trimmedGoal),
      status: "planning",
      workspaceId: activeWorkspace.id,
      workspaceName: activeWorkspace.name,
      workspacePath: activeWorkspace.path,
      codexThreadId: null,
      currentNode: "intake",
      currentPhase: "intake",
      lastNode: null,
      failedNode: null,
      failureReason: null,
      timeline: setTimelineStage(createInitialTimeline(), "intake", "completed"),
      recentActivities: ["已创建任务"],
      pendingApproval: null,
      artifacts: [],
      finalReportAvailable: false,
      loadError: null,
      error: null,
      createdAt: now,
      updatedAt: now,
    };
    setRuns((current) => [run, ...current]);
    setActiveRunId(threadId);
    setMainMode("detail");
    setGoal("");
    setStartError(null);
    setIsSubmitting(true);
    try {
      await langGraphSidecarInvoke({
        threadId,
        goal: trimmedGoal,
        workspaceId: activeWorkspace.id,
        workspaceCwd: activeWorkspace.path,
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setRuns((current) =>
        current.map((entry) =>
          entry.threadId === threadId
            ? {
                ...entry,
                status: "failed",
                currentNode: null,
                timeline: setTimelineStage(entry.timeline, "intake", "failed"),
                error: message,
                updatedAt: Date.now(),
              }
            : entry,
        ),
      );
      setStartError(message);
    } finally {
      setIsSubmitting(false);
    }
  }, [activeWorkspace, goal]);

  const handleApprovePlan = useCallback(
    async (threadId: string) => {
      const run = runs.find((entry) => entry.threadId === threadId);
      await langGraphSidecarResume({
        threadId,
        action: "approve",
        workspaceId: run?.workspaceId ?? activeWorkspace?.id ?? null,
        workspaceCwd: run?.workspacePath ?? activeWorkspace?.path ?? null,
      });
      setRuns((current) =>
        current.map((entry) =>
          entry.threadId === threadId
            ? {
                ...entry,
                status: "running",
                currentPhase: "execute",
                timeline: setTimelineStage(entry.timeline, "approval", "completed"),
                pendingApproval: null,
                updatedAt: Date.now(),
              }
            : entry,
        ),
      );
    },
    [activeWorkspace, runs],
  );

  const handleRejectPlan = useCallback(
    async (threadId: string, feedback: string) => {
      const run = runs.find((entry) => entry.threadId === threadId);
      await langGraphSidecarResume({
        threadId,
        action: "reject",
        feedback: feedback.trim() || undefined,
        workspaceId: run?.workspaceId ?? activeWorkspace?.id ?? null,
        workspaceCwd: run?.workspacePath ?? activeWorkspace?.path ?? null,
      });
      setRuns((current) =>
        current.map((entry) =>
          entry.threadId === threadId
            ? {
                ...entry,
                status: "planning",
                currentPhase: "plan",
                timeline: setTimelineStage(
                  setTimelineStage(entry.timeline, "approval", "completed"),
                  "plan",
                  "running",
                ),
                pendingApproval: null,
                updatedAt: Date.now(),
              }
            : entry,
        ),
      );
    },
    [activeWorkspace, runs],
  );

  const handleContinueRun = useCallback(
    async (threadId: string) => {
      const run = runs.find((entry) => entry.threadId === threadId);
      setRuns((current) =>
        current.map((entry) =>
          entry.threadId === threadId
            ? {
                ...entry,
                status: "running",
                loadError: null,
                recentActivities: addActivity(entry.recentActivities, "正在继续执行"),
                updatedAt: Date.now(),
              }
            : entry,
        ),
      );
      try {
        await langGraphSidecarContinue({
          threadId,
          workspaceId: run?.workspaceId ?? activeWorkspace?.id ?? null,
          workspaceCwd: run?.workspacePath ?? activeWorkspace?.path ?? null,
        });
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        setRuns((current) =>
          current.map((entry) =>
            entry.threadId === threadId
              ? {
                  ...entry,
                  status: "unknown",
                  loadError: `继续执行失败：${message}`,
                  updatedAt: Date.now(),
                }
              : entry,
          ),
        );
      }
    },
    [activeWorkspace, runs],
  );

  const handleRetryStep = useCallback(
    async (threadId: string, node: string) => {
      setRuns((current) =>
        current.map((entry) =>
          entry.threadId === threadId
            ? {
                ...entry,
                status: "running",
                currentNode: node,
                currentPhase: node,
                loadError: null,
                recentActivities: addActivity(entry.recentActivities, "正在重试失败步骤"),
                updatedAt: Date.now(),
              }
            : entry,
        ),
      );
      await handleContinueRun(threadId);
    },
    [handleContinueRun],
  );

  const handleViewLogs = useCallback((threadId: string) => {
    setRuns((current) =>
      current.map((entry) =>
        entry.threadId === threadId
          ? {
              ...entry,
              recentActivities: addActivity(entry.recentActivities, "日志可在恢复内容中查看"),
            }
          : entry,
      ),
    );
  }, []);

  const handleViewReport = useCallback((threadId: string) => {
    setRuns((current) =>
      current.map((entry) =>
        entry.threadId === threadId
          ? {
              ...entry,
              recentActivities: addActivity(
                entry.recentActivities,
                entry.finalReportAvailable ? "最终报告已生成" : "暂未找到最终报告",
              ),
            }
          : entry,
      ),
    );
  }, []);

  return (
    <div className="solo-agent-shell">
      <SoloSidebar
        mode={mode}
        runs={runs}
        activeRunId={activeRunId}
        runsLoading={runsLoading}
        onModeChange={onModeChange}
        workspaces={workspaces}
        activeWorkspace={activeWorkspace}
        onSelectWorkspace={onSelectWorkspace}
        onNewSolo={handleNewSolo}
        onSelectRun={handleSelectRun}
      />
      <SoloMainPanel
        activeWorkspace={activeWorkspace}
        mode={mainMode}
        activeRun={activeRun}
        activeApproval={activeApproval}
        goal={goal}
        isSubmitting={isSubmitting}
        error={startError}
        onGoalChange={setGoal}
        onStart={handleStartSolo}
        onCancel={handleCancelNewSolo}
        onApprovePlan={handleApprovePlan}
        onRejectPlan={handleRejectPlan}
        onContinueRun={handleContinueRun}
        onRetryStep={handleRetryStep}
        onRefreshState={refreshRunState}
        onViewLogs={handleViewLogs}
        onViewReport={handleViewReport}
        onNewSolo={handleNewSolo}
        selectedStepId={selectedStepId}
        stepDetail={stepDetail}
        stepDetailLoading={stepDetailLoading}
        stepDetailError={stepDetailError}
        onOpenStep={handleOpenStep}
        onCloseStep={handleCloseStep}
      />
      <aside className="solo-right-panel" aria-label="Solo details" />
    </div>
  );
}
