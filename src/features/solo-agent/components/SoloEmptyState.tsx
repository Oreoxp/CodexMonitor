type SoloEmptyStateProps = {
  hasWorkspace: boolean;
};

export function SoloEmptyState({ hasWorkspace }: SoloEmptyStateProps) {
  return (
    <div className="solo-empty-state">
      <div className="solo-empty-state-kicker">Solo Agent</div>
      <h1>{hasWorkspace ? "Create a New Solo run" : "Select a workspace"}</h1>
      <p>
        {hasWorkspace
          ? "Choose New Solo when you are ready to start a task flow."
          : "Solo runs use the same workspace/project source as Code mode."}
      </p>
    </div>
  );
}
