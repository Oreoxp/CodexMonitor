// P7-UI-3a — Office (办公室) DM three-column. The S1 conversation landing /
// smoke screen: pick an agent → see its thread (resume + live) → send a message.
// 1:1 with design_handoff_team_mode/Office.html (#dm). Channel group-chat +
// reasoning panel (right column) are out of scope this block (placeholders).
import { useEffect, useState } from "react";
import AppWindow from "lucide-react/dist/esm/icons/app-window";
import Check from "lucide-react/dist/esm/icons/check";
import Ellipsis from "lucide-react/dist/esm/icons/ellipsis";
import Paperclip from "lucide-react/dist/esm/icons/paperclip";
import Pause from "lucide-react/dist/esm/icons/pause";
import Search from "lucide-react/dist/esm/icons/search";
import Send from "lucide-react/dist/esm/icons/send";
import SquarePen from "lucide-react/dist/esm/icons/square-pen";

import { sendUserMessage } from "@services/tauri";
import type { RosterAgent } from "../hooks/useTeamRoster";
import type { AgentLive, AgentStatus } from "../hooks/useTeamLive";
import { useThreadItems } from "../hooks/useThreadItems";
import { TeamConversation } from "./TeamConversation";
import { TeamThinkingPanel } from "./TeamThinkingPanel";

type OfficeScreenProps = {
  workspaceId: string | null;
  agents: RosterAgent[];
  live: Record<string, AgentLive>;
  threadByAgentId: Record<string, string>;
};

const CORNER: Record<AgentStatus, string> = {
  running: "run",
  waiting: "wait",
  idle: "idle",
  paused: "idle",
};

export function OfficeScreen({ workspaceId, agents, live, threadByAgentId }: OfficeScreenProps) {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);

  // Keep selection valid as agents load / change.
  useEffect(() => {
    setSelectedId((cur) =>
      cur && agents.some((a) => a.id === cur) ? cur : agents[0]?.id ?? null,
    );
  }, [agents]);

  const selected = agents.find((a) => a.id === selectedId) ?? null;
  const threadId = selected ? threadByAgentId[selected.id] ?? null : null;
  const { items, appendLocalUser } = useThreadItems(workspaceId, threadId);
  const running = selected ? live[selected.id]?.status === "running" : false;

  const send = async () => {
    const text = draft.trim();
    if (!text || !workspaceId || !threadId || sending) return;
    setSending(true);
    setDraft("");
    appendLocalUser(text);
    try {
      await sendUserMessage(workspaceId, threadId, text);
    } catch (err) {
      console.error("[team-mode] send_user_message failed", err);
    } finally {
      setSending(false);
    }
  };

  return (
    <main className="main">
      <div className="office">
        {/* col 1 — conversation list */}
        <div className="olist">
          <div className="ol-head">
            <h1>办公室</h1>
            <button type="button" className="ico" aria-label="搜索" disabled>
              <Search />
            </button>
            <button type="button" className="ico" aria-label="发起对话" disabled>
              <SquarePen />
            </button>
          </div>
          <div className="ol-tabs">
            <button type="button" className="on">
              私聊
            </button>
            <button type="button" disabled title="频道群聊（暂未开放）">
              频道
            </button>
          </div>
          <div className="ol-scroll">
            <div className="ol-sec">私聊 · 你与成员</div>
            {agents.map((a) => {
              const ls = live[a.id];
              const status = ls?.status ?? "idle";
              const bound = Boolean(threadByAgentId[a.id]);
              return (
                <button
                  key={a.id}
                  type="button"
                  className={`ci${selectedId === a.id ? " on" : ""}${bound ? "" : " disabled"}`}
                  disabled={!bound}
                  onClick={bound ? () => setSelectedId(a.id) : undefined}
                >
                  <div className="ava-wrap">
                    <div className="avatar" style={{ background: a.avatar }}>
                      {a.letter}
                    </div>
                    <span className={`corner ${CORNER[status]}`} />
                  </div>
                  <div className="ci-body">
                    <div className="ci-top">
                      <span className="ci-name">{a.name}</span>
                      <span className="ci-role">{a.role}</span>
                    </div>
                    <div className="ci-sub">
                      <span className={`ci-msg${status === "running" ? " live" : ""}`}>
                        {ls?.activity ?? (bound ? "—" : "未就绪")}
                      </span>
                    </div>
                  </div>
                </button>
              );
            })}

            <div className="ol-sec">协作频道 · 成员之间</div>
            <div className="ci disabled" aria-disabled="true">
              <div className="ch-tile">#</div>
              <div className="ci-body">
                <div className="ci-top">
                  <span className="ci-name">协作频道</span>
                </div>
                <div className="ci-sub">
                  <span className="ci-msg">群聊视图暂未开放（底层 Dev↔Dev 已在 S1 支持）</span>
                </div>
              </div>
            </div>
          </div>
        </div>

        {/* col 2 — DM thread */}
        <div className="othread">
          {selected && threadId ? (
            <>
              <div className="ot-head">
                <div className="avatar" style={{ background: selected.avatar }}>
                  {selected.letter}
                </div>
                <div>
                  <div className="nm">
                    {selected.name} <span className={`dot ${running ? "run live" : "idle"}`} />
                  </div>
                  <div className="meta">
                    <span>{selected.role}</span>
                    <span className="mono">{selected.workspacePath}</span>
                    <span>· {running ? "运行中" : "空闲"}</span>
                  </div>
                </div>
                <div className="spacer" />
                <button type="button" className="ot-act" disabled title="查看 workspace">
                  <AppWindow />
                </button>
                <button type="button" className="ot-act" disabled title="暂停">
                  <Pause />
                </button>
                <button type="button" className="ot-act" disabled title="更多">
                  <Ellipsis />
                </button>
              </div>

              <TeamConversation
                items={items}
                peer={{ name: selected.name, avatar: selected.avatar, letter: selected.letter }}
                busy={running}
              />

              <div className="composer">
                <div className="cmp-quick">
                  <button type="button" className="q approve" disabled>
                    <Check />
                    批准操作
                  </button>
                  <button type="button" className="q" disabled>
                    暂停
                  </button>
                  <button type="button" className="q" disabled>
                    查看变更
                  </button>
                </div>
                <div className="cmp-box">
                  <button type="button" className="att" aria-label="附件" disabled>
                    <Paperclip />
                  </button>
                  <textarea
                    rows={1}
                    placeholder={`发消息给 ${selected.name}…`}
                    value={draft}
                    onChange={(e) => setDraft(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && !e.shiftKey) {
                        e.preventDefault();
                        void send();
                      }
                    }}
                  />
                  <button
                    type="button"
                    className="send"
                    aria-label="发送"
                    disabled={!draft.trim() || sending}
                    onClick={() => void send()}
                  >
                    <Send />
                  </button>
                </div>
              </div>
            </>
          ) : (
            <div className="ot-empty">
              {agents.length === 0
                ? "没有团队成员。"
                : "选择一位成员开始对话；线程将在 provision 完成后就绪。"}
            </div>
          )}
        </div>

        {/* col 3 — thinking panel (③b) */}
        {selected ? (
          <TeamThinkingPanel
            agentName={selected.name}
            items={items}
            busy={running}
            activity={live[selected.id]?.activity ?? null}
          />
        ) : (
          <aside className="oaside">
            <div className="oa-head">当前思考</div>
          </aside>
        )}
      </div>
    </main>
  );
}
