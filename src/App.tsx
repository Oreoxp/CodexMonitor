import { lazy, Suspense } from "react";
import "./styles/base.css";
import "./styles/ds-tokens.css";
import "./styles/ds-modal.css";
import "./styles/ds-toast.css";
import "./styles/ds-panel.css";
import "./styles/ds-diff.css";
import "./styles/ds-popover.css";
import "./styles/ds-tooltip.css";
import "./styles/buttons.css";
import "./styles/sidebar.css";
import "./styles/home.css";
import "./styles/workspace-home.css";
import "./styles/main.css";
import "./styles/messages.css";
import "./styles/approval-toasts.css";
import "./styles/error-toasts.css";
import "./styles/request-user-input.css";
import "./styles/update-toasts.css";
import "./styles/composer.css";
import "./styles/review-inline.css";
import "./styles/diff.css";
import "./styles/diff-viewer.css";
import "./styles/file-tree.css";
import "./styles/panel-tabs.css";
import "./styles/prompts.css";
import "./styles/debug.css";
import "./styles/terminal.css";
import "./styles/plan.css";
import "./styles/about.css";
import "./styles/tabbar.css";
import "./styles/worktree-modal.css";
import "./styles/clone-modal.css";
import "./styles/workspace-from-url-modal.css";
import "./styles/mobile-remote-workspace-modal.css";
import "./styles/branch-switcher-modal.css";
import "./styles/git-init-modal.css";
import "./styles/settings.css";
import "./styles/compact-base.css";
import "./styles/compact-phone.css";
import "./styles/compact-tablet.css";
import "@/features/team-mode/team-mode.css";
import "@/features/team-mode/team-ui.css";
import { useWindowLabel } from "@/features/layout/hooks/useWindowLabel";
import MainApp from "@app/components/MainApp";
import TeamApp from "@/features/team-mode/components/TeamApp";
import {
  AppModeProvider,
  useAppMode,
} from "@/features/team-mode/context/AppModeContext";

const AboutView = lazy(() =>
  import("@/features/about/components/AboutView").then((module) => ({
    default: module.AboutView,
  })),
);

function AppRoot() {
  const windowLabel = useWindowLabel();
  const [mode] = useAppMode();

  if (windowLabel === "about") {
    return (
      <Suspense fallback={null}>
        <AboutView />
      </Suspense>
    );
  }

  return mode === "team" ? <TeamApp /> : <MainApp />;
}

export default function App() {
  return (
    <AppModeProvider>
      <AppRoot />
    </AppModeProvider>
  );
}
