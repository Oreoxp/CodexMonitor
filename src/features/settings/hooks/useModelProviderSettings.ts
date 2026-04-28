import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  fetchModelsFromBaseUrl,
  getModelProviderSettings,
  updateModelProviderSettings,
  type FetchedModel,
  type ModelProviderSettings,
} from "@services/tauri";

const EMPTY_DRAFT: ModelProviderSettings = {
  model: null,
  providerKey: null,
  providerName: null,
  baseUrl: null,
  apiKey: null,
  wireApi: null,
  configPath: null,
};

const DEFAULT_PROVIDER_KEY = "opencrab";
const DEFAULT_WIRE_API = "chat";

const trimmed = (value: string | null | undefined): string | null => {
  if (value == null) {
    return null;
  }
  const next = value.trim();
  return next.length > 0 ? next : null;
};

const settingsEqual = (
  a: ModelProviderSettings,
  b: ModelProviderSettings,
): boolean =>
  trimmed(a.model) === trimmed(b.model) &&
  trimmed(a.providerKey) === trimmed(b.providerKey) &&
  trimmed(a.providerName) === trimmed(b.providerName) &&
  trimmed(a.baseUrl) === trimmed(b.baseUrl) &&
  trimmed(a.apiKey) === trimmed(b.apiKey) &&
  trimmed(a.wireApi) === trimmed(b.wireApi);

export type ModelProviderSettingsHook = {
  /** Saved (server-side) settings — empty drafts when not yet loaded. */
  saved: ModelProviderSettings;
  /** Editable draft mirrored to the form. */
  draft: ModelProviderSettings;
  isLoading: boolean;
  isSaving: boolean;
  isDirty: boolean;
  loadError: string | null;
  saveError: string | null;
  /** Models returned by the most recent base-URL probe. */
  fetchedModels: FetchedModel[];
  fetchedEndpoint: string | null;
  isFetchingModels: boolean;
  fetchError: string | null;
  fetchSuccessMessage: string | null;
  setField: <K extends keyof ModelProviderSettings>(
    key: K,
    value: ModelProviderSettings[K],
  ) => void;
  refresh: () => Promise<void>;
  save: () => Promise<void>;
  fetchModels: (apiKey?: string | null) => Promise<void>;
};

/**
 * Manages the structured `[model_providers.<key>]` + top-level `model` settings
 * stored in `~/.opencrab/config.toml`. The draft state mirrors the form so the
 * user can edit fields, refresh the model list against a base URL, and persist
 * everything in one save.
 */
export function useModelProviderSettings(): ModelProviderSettingsHook {
  const [saved, setSaved] = useState<ModelProviderSettings>(EMPTY_DRAFT);
  const [draft, setDraft] = useState<ModelProviderSettings>(EMPTY_DRAFT);
  const [isLoading, setIsLoading] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [fetchedModels, setFetchedModels] = useState<FetchedModel[]>([]);
  const [fetchedEndpoint, setFetchedEndpoint] = useState<string | null>(null);
  const [isFetchingModels, setIsFetchingModels] = useState(false);
  const [fetchError, setFetchError] = useState<string | null>(null);
  const [fetchSuccessMessage, setFetchSuccessMessage] = useState<string | null>(
    null,
  );

  const requestIdRef = useRef(0);

  const refresh = useCallback(async () => {
    requestIdRef.current += 1;
    const requestId = requestIdRef.current;
    setIsLoading(true);
    setLoadError(null);
    try {
      const result = await getModelProviderSettings();
      if (requestId !== requestIdRef.current) {
        return;
      }
      // Apply sane defaults so the form is always editable.
      const normalized: ModelProviderSettings = {
        ...result,
        providerKey: result.providerKey ?? DEFAULT_PROVIDER_KEY,
        wireApi: result.wireApi ?? DEFAULT_WIRE_API,
      };
      setSaved(normalized);
      setDraft(normalized);
    } catch (error) {
      if (requestId !== requestIdRef.current) {
        return;
      }
      setLoadError(error instanceof Error ? error.message : String(error));
    } finally {
      if (requestId === requestIdRef.current) {
        setIsLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const setField = useCallback<ModelProviderSettingsHook["setField"]>(
    (key, value) => {
      setDraft((prev) => ({ ...prev, [key]: value }));
    },
    [],
  );

  const save = useCallback(async () => {
    setIsSaving(true);
    setSaveError(null);
    try {
      const updated = await updateModelProviderSettings(draft);
      const normalized: ModelProviderSettings = {
        ...updated,
        providerKey: updated.providerKey ?? DEFAULT_PROVIDER_KEY,
        wireApi: updated.wireApi ?? DEFAULT_WIRE_API,
      };
      setSaved(normalized);
      setDraft(normalized);
    } catch (error) {
      setSaveError(error instanceof Error ? error.message : String(error));
    } finally {
      setIsSaving(false);
    }
  }, [draft]);

  const fetchModels = useCallback(
    async (apiKeyOverride?: string | null) => {
      const baseUrl = trimmed(draft.baseUrl);
      if (!baseUrl) {
        setFetchError("Base URL is empty");
        return;
      }
      // Default to whatever's currently in the draft so users don't have to
      // save before refreshing.
      const apiKey =
        apiKeyOverride !== undefined ? apiKeyOverride : trimmed(draft.apiKey);
      setIsFetchingModels(true);
      setFetchError(null);
      setFetchSuccessMessage(null);
      try {
        const response = await fetchModelsFromBaseUrl(baseUrl, apiKey ?? null);
        setFetchedModels(response.models);
        setFetchedEndpoint(response.endpoint);
        setFetchSuccessMessage(response.endpoint);
      } catch (error) {
        setFetchError(error instanceof Error ? error.message : String(error));
        setFetchedModels([]);
      } finally {
        setIsFetchingModels(false);
      }
    },
    [draft.baseUrl, draft.apiKey],
  );

  const isDirty = useMemo(() => !settingsEqual(saved, draft), [saved, draft]);

  return {
    saved,
    draft,
    isLoading,
    isSaving,
    isDirty,
    loadError,
    saveError,
    fetchedModels,
    fetchedEndpoint,
    isFetchingModels,
    fetchError,
    fetchSuccessMessage,
    setField,
    refresh,
    save,
    fetchModels,
  };
}
