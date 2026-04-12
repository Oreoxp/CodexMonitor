import { useCallback, useRef } from "react";
import { useAgentSoundNotifications } from "../../notifications/hooks/useAgentSoundNotifications";
import { useAgentSystemNotifications } from "../../notifications/hooks/useAgentSystemNotifications";
import { useWindowFocusState } from "../../layout/hooks/useWindowFocusState";
import { playNotificationSound } from "../../../utils/notificationSounds";
import { sendNotification } from "../../../services/tauri";
import type { DebugEntry } from "../../../types";
import type { UpdateState, PostUpdateNoticeState } from "../../update/hooks/useUpdater";

const NOOP_STATE: UpdateState = { stage: "idle" };

type Params = {
  enabled?: boolean;
  autoCheckOnMount?: boolean;
  notificationSoundsEnabled: boolean;
  systemNotificationsEnabled: boolean;
  subagentSystemNotificationsEnabled: boolean;
  isSubagentThread?: (workspaceId: string, threadId: string) => boolean;
  getWorkspaceName?: (workspaceId: string) => string | undefined;
  onThreadNotificationSent?: (workspaceId: string, threadId: string) => void;
  onDebug: (entry: DebugEntry) => void;
  successSoundUrl: string;
  errorSoundUrl: string;
};

export function useUpdaterController({
  notificationSoundsEnabled,
  systemNotificationsEnabled,
  subagentSystemNotificationsEnabled,
  isSubagentThread,
  getWorkspaceName,
  onThreadNotificationSent,
  onDebug,
  successSoundUrl,
  errorSoundUrl,
}: Params) {
  const isWindowFocused = useWindowFocusState();
  const nextTestSoundIsError = useRef(false);

  useAgentSoundNotifications({
    enabled: notificationSoundsEnabled,
    isWindowFocused,
    onDebug,
  });

  useAgentSystemNotifications({
    enabled: systemNotificationsEnabled,
    subagentNotificationsEnabled: subagentSystemNotificationsEnabled,
    isSubagentThread,
    isWindowFocused,
    getWorkspaceName,
    onThreadNotificationSent,
    onDebug,
  });

  const handleTestNotificationSound = useCallback(() => {
    const useError = nextTestSoundIsError.current;
    nextTestSoundIsError.current = !useError;
    const type = useError ? "error" : "success";
    const url = useError ? errorSoundUrl : successSoundUrl;
    playNotificationSound(url, type, onDebug);
  }, [errorSoundUrl, onDebug, successSoundUrl]);

  const handleTestSystemNotification = useCallback(() => {
    if (!systemNotificationsEnabled) {
      return;
    }
    void sendNotification(
      "Test Notification",
      "This is a test notification from 小螃蟹.",
    ).catch((error) => {
      onDebug({
        id: `${Date.now()}-client-notification-test-error`,
        timestamp: Date.now(),
        source: "error",
        label: "notification/test-error",
        payload: error instanceof Error ? error.message : String(error),
      });
    });
  }, [onDebug, systemNotificationsEnabled]);

  const noop = useCallback(async () => {}, []);

  return {
    updaterState: NOOP_STATE,
    startUpdate: noop,
    checkForUpdates: noop,
    dismissUpdate: noop,
    postUpdateNotice: null as PostUpdateNoticeState,
    dismissPostUpdateNotice: noop,
    handleTestNotificationSound,
    handleTestSystemNotification,
  };
}
