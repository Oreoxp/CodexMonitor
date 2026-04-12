import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { ModalShell } from "../../design-system/components/modal/ModalShell";

type WorkspaceFromUrlPromptProps = {
  url: string;
  destinationPath: string;
  targetFolderName: string;
  error: string | null;
  isBusy: boolean;
  canSubmit: boolean;
  onUrlChange: (value: string) => void;
  onTargetFolderNameChange: (value: string) => void;
  onChooseDestinationPath: () => void;
  onClearDestinationPath: () => void;
  onCancel: () => void;
  onConfirm: () => void;
};

export function WorkspaceFromUrlPrompt({
  url,
  destinationPath,
  targetFolderName,
  error,
  isBusy,
  canSubmit,
  onUrlChange,
  onTargetFolderNameChange,
  onChooseDestinationPath,
  onClearDestinationPath,
  onCancel,
  onConfirm,
}: WorkspaceFromUrlPromptProps) {
  const { t } = useTranslation();
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  return (
    <ModalShell
      ariaLabel={t('workspaceFromUrl.title')}
      className="workspace-from-url-modal"
      cardClassName="workspace-from-url-modal-card"
      onBackdropClick={() => {
        if (!isBusy) {
          onCancel();
        }
      }}
    >
      <div className="workspace-from-url-modal-content">
        <div className="ds-modal-title">{t('workspaceFromUrl.title')}</div>
        <label className="ds-modal-label" htmlFor="workspace-url-input">
          {t('workspaceFromUrl.urlLabel')}
        </label>
        <input
          id="workspace-url-input"
          ref={inputRef}
          className="ds-modal-input"
          value={url}
          onChange={(event) => onUrlChange(event.target.value)}
          placeholder="https://github.com/org/repo.git"
        />
        <label className="ds-modal-label" htmlFor="workspace-url-target-name">
          {t('workspaceFromUrl.folderNameLabel')}
        </label>
        <input
          id="workspace-url-target-name"
          className="ds-modal-input"
          value={targetFolderName}
          onChange={(event) => onTargetFolderNameChange(event.target.value)}
          placeholder={t('workspaceFromUrl.folderNamePlaceholder')}
        />
        <label className="ds-modal-label" htmlFor="workspace-url-destination">
          {t('workspaceFromUrl.destinationLabel')}
        </label>
        <div style={{ display: "flex", gap: 8 }}>
          <textarea
            id="workspace-url-destination"
            className="ds-modal-input"
            value={destinationPath}
            placeholder={t('workspaceFromUrl.destinationPlaceholder')}
            readOnly
            rows={1}
            wrap="off"
          />
          <button type="button" className="ghost ds-modal-button" onClick={onChooseDestinationPath}>
            {t('workspaceFromUrl.choose')}
          </button>
          <button
            type="button"
            className="ghost ds-modal-button"
            onClick={onClearDestinationPath}
            disabled={destinationPath.trim().length === 0 || isBusy}
          >
            {t('workspaceFromUrl.clear')}
          </button>
        </div>
        {error && <div className="ds-modal-error">{error}</div>}
        <div className="ds-modal-actions">
          <button className="ghost ds-modal-button" onClick={onCancel} disabled={isBusy}>
            {t('workspaceFromUrl.cancel')}
          </button>
          <button
            className="primary ds-modal-button"
            onClick={onConfirm}
            disabled={isBusy || !canSubmit}
          >
            {isBusy ? t('workspaceFromUrl.cloning') : t('workspaceFromUrl.cloneAndAdd')}
          </button>
        </div>
      </div>
    </ModalShell>
  );
}
