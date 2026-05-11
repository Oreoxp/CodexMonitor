import type { SoloStepDetail, SoloTimelineStage } from "./types";

type SoloStepDrawerProps = {
  stage: SoloTimelineStage | null;
  detail: SoloStepDetail | null;
  isLoading: boolean;
  error: string | null;
  onClose: () => void;
  onRetryStep: (node: string) => void;
  onContinueRun: () => void;
};

function textFromConversation(conversation: Record<string, unknown> | null, key: string) {
  const value = conversation?.[key];
  if (typeof value === "string") {
    return value;
  }
  if (value === null || value === undefined) {
    return "";
  }
  return JSON.stringify(value, null, 2);
}

const statusLabel: Record<SoloTimelineStage["status"], string> = {
  pending: "等待中",
  running: "进行中",
  completed: "已完成",
  waiting: "等待你操作",
  failed: "失败",
  skipped: "跳过",
};

export function SoloStepDrawer({
  stage,
  detail,
  isLoading,
  error,
  onClose,
  onRetryStep,
  onContinueRun,
}: SoloStepDrawerProps) {
  if (!stage) {
    return null;
  }

  const prompt = textFromConversation(detail?.conversation ?? null, "prompt");
  const response = textFromConversation(detail?.conversation ?? null, "raw_response");
  const parsed = textFromConversation(detail?.conversation ?? null, "parsed_result");
  const stepError = textFromConversation(detail?.conversation ?? null, "error");

  return (
    <aside className="solo-step-drawer" aria-label="Step detail">
      <div className="solo-step-drawer-header">
        <div>
          <div className="solo-detail-section-header">步骤详情</div>
          <h2>{stage.label}</h2>
          <div className={`solo-run-status-pill is-${stage.status}`}>
            {statusLabel[stage.status]}
          </div>
        </div>
        <button className="oc-button" type="button" onClick={onClose}>
          Close
        </button>
      </div>

      {stage.status === "failed" ? (
        <div className="solo-action-bar">
          <button className="oc-button oc-button-primary" onClick={() => onRetryStep(detail?.node ?? stage.id)}>
            Retry Current Step
          </button>
          <button className="oc-button" onClick={onContinueRun}>
            Continue Anyway
          </button>
        </div>
      ) : null}

      {isLoading ? <div className="solo-activity-empty">正在读取步骤详情……</div> : null}
      {error ? <div className="solo-form-error">{error}</div> : null}
      {!isLoading && !error && stage.status === "pending" ? (
        <div className="solo-activity-empty">这个步骤还没有开始。</div>
      ) : null}
      {!isLoading && !error && stage.status === "running" ? (
        <div className="solo-activity-empty">正在执行中，结果生成后会显示在这里。</div>
      ) : null}

      {!isLoading && !error ? (
        <div className="solo-step-drawer-body">
          <section>
            <div className="solo-detail-section-header">Prompt / 输入</div>
            <pre>{prompt || "暂无输入记录。"}</pre>
          </section>
          <section>
            <div className="solo-detail-section-header">Codex Response / 输出</div>
            <pre>{response || "暂无输出记录。"}</pre>
          </section>
          <section>
            <div className="solo-detail-section-header">Parsed Result / 摘要</div>
            <pre>{parsed || "暂无结构化摘要。"}</pre>
          </section>
          {stepError ? <div className="solo-form-error">{stepError}</div> : null}
          <section>
            <div className="solo-detail-section-header">产物</div>
            {detail?.artifacts.length ? (
              <ul className="solo-step-file-list">
                {detail.artifacts.map((artifact) => (
                  <li key={artifact}>{artifact}</li>
                ))}
              </ul>
            ) : (
              <div className="solo-activity-empty">暂无相关产物。</div>
            )}
          </section>
          <section>
            <div className="solo-detail-section-header">相关事件</div>
            {detail?.events.length ? (
              <ul className="solo-step-file-list">
                {detail.events.map((event, index) => (
                  <li key={index}>{JSON.stringify(event)}</li>
                ))}
              </ul>
            ) : (
              <div className="solo-activity-empty">暂无相关事件。</div>
            )}
          </section>
          <section>
            <div className="solo-detail-section-header">相关日志</div>
            {detail?.logs.length ? (
              <ul className="solo-step-file-list">
                {detail.logs.map((log) => (
                  <li key={log}>{log}</li>
                ))}
              </ul>
            ) : (
              <div className="solo-activity-empty">暂无相关日志。</div>
            )}
          </section>
        </div>
      ) : null}
    </aside>
  );
}
