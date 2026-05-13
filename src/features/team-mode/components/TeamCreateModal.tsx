import { useEffect, useState } from "react";
import { ModalShell } from "../../design-system/components/modal/ModalShell";
import { createTeamFromTemplate, listTemplates } from "../../../services/tauri";
import type { TeamConfig, TemplateInfo } from "../types";

type TeamCreateModalProps = {
  workspaceId: string;
  onClose: () => void;
  onCreated: (team: TeamConfig) => void;
};

export function TeamCreateModal({ workspaceId, onClose, onCreated }: TeamCreateModalProps) {
  const [templates, setTemplates] = useState<TemplateInfo[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [creatingId, setCreatingId] = useState<string | null>(null);
  const [createError, setCreateError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    listTemplates()
      .then((result) => {
        if (!cancelled) setTemplates(result);
      })
      .catch((err: unknown) => {
        if (!cancelled) setLoadError(String(err));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const handlePick = async (templateId: string) => {
    setCreateError(null);
    setCreatingId(templateId);
    try {
      const team = await createTeamFromTemplate(workspaceId, templateId);
      onCreated(team);
      onClose();
    } catch (err) {
      setCreateError(String(err));
      setCreatingId(null);
    }
  };

  return (
    <ModalShell
      ariaLabel="Create team from template"
      onBackdropClick={creatingId ? undefined : onClose}
      cardClassName="team-mode-create-card"
    >
      <header className="team-mode-create-header">
        <h2>Create team</h2>
        <p className="team-mode-create-subtitle">Pick a template to start with.</p>
      </header>

      <div className="team-mode-create-body">
        {loadError ? (
          <div className="team-mode-create-error">Failed to load templates: {loadError}</div>
        ) : templates === null ? (
          <div className="team-mode-create-loading">Loading…</div>
        ) : (
          <ul className="team-mode-template-list">
            {templates.map((tmpl) => (
              <li key={tmpl.id}>
                <button
                  type="button"
                  className="team-mode-template-row"
                  onClick={() => handlePick(tmpl.id)}
                  disabled={creatingId !== null}
                >
                  <span className="team-mode-template-name">{tmpl.displayName}</span>
                  <span className="team-mode-template-desc">{tmpl.description}</span>
                  {creatingId === tmpl.id ? (
                    <span className="team-mode-template-status">Creating…</span>
                  ) : null}
                </button>
              </li>
            ))}
          </ul>
        )}
        {createError ? (
          <div className="team-mode-create-error">{createError}</div>
        ) : null}
      </div>

      <footer className="team-mode-create-footer">
        <button
          type="button"
          className="team-mode-button"
          onClick={onClose}
          disabled={creatingId !== null}
        >
          Cancel
        </button>
      </footer>
    </ModalShell>
  );
}
