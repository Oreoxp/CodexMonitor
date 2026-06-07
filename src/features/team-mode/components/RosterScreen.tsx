// P7 Roster (组织架构) — 1:1 reproduction of design_handoff_team_mode/Roster.html.
//
// Static agent config is REAL (name / role / model / toolsPreset → avatar,
// autonomy badge, permission toggles, reportsTo). LIVE fields (status / 实时活动
// / 今日用量 / 持久记忆 / 运行时长 / metrics 运行中·待批准·用量) are placeholders
// marked TODO(2b) — they need the sidecar + observation layer re-wired into the
// new shell. Search filters real name/role; status chips filter the (currently
// placeholder) status; "按角色分组" groups by the real role field.
//
// Out-of-P7-scope per phased plan, intentionally NOT built: independent 4-toggle
// permissions (toggles here are read-only, derived from toolsPreset) and org
// hierarchy / squad grouping (we group by role, the only data-backed grouping).
import { useMemo, useState } from "react";
import AlignLeft from "lucide-react/dist/esm/icons/align-left";
import MessageSquare from "lucide-react/dist/esm/icons/message-square";
import Plus from "lucide-react/dist/esm/icons/plus";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw";
import Search from "lucide-react/dist/esm/icons/search";

import type { RosterAgent } from "../hooks/useTeamRoster";
import type { AgentLive, AgentStatus } from "../hooks/useTeamLive";

type RosterScreenProps = {
  /** Shaped roster (from useTeamRoster, lifted to TeamApp). */
  agents: RosterAgent[];
  /** Team name for the breadcrumb. */
  teamName: string | null;
  /** Live per-agent status/activity (from useTeamLive). */
  live: Record<string, AgentLive>;
  /** Count of agents currently running a turn. */
  runningCount: number;
  /** "打开对话线程" jumps to the Office view. */
  onOpenOffice: () => void;
};

type StatusFilter = "all" | "running" | "waiting" | "idle";

const STATUS_CHIPS: { key: StatusFilter; label: string; dot?: string }[] = [
  { key: "all", label: "全部" },
  { key: "running", label: "运行中", dot: "run" },
  { key: "waiting", label: "待批准", dot: "wait" },
  { key: "idle", label: "空闲", dot: "idle" },
];

const STATUS_DOT: Record<AgentStatus, string> = {
  running: "run",
  idle: "idle",
  waiting: "wait",
  paused: "paused",
};

const AUTO_SEG = ["手动批准", "半自动", "全自动"];

