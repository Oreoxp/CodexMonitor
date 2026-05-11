import type { SoloRun } from "./types";

type SoloRunListProps = {
  runs: SoloRun[];
  activeRunId: string | null;
  isLoading?: boolean;
  onSelectRun: (threadId: string) => void;
};

function statusLabel(status: SoloRun["status"]) {
  switch (status) {
    case "waiting_approval":
      return "等待审批";
    case "delivered":
    case "delivered_with_warnings":
    case "done":
      return "已完成";
    case "failed":
    case "paused_on_error":
      return "失败";
    case "planning":
      return "生成计划";
    case "running":
      return "进行中";
    case "stale_running":
    case "interrupted":
      return "可继续";
    case "unknown":
      return "待刷新";
    case "draft":
      return "草稿";
  }
}

function formatUpdatedAt(updatedAt: number) {
  if (!Number.isFinite(updatedAt) || updatedAt <= 0) {
    return "unknown";
  }
  return new Date(updatedAt).toLocaleString();
}

export function SoloRunList({
  runs,
  activeRunId,
  isLoading = false,
  onSelectRun,
}: SoloRunListProps) {
  if (!runs.length) {
    return (
      <div className="solo-run-list" aria-label="Solo runs">
        <div className="solo-run-list-empty">
          <div className="solo-run-list-empty-title">
            {isLoading ? "Loading Solo runs..." : "No Solo runs yet"}
          </div>
          <div className="solo-run-list-empty-body">
            {isLoading ? "Reading workspace history." : "Click New Solo to start."}
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="solo-run-list" aria-label="Solo runs">
      {runs.map((run) => (
        <button
          key={run.threadId}
          className={`solo-run-row${run.threadId === activeRunId ? " is-active" : ""}`}
          type="button"
          onClick={() => onSelectRun(run.threadId)}
        >
          <span className="solo-run-row-title">{run.title}</span>
          <span className={`solo-run-row-status is-${run.status}`}>
            {statusLabel(run.status)}
          </span>
          <span className="solo-run-row-updated">{formatUpdatedAt(run.updatedAt)}</span>
          <span className="solo-run-row-thread">{run.threadId}</span>
        </button>
      ))}
    </div>
  );
}
