// P7-UI-3a — team-mode DM conversation renderer. NEW component (does NOT
// restyle the shared Messages/MessageRows — that would touch Normal mode).
// Renders ConversationItem[] in the new design language: .msg bubbles,
// .work-card for tool calls, @mention highlight, typing indicator.
//
// Scope: message bubbles (user/assistant) + tool work-cards + typing. reasoning
// items are skipped here (they belong to the ③b right-side thinking panel);
// diff/review/explore/userInput are skipped this block.
import { Fragment, useEffect, useRef, type ReactNode } from "react";

import type { ConversationItem } from "@/types";

export type ConversationPeer = { name: string; avatar: string; letter: string };

const MAX_OUTPUT = 280;

// Lightweight @mention / #channel highlight (text-level; not a ConversationItem
// kind). Markdown rendering is a deliberate non-goal this block.
function renderText(text: string): ReactNode[] {
  const parts: ReactNode[] = [];
  const re = /([@#][^\s@#，。,.:：、）)]+)/g;
  let last = 0;
  let key = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) parts.push(<Fragment key={key++}>{text.slice(last, m.index)}</Fragment>);
    parts.push(
      <span key={key++} className="mention">
        {m[0]}
      </span>,
    );
    last = m.index + m[0].length;
  }
  if (last < text.length) parts.push(<Fragment key={key++}>{text.slice(last)}</Fragment>);
  return parts;
}

function PeerAvatar({ peer }: { peer: ConversationPeer }) {
  return (
    <div className="avatar" style={{ background: peer.avatar }}>
      {peer.letter}
    </div>
  );
}

function Row({ item, peer }: { item: ConversationItem; peer: ConversationPeer }) {
  if (item.kind === "message") {
    if (item.role === "user") {
      return (
        <div className="msg me">
          <div className="col">
            <div className="bubble">{renderText(item.text)}</div>
          </div>
        </div>
      );
    }
    return (
      <div className="msg them">
        <PeerAvatar peer={peer} />
        <div className="col">
          <div className="bubble">{renderText(item.text)}</div>
        </div>
      </div>
    );
  }

  if (item.kind === "tool") {
    const output = item.output ? item.output.slice(0, MAX_OUTPUT) : null;
    return (
      <div className="msg them">
        <PeerAvatar peer={peer} />
        <div className="col">
          <div className="bubble">
            <div className="work-card" style={{ marginTop: 0 }}>
              <div className="wc-head">{item.title || item.toolType || "工具调用"}</div>
              {item.detail ? <div className="wc-body">{item.detail}</div> : null}
              {output ? (
                <div className="wc-res">
                  <span className="run">{output}</span>
                </div>
              ) : item.status ? (
                <div className="wc-res">
                  <span className="run">{item.status}</span>
                </div>
              ) : null}
            </div>
          </div>
        </div>
      </div>
    );
  }

  // reasoning → ③b panel; diff/review/explore/userInput → not rendered this block.
  return null;
}

export function TeamConversation({
  items,
  peer,
  busy,
}: {
  items: ConversationItem[];
  peer: ConversationPeer;
  busy: boolean;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [items, busy]);

  return (
    <div className="ot-scroll" ref={scrollRef}>
      <div className="daydiv">对话</div>
      {items.map((item) => (
        <Row key={item.id} item={item} peer={peer} />
      ))}
      {busy ? (
        <div className="typing">
          <PeerAvatar peer={peer} />
          <div className="dots">
            <i />
            <i />
            <i />
          </div>
        </div>
      ) : null}
    </div>
  );
}