export function RosterScreen({ agents, teamName, live, runningCount, onOpenOffice }: RosterScreenProps) {
  const [search, setSearch] = useState("");
  const [statusFilter, setStatusFilter] = useState<StatusFilter>("all");
  const [grouped, setGrouped] = useState(true);
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    return agents.filter((a) => {
      const status = live[a.id]?.status ?? "idle";
      if (statusFilter !== "all" && status !== statusFilter) return false;
      if (!q) return true;
      return (
        a.name.toLowerCase().includes(q) || a.role.toLowerCase().includes(q)
      );
    });
  }, [agents, search, statusFilter, live]);

  const groups = useMemo(() => {
    if (!grouped) {
      return [{ key: "__all__", label: null as string | null, rows: filtered }];
    }
    const order: string[] = [];
    const byRole = new Map<string, RosterAgent[]>();
    for (const a of filtered) {
      if (!byRole.has(a.role)) {
        byRole.set(a.role, []);
        order.push(a.role);
      }
      byRole.get(a.role)!.push(a);
    }
    return order.map((role) => ({ key: role, label: role, rows: byRole.get(role)! }));
  }, [filtered, grouped]);

  const total = agents.length;
  let rowSeq = 0; // running index to mark the visually-last row

  return (
    <main className="main">
      <div className="cfg-head">
        <div className="row1">
          <div>
            <div className="crumb">{teamName ?? "—"}</div>
            <h1>组织架构</h1>
          </div>
          <div className="spacer" />
          <button type="button" className="btn ghost" disabled title="即将到来">
            <RefreshCw />
            同步
          </button>
          <button type="button" className="btn primary" disabled title="即将到来">
            <Plus />
            添加 Agent
          </button>
        </div>
        <div className="tabs">
          <button type="button" className="on">
            成员<span className="count">{total}</span>
          </button>
          <button type="button" disabled>
            编排规则
          </button>
          <button type="button" disabled>
            共享记忆
          </button>
          <button type="button" disabled>
            用量
          </button>
        </div>
      </div>

      <div className="scroll">
        {/* metrics — only "Agent 总数" is real; the rest are live placeholders */}
        <div className="metrics">
          <div className="metric">
            <div className="k">Agent 总数</div>
            <div className="n">{total}</div>
            <Spark heights={[40, 55, 50, 70, 65, 100]} />
          </div>
          <div className="metric">
            <div className="k">
              <span className="dot run" />
              运行中
            </div>
            <div className="n">
              {runningCount}
              <small>/ {total}</small>
            </div>
            <Spark heights={[60, 80, 50, 90, 70, 60]} color="rgba(48,198,89,.4)" />
          </div>
          <div className="metric">
            <div className="k">
              <span className="dot wait" />
              待你批准
            </div>
            <div className="n" style={{ color: "#b06a00" }}>
              —
            </div>
            <div className="spark" style={{ alignItems: "center" }}>
              <span style={{ fontSize: 11, color: "var(--ink-3)" }}>实时数据 · 2b</span>
            </div>
          </div>
          <div className="metric">
            <div className="k">今日用量</div>
            <div className="n">—</div>
            <Spark heights={[30, 45, 60, 40, 85, 70]} />
          </div>
        </div>

        {/* toolbar */}
        <div className="toolbar">
          <div className="search">
            <Search />
            <input
              placeholder="搜索 agent…"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
          </div>
          <div className="chips">
            {STATUS_CHIPS.map((chip) => (
              <button
                key={chip.key}
                type="button"
                className={`chip${statusFilter === chip.key ? " on" : ""}`}
                onClick={() => setStatusFilter(chip.key)}
              >
                {chip.dot ? <span className={`dot ${chip.dot}`} /> : null}
                {chip.label}
              </button>
            ))}
          </div>
          <div className="spacer" />
          <button
            type="button"
            className={`btn ghost${grouped ? " on" : ""}`}
            onClick={() => setGrouped((g) => !g)}
            aria-pressed={grouped}
          >
            <AlignLeft />
            按角色分组
          </button>
        </div>

        {/* table */}
        <div className="tbl">
          <div className="thead">
            <span />
            <span>Agent</span>
            <span>状态 · 实时活动</span>
            <span>模型</span>
            <span>自治</span>
            <span>Workspace</span>
            <span style={{ textAlign: "right" }}>今日用量</span>
            <span />
          </div>

          {filtered.length === 0 ? (
            <div className="grp-h">
              <span>无匹配 agent</span>
              <span className="ll" />
            </div>
          ) : null}

          {groups.map((group) => (
            <div key={group.key}>
              {group.label !== null ? (
                <div className="grp-h">
                  <span>{group.label}</span>
                  <span className="c">{group.rows.length}</span>
                  <span className="ll" />
                </div>
              ) : null}
              {group.rows.map((a) => {
                rowSeq += 1;
                const isOpen = expandedId === a.id;
                const isLast = rowSeq === filtered.length && !isOpen;
                return (
                  <div key={a.id}>
                    <div
                      className={`trow${isOpen ? " open" : ""}${isLast ? " last" : ""}`}
                      onClick={() => setExpandedId(isOpen ? null : a.id)}
                    >
                      <div className="chk" />
                      <div className="cell-agent">
                        <div className="avatar" style={{ background: a.avatar }}>
                          {a.letter}
                        </div>
                        <div>
                          <div className="nm">{a.name}</div>
                          <div className="role">{a.role}</div>
                        </div>
                      </div>
                      <div className="cell-status">
                        {(() => {
                          const ls = live[a.id];
                          const status = ls?.status ?? "idle";
                          const running = status === "running";
                          return (
                            <>
                              <span
                                className={`dot ${STATUS_DOT[status]}${running ? " live" : ""}`}
                              />
                              <span className={`txt${running ? "" : " muted"}`}>
                                {ls?.activity ?? "—"}
                              </span>
                            </>
                          );
                        })()}
                      </div>
                      <div className="cell-mono">{a.model}</div>
                      <div>
                        <span className={`badge ${a.autonomy.cls}`}>
                          {a.autonomy.label}
                        </span>
                      </div>
                      <div className="cell-ws">{a.workspacePath}</div>
                      <div className="cell-use">—</div>
                      <div className="row-more">{isOpen ? "⌃" : "⌄"}</div>
                    </div>
                    {isOpen ? <ExpandPanel agent={a} onOpenOffice={onOpenOffice} /> : null}
                  </div>
                );
              })}
            </div>
          ))}
        </div>
      </div>
    </main>
  );
}

