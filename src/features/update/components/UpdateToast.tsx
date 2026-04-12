import type { UpdateState, PostUpdateNoticeState } from "../hooks/useUpdater";

export type UpdateToastProps = {
  state: UpdateState;
  onUpdate: () => void;
  onDismiss: () => void;
  postUpdateNotice?: PostUpdateNoticeState;
  onDismissPostUpdateNotice?: () => void;
};

export function UpdateToast(_props: UpdateToastProps) {
  return null;
}
