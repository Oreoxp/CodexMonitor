// P7 Carrier — Team Mode app shell (replaces the old TeamMainApp fork).
//
// New ground-up shell for the P7 design: macOS frosted window
// (`.team-ui .win`, 13px radius) over the existing transparent Tauri window
// (real vibrancy via backdrop-filter — no fake IDE backdrop). Hosts the
// team-variant Sidebar + a routed screen (Roster / Office).
//
// 2b — owns the team RUNTIME: useTeamRuntime resolves a workspace + drives the
// sidecar lifecycle (connect → start → provision) so the team actually runs;
// useTeamLive subscribes to per-agent live status/activity. Both feed Roster.
//
// Window chrome: `.drag-strip` makes the title area draggable; macOS draws
// native traffic lights via `titleBarStyle:"Overlay"`; WindowCaptionControls
// renders the Windows caption buttons (no-op on macOS). Normal mode untouched.
import { useState } from "react";

import { WindowCaptionControls } from "@/features/layout/components/WindowCaptionControls";
import { TeamSidebar, type TeamView } from "./TeamSidebar";
import { RosterScreen } from "./RosterScreen";
import { OfficeScreen } from "./OfficeScreen";
import { useTeamRuntime } from "../hooks/useTeamRuntime";
import { useTeamLive } from "../hooks/useTeamLive";
import { useTeamRoster } from "../hooks/useTeamRoster";

export default function TeamApp() {
  const [view, setView] = useState<TeamView>("roster");
  const { workspaceId, provisionError, dismissError, provisioned, noTeam } =
    useTeamRuntime();
  const { agents, teamName } = useTeamRoster(workspaceId);
  const { byAgentId, runningCount, threadByAgentId } = useTeamLive(workspaceId, provisioned);

  return (
    <div className="team-ui">
      <div className="win">
        {/* Top drag region — same mechanism normal mode uses on its header
            (data-tauri-drag-region). Lets the user drag the window by the top. */}
        <div className="drag-strip" id="titlebar" data-tauri-drag-region />
        <WindowCaptionControls />
        <TeamSidebar view={view} onSelect={setView} />
        <div className="tm-stage">
          {provisionError ? (
            <div className="tm-banner" role="alert">
              <span className="tm-banner-body">团队启动失败：{provisionError}</span>
              <button
                type="button"
                className="tm-banner-dismiss"
                onClick={dismissError}
                aria-label="关闭"
              >
                ×
              </button>
            </div>
          ) : noTeam ? (
            <div className="tm-banner tm-banner-info">
              <span className="tm-banner-body">
                当前 workspace 还没有团队（team.json）。建队流将在后续接入；下方为示例花名册。
              </span>
            </div>
          ) : null}
          {view === "office" ? (
            <OfficeScreen
              workspaceId={workspaceId}
              agents={agents}
              live={byAgentId}
              threadByAgentId={threadByAgentId}
            />
          ) : (
            <RosterScreen
              agents={agents}
              teamName={teamName}
              live={byAgentId}
              runningCount={runningCount}
              onOpenOffice={() => setView("office")}
            />
          )}
        </div>
      </div>
    </div>
  );
}
