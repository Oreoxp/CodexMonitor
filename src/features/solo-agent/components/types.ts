import type { WorkspaceInfo } from "@/types";

export type AppMode = "code" | "solo";

export type SoloRunStatus =
  | "draft"
  | "planning"
  | "running"
  | "stale_running"
  | "interrupted"
  | "paused_on_error"
  | "waiting_approval"
  | "delivered"
  | "delivered_with_warnings"
  | "done"
  | "failed"
  | "unknown";

export type SoloTimelineStageId =
  | "intake"
  | "understand"
  | "context_scan"
  | "plan"
  | "approval"
  | "execute"
  | "inspect"
  | "diagnose"
  | "repair"
  | "recheck"
  | "deliver";

export type SoloTimelineStageStatus =
  | "pending"
  | "running"
  | "completed"
  | "waiting"
  | "failed"
  | "skipped";

export type SoloTimelineStage = {
  id: SoloTimelineStageId;
  label: string;
  status: SoloTimelineStageStatus;
};

export type PlanReviewDraft = {
  summary: string;
  steps: string[];
  revision: number;
};

export type PendingPlanApproval = {
  thread_id: string;
  node: string;
  draft: PlanReviewDraft;
};

export type SoloRun = {
  threadId: string;
  goal: string;
  title: string;
  status: SoloRunStatus;
  workspaceId: string;
  workspaceName: string;
  workspacePath: string;
  codexThreadId: string | null;
  currentNode: string | null;
  currentPhase: string | null;
  lastNode: string | null;
  failedNode: string | null;
  failureReason: string | null;
  timeline: SoloTimelineStage[];
  recentActivities: string[];
  pendingApproval: PendingPlanApproval | null;
  artifacts: string[];
  finalReportAvailable: boolean;
  loadError: string | null;
  error: string | null;
  createdAt: number;
  updatedAt: number;
};

export type SoloStepDetail = {
  thread_id: string;
  node: string;
  conversation: Record<string, unknown> | null;
  artifacts: string[];
  logs: string[];
  events: unknown[];
};

export type SoloWorkspaceSelectionProps = {
  workspaces: WorkspaceInfo[];
  activeWorkspace: WorkspaceInfo | null;
  onSelectWorkspace: (workspaceId: string) => void;
};
