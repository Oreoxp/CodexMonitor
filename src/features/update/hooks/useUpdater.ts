import { useCallback } from "react";

type UpdateStage =
  | "idle"
  | "checking"
  | "available"
  | "downloading"
  | "installing"
  | "restarting"
  | "latest"
  | "error";

type UpdateProgress = {
  totalBytes?: number;
  downloadedBytes: number;
};

export type UpdateState = {
  stage: UpdateStage;
  version?: string;
  progress?: UpdateProgress;
  error?: string;
};

export type PostUpdateNoticeState = {
  stage: "loading" | "ready" | "fallback";
  version: string;
  htmlUrl: string;
  body?: string;
} | null;

const NOOP_STATE: UpdateState = { stage: "idle" };

type UseUpdaterOptions = {
  enabled?: boolean;
  autoCheckOnMount?: boolean;
  onDebug?: unknown;
};

export function useUpdater(_options?: UseUpdaterOptions) {
  const noop = useCallback(async (_opts?: { announceNoUpdate?: boolean }) => {}, []);
  const noopVoid = useCallback(async () => {}, []);
  return {
    state: NOOP_STATE,
    startUpdate: noopVoid,
    checkForUpdates: noop,
    dismiss: noopVoid,
    postUpdateNotice: null as PostUpdateNoticeState,
    dismissPostUpdateNotice: noopVoid,
  };
}
