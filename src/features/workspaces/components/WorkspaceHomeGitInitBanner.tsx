import { useTranslation } from "react-i18next";

type WorkspaceHomeGitInitBannerProps = {
  isLoading: boolean;
  onInitGitRepo: () => void | Promise<void>;
};

export function WorkspaceHomeGitInitBanner({
  isLoading,
  onInitGitRepo,
}: WorkspaceHomeGitInitBannerProps) {
  const { t } = useTranslation();
  return (
    <div className="workspace-home-git-banner" role="region" aria-label={t('workspaceHome.gitSetup')}>
      <div className="workspace-home-git-banner-title">
        {t('workspaceHome.gitNotInitialized')}
      </div>
      <div className="workspace-home-git-banner-actions">
        <button
          type="button"
          className="primary"
          onClick={() => void onInitGitRepo()}
          disabled={isLoading}
        >
          {isLoading ? t('workspaceHome.initializingGit') : t('workspaceHome.initializeGit')}
        </button>
      </div>
    </div>
  );
}

