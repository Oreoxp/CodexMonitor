// P7-UI-3b — Office right column "当前思考" (reasoning panel, #aside-dm).
// Builds the .think step timeline from the SAME useThreadItems stream the DM
// view uses: kind:"reasoning" items become steps, kind:"tool" items become
// tool-call pills under the preceding step. The last step is rendered "active"
// (blue glow + cursor) while the agent's turn is running, else "done".
//
// Data sources: reasoning + tool items (real, resume + live). "pending" future
// steps need a plan source (tasks) — not wired, so omitted. Task title/progress/
// tokens in the task-strip have no per-agent source → placeholder (live activity
// is shown for "正在处理"). Live reasoning-text streaming is a polish (steps
// currently append on item completion).
import { useMemo } from "react";
import Terminal from "lucide-react/dist/esm/icons/terminal";

import type { ConversationItem } from "@/types";

type Step = { id: string; title: string; reason: string; tools: string[] };

function firstLine(text: string): string {
  const line = text.split("\n").map((l) => l.trim()).find(Boolean) ?? "";
  return line.length > 64 ? `${line.slice(0, 64)}…` : line;
}

function buildSteps(items: ConversationItem[]): Step[] {
  const steps: Step[] = [];
  for (const it of items) {
    if (it.kind === "reasoning") {
      const title = firstLine(it.summary) || firstLine(it.content) || "推理";
      const reason = it.content.trim() && firstLine(it.content) !== title ? it.content.trim() : "";
      steps.push({ id: it.id, title, reason, tools: [] });
    } else if (it.kind === "tool") {
      const pill = (it.detail || it.title || it.toolType || "").trim();
      if (!pill) continue;
      const last = steps[steps.length - 1];
      if (last) last.tools.push(pill);
      else steps.push({ id: it.id, title: it.title || it.toolType || "工具调用", reason: "", tools: [pill] });
    }
  }
  return steps;
}

function ThinkStep({ step, last, active }: { step: Step; last: boolean; active: boolean }) {
  return (
    <div className={`tk${last ? " last" : ""}`}>
      <div className="rail">
        <div className={`node ${active ? "active" : "done"}`}>{active ? "" : "✓"}</div>
        {!last ? <div className="line" /> : null}
      </div>
      <div className="body">
        <div className="ttl">
          {step.title}
          {active ? <span className="cursor" /> : null}
        </div>
        {step.reason ? <div className="reason">{step.reason}</div> : null}
        {step.tools.map((tool, i) => (
          <span key={i} className="tool">
            <Terminal />
            {tool}
          </span>
        ))}
      </div>
    </div>
  );
}

export function TeamThinkingPanel({
  agentName,
  items,
  busy,
  activity,
}: {
  agentName: string;
  items: ConversationItem[];
  busy: boolean;
  activity: string | null;
}) {
  const steps = useMemo(() => buildSteps(items), [items]);

  return (
    <aside className="oaside">
      <div className="oa-head">
        <span className="t">当前思考 · {agentName}</span>
        {busy ? (
          <span className="live-pill">
            <span className="dot run live" style={{ width: 6, height: 6 }} />
            实时
          </span>
        ) : null}
      </div>

      <div className="task-strip">
        <div className="k">正在处理</div>
        <div className="v">{activity ?? (busy ? "运行中…" : "暂无任务")}</div>
        <div className="bar">
          <i style={{ width: busy ? "66%" : "0%" }} />
        </div>
        <div className="sub">子任务 — · 用量 —（任务/进度实时数据后续接入）</div>
      </div>

      <div className="think">
        {steps.length === 0 ? (
          <div className="think-empty">
            {busy ? "正在思考…推理步骤会在产生时实时显示。" : "暂无推理步骤。agent 运行时这里实时展示推理链与工具调用。"}
          </div>
        ) : (
          steps.map((step, i) => (
            <ThinkStep
              key={step.id}
              step={step}
              last={i === steps.length - 1}
              active={busy && i === steps.length - 1}
            />
          ))
        )}
      </div>
    </aside>
  );
}
