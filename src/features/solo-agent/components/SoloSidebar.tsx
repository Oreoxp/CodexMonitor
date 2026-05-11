import Plus from "lucide-react/dist/esm/icons/plus";
import type { AppMode, SoloRun, SoloWorkspaceSelectionProps } from "./types";
import { ModeSwitcher } from "./ModeSwitcher";
import { SoloRunList } from "./SoloRunList";

type SoloSidebarProps = SoloWorkspaceSelectionProps & {
  mode: AppMode;
  runs: SoloRun[];
  activeRunId: string | null;
  runsLoading: boolean;
  onModeChange: (mode: AppMode) => void;
  onNewSolo: () => void;
  onSelectRun: (threadId: string) => void;
};

export function SoloSidebar({
  mode,
  runs,
  activeRunId,
  runsLoading,
  onModeChange,
  workspaces,
  activeWorkspace,
  onSelectWorkspace,
  onNewSolo,
  onSelectRun,
}: SoloSidebarProps) {
  return (
    <aside className="solo-sidebar">
      <div className="solo-sidebar-drag-strip" />
      <div className="solo-sidebar-header">
        <ModeSwitcher mode={mode} onModeChange={onModeChange} />
      </div>
      <section className="solo-workspace-section" aria-label="Workspace">
        <div className="solo-section-label">Workspace</div>
        {workspaces.length > 0 ? (
          <select
            className="solo-workspace-select"
            value={activeWorkspace?.id ?? ""}
            onChange={(event) => onSelectWorkspace(event.target.value)}
            aria-label="Select workspace"
          >
            {!activeWorkspace && <option value="">Select workspace</option>}
            {workspaces.map((workspace) => (
              <option key={workspace.id} value={workspace.id}>
                {workspace.name}
              </option>
            ))}
          </select>
        ) : (
          <div className="solo-workspace-empty">No workspace selected</div>
        )}
        {activeWorkspace ? (
          <div className="solo-workspace-path" title={activeWorkspace.path}>
            {activeWorkspace.path}
          </div>
        ) : null}
      </section>
      <button
        className="solo-new-button"
        type="button"
        onClick={onNewSolo}
        disabled={!activeWorkspace}
      >
        <Plus aria-hidden />
        <span>New Solo</span>
      </button>
      <section className="solo-runs-section" aria-label="Solo runs">
        <div className="solo-section-label">Solo Runs</div>
        <SoloRunList
          runs={runs}
          activeRunId={activeRunId}
          isLoading={runsLoading}
          onSelectRun={onSelectRun}
        />
      </section>
    </aside>
  );
}
