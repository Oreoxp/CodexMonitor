// P7 Carrier — team-variant sidebar (264px). 1:1 with
// design_handoff_team_mode (chrome.css `.sidebar` + README "共用外壳").
// Normal|Team segment, 项目 header, hpio project switcher, nav, usage
// footer. Routing is local view state (no router in this app); the
// Normal segment flips app mode back to <MainApp/> via useAppMode.
//
// Data is placeholder this block (counts / usage / project name);
// real agent + workspace data wires in the Roster/Office slices.
import type { LucideIcon } from "lucide-react";
import Network from "lucide-react/dist/esm/icons/network";
import MessageSquare from "lucide-react/dist/esm/icons/message-square";
import Activity from "lucide-react/dist/esm/icons/activity";
import Database from "lucide-react/dist/esm/icons/database";
import Settings from "lucide-react/dist/esm/icons/settings";
import PanelLeft from "lucide-react/dist/esm/icons/panel-left";

import { useAppMode } from "../hooks/useAppMode";

export type TeamView = "roster" | "office";

type NavItem = {
  view: TeamView | "activity" | "shared-memory";
  label: string;
  icon: LucideIcon;
  count?: string;
  unread?: string;
  disabled?: boolean;
};

// 活动 / 共享记忆 are nav placeholders (disabled / coming-soon) per the
// phased-plan P7 carrier scope — Org-chart + Office are the live views.
const NAV: NavItem[] = [
  { view: "roster", label: "组织架构", icon: Network, count: "5" },
  { view: "office", label: "办公室", icon: MessageSquare, unread: "3" },
  { view: "activity", label: "活动", icon: Activity, disabled: true },
  { view: "shared-memory", label: "共享记忆", icon: Database, disabled: true },
];

type TeamSidebarProps = {
  view: TeamView;
  onSelect: (view: TeamView) => void;
};

export function TeamSidebar({ view, onSelect }: TeamSidebarProps) {
  const [mode, setMode] = useAppMode();

  return (
    <aside className="sidebar">
      {/* Normal | Team — selecting Normal returns to <MainApp/> */}
      <div className="sb-mode" role="tablist" aria-label="App mode">
        <button
          type="button"
          role="tab"
          aria-selected={mode === "normal"}
          className={mode === "normal" ? "on" : undefined}
          onClick={() => setMode("normal")}
        >
          Normal
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={mode === "team"}
          className={mode === "team" ? "on" : undefined}
          onClick={mode === "team" ? undefined : () => setMode("team")}
        >
          Team
        </button>
      </div>

      <nav className="sb-nav" aria-label="Team navigation">
        {NAV.map((item) => {
          const ItemIcon = item.icon;
          const active = !item.disabled && item.view === view;
          return (
            <button
              key={item.view}
              type="button"
              className={`nav${active ? " on" : ""}`}
              disabled={item.disabled}
              aria-current={active ? "page" : undefined}
              title={item.disabled ? "即将到来" : undefined}
              onClick={
                item.disabled
                  ? undefined
                  : () => onSelect(item.view as TeamView)
              }
            >
              <ItemIcon />
              <span className="lab">{item.label}</span>
              {item.count ? <span className="n">{item.count}</span> : null}
              {item.unread ? (
                <span className="n unread">{item.unread}</span>
              ) : null}
            </button>
          );
        })}
      </nav>

      <div className="sb-foot">
        <div className="usage-l">用量</div>
        <div className="usage-r">
          <span className="k">本次会话</span>
          <span className="v tnum">2.4M tok</span>
        </div>
        <div className="sb-btns">
          <button type="button">
            <Settings />
            设置
          </button>
          <button type="button" aria-label="更多">
            <PanelLeft />
          </button>
        </div>
      </div>
    </aside>
  );
}