function Spark({ heights, color }: { heights: number[]; color?: string }) {
  return (
    <div className="spark">
      {heights.map((h, i) => (
        <i key={i} style={{ height: `${h}%`, ...(color ? { background: color } : {}) }} />
      ))}
    </div>
  );
}

function ExpandPanel({
  agent,
  onOpenOffice,
}: {
  agent: RosterAgent;
  onOpenOffice: () => void;
}) {
  const { perms } = agent;
  return (
    <div className="expand">
      <div className="grid">
        {/* 身份与模型 */}
        <div className="ex-card">
          <div className="lab">身份与模型</div>
          <div className="ex-row">
            名称<span className="v">{agent.name}</span>
          </div>
          <div className="ex-row">
            角色<span className="v">{agent.role}</span>
          </div>
          <div className="ex-row">
            模型<span className="v mono">{agent.model}</span>
          </div>
          <div className="ex-row">
            汇报给
            <span className="v">
              {agent.reportsTo ? (
                <>
                  <span
                    className="avatar"
                    style={{
                      width: 18,
                      height: 18,
                      borderRadius: 5,
                      fontSize: 9,
                      background: agent.reportsTo.avatar,
                    }}
                  >
                    {agent.reportsTo.letter}
                  </span>
                  {agent.reportsTo.name}
                </>
              ) : (
                "用户"
              )}
            </span>
          </div>
          <div className="lab" style={{ marginTop: 14 }}>
            自治级别
          </div>
          <div className="auto-seg">
            {AUTO_SEG.map((label, i) => (
              <button
                key={label}
                type="button"
                className={agent.autonomy.seg === i ? "on" : undefined}
                disabled
              >
                {label}
              </button>
            ))}
          </div>
        </div>

        {/* 权限 — read-only, derived from toolsPreset (4-toggle edit = out of P7 scope) */}
        <div className="ex-card">
          <div className="lab">权限</div>
          <PermRow label="读写文件" on={perms.fs} />
          <PermRow label="执行命令" on={perms.exec} />
          <PermRow label="网络访问" on={perms.net} />
          <PermRow label="Git 提交 / 推送" on={perms.git} />
          <div className="ex-row">
            持久记忆<span className="v tnum">—</span>
          </div>
        </div>

        {/* 实时活动 — placeholder (TODO 2b) */}
        <div className="ex-card">
          <div className="lab">实时活动</div>
          <div className="log">
            <span className="t">—</span> 实时活动 / 工具调用日志将在 2b 接入 agent 事件流
          </div>
        </div>
      </div>

      <div className="ex-actions">
        <button type="button" className="open-thread" onClick={onOpenOffice}>
          <MessageSquare />
          打开对话线程
        </button>
        <span className="badge">
          <span className="dot idle" />运行时长 —
        </span>
        <div className="spacer" />
        <button type="button" className="btn ghost" style={{ color: "var(--red)" }} disabled>
          移除
        </button>
        <button type="button" className="btn primary" disabled>
          保存
        </button>
      </div>
    </div>
  );
}

function PermRow({ label, on }: { label: string; on: boolean }) {
  return (
    <div className="ex-row">
      {label}
      <span className="v">
        <span className={`toggle${on ? " on" : ""}`} aria-disabled="true" />
      </span>
    </div>
  );
}
