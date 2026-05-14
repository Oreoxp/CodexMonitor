// TODO(post-1.1): upstream this into an injectable sendTransport in
// useThreadMessaging once dev/qa agents arrive. See
// docs/scratch/investigation-team-mode-chat-reuse.md decision B1.
//
// B1 parallel send path for Team Mode. Reuses the entire normal-mode chat
// rendering stack — the user message item, processing spinner, streaming
// deltas and turn completion all arrive via `app-server-event` (routed by
// threadId into the shared reducer), exactly as in normal mode — so this path
// does NOT re-implement that bookkeeping. It only swaps the *transport*: the
// user's turn goes through `sidecar_pm_say` (so the LangGraph sidecar sees it)
// instead of going direct to Codex `turn/start`.

import { useCallback } from "react";
import { sidecarPmSay } from "@services/tauri";
import type {
  AppMention,
  ComposerSendIntent,
  DebugEntry,
  SendMessageResult,
  WorkspaceInfo,
} from "@/types";

type UseTeamModeMessagingOptions = {
  activeWorkspaceId: string | null;
  // The active agent's Codex thread id, resolved by useActiveAgentThreadId.
  // Pre-bootstrapped (decision A1), so it should be set before the user can
  // send. If it is still null at send time we fail loudly rather than minting
  // a normal Codex thread — Team Mode must never bypass the sidecar.
  activeAgentThreadId: string | null;
  onDebug?: (entry: DebugEntry) => void;
};

type TeamModeMessaging = {
  sendUserMessage: (
    text: string,
    images?: string[],
    appMentions?: AppMention[],
    options?: { sendIntent?: ComposerSendIntent },
  ) => Promise<SendMessageResult>;
  sendUserMessageToThread: (
    workspace: WorkspaceInfo,
    threadId: string,
    text: string,
    images?: string[],
  ) => Promise<void | SendMessageResult>;
};

export function useTeamModeMessaging({
  activeWorkspaceId,
  activeAgentThreadId,
  onDebug,
}: UseTeamModeMessagingOptions): TeamModeMessaging {
  const send = useCallback(
    async (
      workspaceId: string,
      threadId: string | null,
      text: string,
    ): Promise<SendMessageResult> => {
      const messageText = text.trim();
      if (!messageText) {
        return { status: "blocked" };
      }
      if (!threadId) {
        const message =
          "Team Mode: no active agent thread yet — agent_ensure_thread has " +
          "not resolved. Message not sent.";
        onDebug?.({
          id: `${Date.now()}-team-send-no-thread`,
          timestamp: Date.now(),
          source: "error",
          label: "team-mode/send blocked",
          payload: message,
        });
        return { status: "blocked" };
      }
      onDebug?.({
        id: `${Date.now()}-team-pm-say`,
        timestamp: Date.now(),
        source: "client",
        label: "sidecar_pm_say",
        payload: { workspaceId, threadId, text: messageText },
      });
      try {
        await sidecarPmSay(workspaceId, messageText);
        return { status: "sent" };
      } catch (err) {
        onDebug?.({
          id: `${Date.now()}-team-pm-say-error`,
          timestamp: Date.now(),
          source: "error",
          label: "sidecar_pm_say error",
          payload: String(err),
        });
        return { status: "blocked" };
      }
    },
    [onDebug],
  );

  const sendUserMessage = useCallback(
    async (
      text: string,
      _images: string[] = [],
      _appMentions: AppMention[] = [],
      _options?: { sendIntent?: ComposerSendIntent },
    ): Promise<SendMessageResult> => {
      if (!activeWorkspaceId) {
        return { status: "blocked" };
      }
      return send(activeWorkspaceId, activeAgentThreadId, text);
    },
    [activeWorkspaceId, activeAgentThreadId, send],
  );

  const sendUserMessageToThread = useCallback(
    async (
      workspace: WorkspaceInfo,
      threadId: string,
      text: string,
      _images: string[] = [],
    ): Promise<void | SendMessageResult> => {
      return send(workspace.id, threadId, text);
    },
    [send],
  );

  return { sendUserMessage, sendUserMessageToThread };
}
