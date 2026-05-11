import type { FormEvent } from "react";
import type { WorkspaceInfo } from "@/types";

type SoloNewRunFormProps = {
  workspace: WorkspaceInfo;
  goal: string;
  isSubmitting: boolean;
  error: string | null;
  onGoalChange: (goal: string) => void;
  onStart: () => void;
  onCancel: () => void;
};

export function SoloNewRunForm({
  workspace,
  goal,
  isSubmitting,
  error,
  onGoalChange,
  onStart,
  onCancel,
}: SoloNewRunFormProps) {
  const handleSubmit = (event: FormEvent) => {
    event.preventDefault();
    onStart();
  };

  return (
    <form className="solo-new-run-form" onSubmit={handleSubmit}>
      <div className="solo-form-header">
        <div className="solo-empty-state-kicker">New Solo</div>
        <h1>Describe the task</h1>
      </div>
      <div className="solo-form-workspace">
        <div className="solo-section-label">Workspace</div>
        <div className="solo-form-workspace-name">{workspace.name}</div>
        <div className="solo-form-workspace-path" title={workspace.path}>
          {workspace.path}
        </div>
      </div>
      <label className="solo-goal-label" htmlFor="solo-goal">
        Goal
      </label>
      <textarea
        id="solo-goal"
        className="solo-goal-textarea"
        value={goal}
        onChange={(event) => onGoalChange(event.target.value)}
        placeholder="What should the Solo Agent accomplish?"
        rows={8}
        disabled={isSubmitting}
        autoFocus
      />
      {error ? <div className="solo-form-error">{error}</div> : null}
      <div className="solo-form-actions">
        <button
          className="secondary"
          type="button"
          onClick={onCancel}
          disabled={isSubmitting}
        >
          Cancel
        </button>
        <button className="primary" type="submit" disabled={isSubmitting || !goal.trim()}>
          {isSubmitting ? "Starting..." : "Start Solo"}
        </button>
      </div>
    </form>
  );
}
