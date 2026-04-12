export const STORAGE_KEY_PENDING_POST_UPDATE_VERSION =
  "opencrab.pendingPostUpdateVersion";

export type PostUpdateReleaseInfo = {
  body: string | null;
  htmlUrl: string;
  tag: string | null;
};

function normalizeStoredVersion(value: string): string {
  let normalized = value.trim();
  while (normalized.startsWith("v") || normalized.startsWith("V")) {
    normalized = normalized.slice(1);
  }
  return normalized.trim();
}

export function normalizeReleaseVersion(value: string): string {
  return normalizeStoredVersion(value);
}

export function buildReleaseTagUrl(_version: string): string {
  return "";
}

export function savePendingPostUpdateVersion(version: string): void {
  if (typeof window === "undefined") {
    return;
  }
  const normalized = normalizeStoredVersion(version);
  if (!normalized) {
    return;
  }
  try {
    window.localStorage.setItem(
      STORAGE_KEY_PENDING_POST_UPDATE_VERSION,
      normalized,
    );
  } catch {
    // Best-effort persistence.
  }
}

export function loadPendingPostUpdateVersion(): string | null {
  if (typeof window === "undefined") {
    return null;
  }
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY_PENDING_POST_UPDATE_VERSION);
    if (!raw) {
      return null;
    }
    const normalized = normalizeStoredVersion(raw);
    return normalized || null;
  } catch {
    return null;
  }
}

export function clearPendingPostUpdateVersion(): void {
  if (typeof window === "undefined") {
    return;
  }
  try {
    window.localStorage.removeItem(STORAGE_KEY_PENDING_POST_UPDATE_VERSION);
  } catch {
    // Best-effort persistence.
  }
}

export async function fetchReleaseNotesForVersion(
  _version: string,
): Promise<PostUpdateReleaseInfo> {
  return { body: null, htmlUrl: "", tag: null };
}
