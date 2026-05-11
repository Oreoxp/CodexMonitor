import type { SoloTimelineStage } from "./types";

type SoloTimelineProps = {
  stages: SoloTimelineStage[];
  onSelectStage?: (stageId: SoloTimelineStage["id"]) => void;
};

export function SoloTimeline({ stages, onSelectStage }: SoloTimelineProps) {
  const statusLabel: Record<SoloTimelineStage["status"], string> = {
    pending: "等待中",
    running: "进行中",
    completed: "已完成",
    waiting: "等待你操作",
    failed: "失败",
    skipped: "跳过",
  };

  return (
    <section className="solo-timeline" aria-label="Solo run timeline">
      <div className="solo-detail-section-header">任务进度</div>
      <ol className="solo-timeline-list">
        {stages.map((stage) => (
          <li key={stage.id}>
            <button
              className={`solo-timeline-item is-${stage.status}`}
              type="button"
              onClick={() => onSelectStage?.(stage.id)}
            >
              <span className="solo-timeline-marker" aria-hidden />
              <span className="solo-timeline-label">{stage.label}</span>
              <span className="solo-timeline-status">{statusLabel[stage.status]}</span>
            </button>
          </li>
        ))}
      </ol>
    </section>
  );
}
