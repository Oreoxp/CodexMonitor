import { useEffect, useMemo, useRef } from "react";
import { useTranslation } from "react-i18next";
import Stethoscope from "lucide-react/dist/esm/icons/stethoscope";
import type { Dispatch, SetStateAction } from "react";
import type {
  AppSettings,
  CodexDoctorResult,
  CodexUpdateResult,
  ModelOption,
} from "@/types";
import {
  SettingsSection,
  SettingsToggleRow,
} from "@/features/design-system/components/settings/SettingsPrimitives";
import { FileEditorCard } from "@/features/shared/components/FileEditorCard";
import { useModelProviderSettings } from "@settings/hooks/useModelProviderSettings";

type SettingsCodexSectionProps = {
  appSettings: AppSettings;
  onUpdateAppSettings: (next: AppSettings) => Promise<void>;
  defaultModels: ModelOption[];
  defaultModelsLoading: boolean;
  defaultModelsError: string | null;
  defaultModelsConnectedWorkspaceCount: number;
  onRefreshDefaultModels: () => void;
  codexPathDraft: string;
  codexArgsDraft: string;
  codexDirty: boolean;
  isSavingSettings: boolean;
  doctorState: {
    status: "idle" | "running" | "done";
    result: CodexDoctorResult | null;
  };
  codexUpdateState: {
    status: "idle" | "running" | "done";
    result: CodexUpdateResult | null;
  };
  globalAgentsMeta: string;
  globalAgentsError: string | null;
  globalAgentsContent: string;
  globalAgentsLoading: boolean;
  globalAgentsRefreshDisabled: boolean;
  globalAgentsSaveDisabled: boolean;
  globalAgentsSaveLabel: string;
  globalConfigMeta: string;
  globalConfigError: string | null;
  globalConfigContent: string;
  globalConfigLoading: boolean;
  globalConfigRefreshDisabled: boolean;
  globalConfigSaveDisabled: boolean;
  globalConfigSaveLabel: string;
  onSetCodexPathDraft: Dispatch<SetStateAction<string>>;
  onSetCodexArgsDraft: Dispatch<SetStateAction<string>>;
  onSetGlobalAgentsContent: (value: string) => void;
  onSetGlobalConfigContent: (value: string) => void;
  onBrowseCodex: () => Promise<void>;
  onSaveCodexSettings: () => Promise<void>;
  onRunDoctor: () => Promise<void>;
  onRunCodexUpdate: () => Promise<void>;
  onRefreshGlobalAgents: () => void;
  onSaveGlobalAgents: () => void;
  onRefreshGlobalConfig: () => void;
  onSaveGlobalConfig: () => void;
};

const DEFAULT_REASONING_EFFORT = "medium";

const normalizeEffortValue = (value: unknown): string | null => {
  if (typeof value !== "string") {
    return null;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed.toLowerCase() : null;
};

function coerceSavedModelSlug(value: string | null, models: ModelOption[]): string | null {
  const trimmed = (value ?? "").trim();
  if (!trimmed) {
    return null;
  }
  const bySlug = models.find((model) => model.model === trimmed);
  if (bySlug) {
    return bySlug.model;
  }
  const byId = models.find((model) => model.id === trimmed);
  return byId ? byId.model : null;
}

const getReasoningSupport = (model: ModelOption | null): boolean => {
  if (!model) {
    return false;
  }
  return model.supportedReasoningEfforts.length > 0 || model.defaultReasoningEffort !== null;
};

const getReasoningOptions = (model: ModelOption | null): string[] => {
  if (!model) {
    return [];
  }
  const supported = model.supportedReasoningEfforts
    .map((effort) => normalizeEffortValue(effort.reasoningEffort))
    .filter((effort): effort is string => Boolean(effort));
  if (supported.length > 0) {
    return Array.from(new Set(supported));
  }
  const fallback = normalizeEffortValue(model.defaultReasoningEffort);
  return fallback ? [fallback] : [];
};

export function SettingsCodexSection({
  appSettings,
  onUpdateAppSettings,
  defaultModels,
  defaultModelsLoading,
  defaultModelsError,
  defaultModelsConnectedWorkspaceCount,
  onRefreshDefaultModels,
  codexPathDraft,
  codexArgsDraft,
  codexDirty,
  isSavingSettings,
  doctorState,
  codexUpdateState,
  globalAgentsMeta,
  globalAgentsError,
  globalAgentsContent,
  globalAgentsLoading,
  globalAgentsRefreshDisabled,
  globalAgentsSaveDisabled,
  globalAgentsSaveLabel,
  globalConfigMeta,
  globalConfigError,
  globalConfigContent,
  globalConfigLoading,
  globalConfigRefreshDisabled,
  globalConfigSaveDisabled,
  globalConfigSaveLabel,
  onSetCodexPathDraft,
  onSetCodexArgsDraft,
  onSetGlobalAgentsContent,
  onSetGlobalConfigContent,
  onBrowseCodex,
  onSaveCodexSettings,
  onRunDoctor,
  onRunCodexUpdate,
  onRefreshGlobalAgents,
  onSaveGlobalAgents,
  onRefreshGlobalConfig,
  onSaveGlobalConfig,
}: SettingsCodexSectionProps) {
  const latestModelSlug = defaultModels[0]?.model ?? null;
  const savedModelSlug = useMemo(
    () => coerceSavedModelSlug(appSettings.lastComposerModelId, defaultModels),
    [appSettings.lastComposerModelId, defaultModels],
  );
  const selectedModelSlug = savedModelSlug ?? latestModelSlug ?? "";
  const selectedModel = useMemo(
    () => defaultModels.find((model) => model.model === selectedModelSlug) ?? null,
    [defaultModels, selectedModelSlug],
  );
  const reasoningSupported = useMemo(
    () => getReasoningSupport(selectedModel),
    [selectedModel],
  );
  const reasoningOptions = useMemo(
    () => getReasoningOptions(selectedModel),
    [selectedModel],
  );
  const savedEffort = useMemo(
    () => normalizeEffortValue(appSettings.lastComposerReasoningEffort),
    [appSettings.lastComposerReasoningEffort],
  );
  const selectedEffort = useMemo(() => {
    if (!reasoningSupported) {
      return "";
    }
    if (savedEffort && reasoningOptions.includes(savedEffort)) {
      return savedEffort;
    }
    if (reasoningOptions.includes(DEFAULT_REASONING_EFFORT)) {
      return DEFAULT_REASONING_EFFORT;
    }
    const fallback = normalizeEffortValue(selectedModel?.defaultReasoningEffort);
    if (fallback && reasoningOptions.includes(fallback)) {
      return fallback;
    }
    return reasoningOptions[0] ?? "";
  }, [reasoningOptions, reasoningSupported, savedEffort, selectedModel]);

  const didNormalizeDefaultsRef = useRef(false);
  const { t } = useTranslation();

  const providerSettings = useModelProviderSettings();
  useEffect(() => {
    if (didNormalizeDefaultsRef.current) {
      return;
    }
    if (!defaultModels.length) {
      return;
    }
    const savedRawModel = (appSettings.lastComposerModelId ?? "").trim();
    const savedRawEffort = (appSettings.lastComposerReasoningEffort ?? "").trim();
    const shouldNormalizeModel = savedRawModel.length === 0 || savedModelSlug === null;
    const shouldNormalizeEffort =
      reasoningSupported &&
      (savedRawEffort.length === 0 ||
        savedEffort === null ||
        !reasoningOptions.includes(savedEffort));
    if (!shouldNormalizeModel && !shouldNormalizeEffort) {
      didNormalizeDefaultsRef.current = true;
      return;
    }

    const next: AppSettings = {
      ...appSettings,
      lastComposerModelId: shouldNormalizeModel ? selectedModelSlug : appSettings.lastComposerModelId,
      lastComposerReasoningEffort: shouldNormalizeEffort
        ? selectedEffort
        : appSettings.lastComposerReasoningEffort,
    };
    didNormalizeDefaultsRef.current = true;
    void onUpdateAppSettings(next);
  }, [
    appSettings,
    defaultModels.length,
    onUpdateAppSettings,
    reasoningOptions,
    reasoningSupported,
    savedEffort,
    savedModelSlug,
    selectedModelSlug,
    selectedEffort,
  ]);

  return (
    <SettingsSection
      title={t("settings.codex.title")}
      subtitle={t("settings.codex.subtitle")}
    >
      <div className="settings-field">
        <label className="settings-field-label" htmlFor="codex-path">
          {t("settings.codex.defaultCodexPath")}
        </label>
        <div className="settings-field-row">
          <input
            id="codex-path"
            className="settings-input"
            value={codexPathDraft}
            placeholder="codex"
            onChange={(event) => onSetCodexPathDraft(event.target.value)}
          />
          <button
            type="button"
            className="ghost"
            onClick={() => {
              void onBrowseCodex();
            }}
          >
            {t("settings.codex.browse")}
          </button>
          <button
            type="button"
            className="ghost"
            onClick={() => onSetCodexPathDraft("")}
          >
            {t("settings.codex.usePath")}
          </button>
        </div>
        <div className="settings-help">{t("settings.codex.pathHelp")}</div>
        <label className="settings-field-label" htmlFor="codex-args">
          {t("settings.codex.defaultCodexArgs")}
        </label>
        <div className="settings-field-row">
          <input
            id="codex-args"
            className="settings-input"
            value={codexArgsDraft}
            placeholder="--profile personal"
            onChange={(event) => onSetCodexArgsDraft(event.target.value)}
          />
          <button
            type="button"
            className="ghost"
            onClick={() => onSetCodexArgsDraft("")}
          >
            {t("settings.codex.clear")}
          </button>
        </div>
        <div className="settings-help">
          {t("settings.codex.argsHelp")} <code>app-server</code>. {t("settings.codex.argsHelpQuotes")}
        </div>
        <div className="settings-help">
          {t("settings.codex.sharedSettings")}
        </div>
        <div className="settings-help">
          {t("settings.codex.perThreadOverride")} <code>-m</code>/
          <code>--model</code>, <code>-a</code>/<code>--ask-for-approval</code>,{" "}
          <code>-s</code>/<code>--sandbox</code>, <code>--full-auto</code>,{" "}
          <code>--dangerously-bypass-approvals-and-sandbox</code>, <code>--oss</code>,{" "}
          <code>--local-provider</code>, {t("settings.codex.and")} <code>--no-alt-screen</code>.
        </div>
        <div className="settings-field-actions">
          {codexDirty && (
            <button
              type="button"
              className="primary"
              onClick={() => {
                void onSaveCodexSettings();
              }}
              disabled={isSavingSettings}
            >
              {isSavingSettings ? t("settings.codex.saving") : t("settings.codex.save")}
            </button>
          )}
          <button
            type="button"
            className="ghost settings-button-compact"
            onClick={() => {
              void onRunDoctor();
            }}
            disabled={doctorState.status === "running"}
          >
            <Stethoscope aria-hidden />
            {doctorState.status === "running" ? t("settings.codex.running") : t("settings.codex.runDoctor")}
          </button>
          <button
            type="button"
            className="ghost settings-button-compact"
            onClick={() => {
              void onRunCodexUpdate();
            }}
            disabled={codexUpdateState.status === "running"}
            title={t("settings.codex.updateCodex")}
          >
            <Stethoscope aria-hidden />
            {codexUpdateState.status === "running" ? t("settings.codex.updating") : t("settings.codex.update")}
          </button>
        </div>

        {doctorState.result && (
          <div className={`settings-doctor ${doctorState.result.ok ? "ok" : "error"}`}>
            <div className="settings-doctor-title">
              {doctorState.result.ok ? t("settings.codex.codexLooksGood") : t("settings.codex.codexIssueDetected")}
            </div>
            <div className="settings-doctor-body">
              <div>{t("settings.codex.doctorVersion")}: {doctorState.result.version ?? t("settings.codex.unknown")}</div>
              <div>{t("settings.codex.appServer")}: {doctorState.result.appServerOk ? t("settings.codex.ok") : t("settings.codex.failed")}</div>
              <div>
                {t("settings.codex.node")}:{" "}
                {doctorState.result.nodeOk
                  ? `${t("settings.codex.ok")} (${doctorState.result.nodeVersion ?? t("settings.codex.unknown")})`
                  : t("settings.codex.missing")}
              </div>
              {doctorState.result.details && <div>{doctorState.result.details}</div>}
              {doctorState.result.nodeDetails && <div>{doctorState.result.nodeDetails}</div>}
              {doctorState.result.path && (
                <div className="settings-doctor-path">PATH: {doctorState.result.path}</div>
              )}
            </div>
          </div>
        )}

        {codexUpdateState.result && (
          <div
            className={`settings-doctor ${codexUpdateState.result.ok ? "ok" : "error"}`}
          >
            <div className="settings-doctor-title">
              {codexUpdateState.result.ok
                ? codexUpdateState.result.upgraded
                  ? t("settings.codex.codexUpdated")
                  : t("settings.codex.codexAlreadyUpToDate")
                : t("settings.codex.codexUpdateFailed")}
            </div>
            <div className="settings-doctor-body">
              <div>{t("settings.codex.method")}: {codexUpdateState.result.method}</div>
              {codexUpdateState.result.package && (
                <div>{t("settings.codex.package")}: {codexUpdateState.result.package}</div>
              )}
              <div>
                {t("settings.codex.doctorVersion")}:{" "}
                {codexUpdateState.result.afterVersion ??
                  codexUpdateState.result.beforeVersion ??
                  t("settings.codex.unknown")}
              </div>
              {codexUpdateState.result.details && <div>{codexUpdateState.result.details}</div>}
              {codexUpdateState.result.output && (
                <details>
                  <summary>{t("settings.codex.output")}</summary>
                  <pre>{codexUpdateState.result.output}</pre>
                </details>
              )}
            </div>
          </div>
        )}
      </div>

      <div className="settings-divider" />
      <div className="settings-field-label settings-field-label--section">
        {t("settings.codex.provider.sectionTitle")}
      </div>
      <div className="settings-help">
        {t("settings.codex.provider.sectionSubtitle")}
      </div>
      {providerSettings.loadError && (
        <div className="settings-agents-error">
          {t("settings.codex.provider.loadError")}: {providerSettings.loadError}
        </div>
      )}

      <div className="settings-field">
        <label className="settings-field-label" htmlFor="provider-key">
          {t("settings.codex.provider.providerKeyLabel")}
        </label>
        <input
          id="provider-key"
          className="settings-input"
          value={providerSettings.draft.providerKey ?? ""}
          placeholder="opencrab"
          disabled={providerSettings.isLoading}
          onChange={(event) =>
            providerSettings.setField("providerKey", event.target.value)
          }
        />
        <div className="settings-help">
          {t("settings.codex.provider.providerKeyHelp")}
        </div>

        <label className="settings-field-label" htmlFor="provider-name">
          {t("settings.codex.provider.providerNameLabel")}
        </label>
        <input
          id="provider-name"
          className="settings-input"
          value={providerSettings.draft.providerName ?? ""}
          placeholder={t("settings.codex.provider.providerNamePlaceholder")}
          disabled={providerSettings.isLoading}
          onChange={(event) =>
            providerSettings.setField("providerName", event.target.value)
          }
        />

        <label className="settings-field-label" htmlFor="provider-base-url">
          {t("settings.codex.provider.baseUrlLabel")}
        </label>
        <div className="settings-field-row">
          <input
            id="provider-base-url"
            className="settings-input"
            value={providerSettings.draft.baseUrl ?? ""}
            placeholder={t("settings.codex.provider.baseUrlPlaceholder")}
            disabled={providerSettings.isLoading}
            onChange={(event) =>
              providerSettings.setField("baseUrl", event.target.value)
            }
          />
          <button
            type="button"
            className="ghost"
            disabled={
              providerSettings.isFetchingModels ||
              !((providerSettings.draft.baseUrl ?? "").trim())
            }
            onClick={() => {
              void providerSettings.fetchModels();
            }}
          >
            {providerSettings.isFetchingModels
              ? t("settings.codex.provider.refreshing")
              : t("settings.codex.provider.refreshFromBaseUrl")}
          </button>
        </div>
        <div className="settings-help">
          {t("settings.codex.provider.baseUrlHelp", {
            endpoint:
              providerSettings.fetchedEndpoint ??
              `${(providerSettings.draft.baseUrl ?? "").replace(/\/+$/, "") || "BASE_URL"}/models`,
          })}
        </div>
        {providerSettings.fetchError && (
          <div className="settings-agents-error">
            {t("settings.codex.provider.refreshError")}: {providerSettings.fetchError}
          </div>
        )}
        {providerSettings.fetchSuccessMessage && !providerSettings.fetchError && (
          <div className="settings-help">
            {t("settings.codex.provider.refreshSuccess", {
              count: providerSettings.fetchedModels.length,
              endpoint: providerSettings.fetchSuccessMessage,
            })}
          </div>
        )}

        <label className="settings-field-label" htmlFor="provider-model">
          {t("settings.codex.provider.modelLabel")}
        </label>
        <div className="settings-field-row">
          <input
            id="provider-model"
            className="settings-input"
            value={providerSettings.draft.model ?? ""}
            placeholder={t("settings.codex.provider.modelPlaceholder")}
            disabled={providerSettings.isLoading}
            onChange={(event) =>
              providerSettings.setField("model", event.target.value)
            }
            list="provider-model-options"
          />
          {providerSettings.fetchedModels.length > 0 && (
            <select
              className="settings-select"
              value=""
              aria-label={t("settings.codex.provider.modelLabel")}
              onChange={(event) => {
                if (event.target.value) {
                  providerSettings.setField("model", event.target.value);
                }
              }}
            >
              <option value="" disabled>
                {`▾ ${providerSettings.fetchedModels.length}`}
              </option>
              {providerSettings.fetchedModels.map((model) => (
                <option key={model.id} value={model.id}>
                  {model.id}
                </option>
              ))}
            </select>
          )}
        </div>
        {providerSettings.fetchedModels.length > 0 && (
          <datalist id="provider-model-options">
            {providerSettings.fetchedModels.map((model) => (
              <option key={model.id} value={model.id} />
            ))}
          </datalist>
        )}
        <div className="settings-help">
          {t("settings.codex.provider.modelHelp")}
        </div>

        <label className="settings-field-label" htmlFor="provider-api-key">
          {t("settings.codex.provider.apiKeyLabel")}
        </label>
        <input
          id="provider-api-key"
          className="settings-input"
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={providerSettings.draft.apiKey ?? ""}
          placeholder={t("settings.codex.provider.apiKeyPlaceholder")}
          disabled={providerSettings.isLoading}
          onChange={(event) =>
            providerSettings.setField("apiKey", event.target.value)
          }
        />
        <div className="settings-help">
          {t("settings.codex.provider.apiKeyHelp")}
        </div>

        <label className="settings-field-label" htmlFor="provider-wire-api">
          {t("settings.codex.provider.wireApiLabel")}
        </label>
        <select
          id="provider-wire-api"
          className="settings-select"
          value="responses"
          disabled
          aria-readonly="true"
          onChange={() =>
            providerSettings.setField("wireApi", "responses")
          }
        >
          <option value="responses">
            {t("settings.codex.provider.wireApiResponses")}
          </option>
        </select>
        <div className="settings-help">
          {t("settings.codex.provider.wireApiHelp")}
        </div>

        <div className="settings-field-actions">
          <button
            type="button"
            className="primary"
            disabled={
              providerSettings.isSaving ||
              providerSettings.isLoading ||
              !providerSettings.isDirty
            }
            onClick={() => {
              void providerSettings.save();
            }}
          >
            {providerSettings.isSaving
              ? t("settings.codex.provider.saving")
              : t("settings.codex.provider.save")}
          </button>
        </div>
        {providerSettings.saveError && (
          <div className="settings-agents-error">
            {providerSettings.saveError}
          </div>
        )}
        {providerSettings.saved.configPath && (
          <div className="settings-help">
            {t("settings.codex.storedAt")}{" "}
            <code>{providerSettings.saved.configPath}</code>
          </div>
        )}
      </div>

      <div className="settings-divider" />
      <div className="settings-field-label settings-field-label--section">
        {t("settings.codex.defaultParameters")}
      </div>

      <SettingsToggleRow
        title={
          <label htmlFor="default-model">
            {t("settings.codex.model")}
          </label>
        }
        subtitle={
          defaultModelsConnectedWorkspaceCount === 0
            ? t("settings.codex.addWorkspaceToLoadModels")
            : defaultModelsLoading
              ? t("settings.codex.loadingModels")
              : defaultModelsError
                ? `${t("settings.codex.couldntLoadModels")}: ${defaultModelsError}`
                : t("settings.codex.modelSubtitle")
        }
      >
        <div className="settings-field-row">
          <select
            id="default-model"
            className="settings-select"
            value={selectedModelSlug}
            disabled={!defaultModels.length || defaultModelsLoading}
            onChange={(event) =>
              void onUpdateAppSettings({
                ...appSettings,
                lastComposerModelId: event.target.value,
              })
            }
            aria-label={t("settings.codex.model")}
          >
            {defaultModels.map((model) => (
              <option key={model.model} value={model.model}>
                {model.displayName?.trim() || model.model}
              </option>
            ))}
          </select>
          <button
            type="button"
            className="ghost"
            onClick={onRefreshDefaultModels}
            disabled={defaultModelsLoading || defaultModelsConnectedWorkspaceCount === 0}
          >
            {t("settings.codex.refresh")}
          </button>
        </div>
      </SettingsToggleRow>

      <SettingsToggleRow
        title={
          <label htmlFor="default-effort">
            {t("settings.codex.reasoningEffort")}
          </label>
        }
        subtitle={
          reasoningSupported
            ? t("settings.codex.reasoningEffortSubtitle")
            : t("settings.codex.reasoningEffortUnsupported")
        }
      >
        <select
          id="default-effort"
          className="settings-select"
          value={selectedEffort}
          onChange={(event) =>
            void onUpdateAppSettings({
              ...appSettings,
              lastComposerReasoningEffort: event.target.value,
            })
          }
          aria-label={t("settings.codex.reasoningEffort")}
          disabled={!reasoningSupported}
        >
          {!reasoningSupported && <option value="">{t("settings.codex.notSupported")}</option>}
          {reasoningOptions.map((effort) => (
            <option key={effort} value={effort}>
              {effort}
            </option>
          ))}
        </select>
      </SettingsToggleRow>

      <SettingsToggleRow
        title={
          <label htmlFor="default-access">
            {t("settings.codex.accessMode")}
          </label>
        }
        subtitle={t("settings.codex.accessModeSubtitle")}
      >
        <select
          id="default-access"
          className="settings-select"
          value={appSettings.defaultAccessMode}
          onChange={(event) =>
            void onUpdateAppSettings({
              ...appSettings,
              defaultAccessMode: event.target.value as AppSettings["defaultAccessMode"],
            })
          }
        >
          <option value="read-only">{t("settings.codex.readOnly")}</option>
          <option value="current">{t("settings.codex.onRequest")}</option>
          <option value="full-access">{t("settings.codex.fullAccess")}</option>
        </select>
      </SettingsToggleRow>
      <div className="settings-field">
        <label className="settings-field-label" htmlFor="review-delivery">
          {t("settings.codex.reviewMode")}
        </label>
        <select
          id="review-delivery"
          className="settings-select"
          value={appSettings.reviewDeliveryMode}
          onChange={(event) =>
            void onUpdateAppSettings({
              ...appSettings,
              reviewDeliveryMode: event.target.value as AppSettings["reviewDeliveryMode"],
            })
          }
        >
          <option value="inline">{t("settings.codex.reviewInline")}</option>
          <option value="detached">{t("settings.codex.reviewDetached")}</option>
        </select>
        <div className="settings-help">
          {t("settings.codex.reviewHelp")} <code>/review</code> {t("settings.codex.reviewHelpSuffix")}
        </div>
      </div>

      <FileEditorCard
        title={t("settings.codex.globalAgentsMd")}
        meta={globalAgentsMeta}
        error={globalAgentsError}
        value={globalAgentsContent}
        placeholder={t("settings.codex.globalAgentsPlaceholder")}
        disabled={globalAgentsLoading}
        refreshDisabled={globalAgentsRefreshDisabled}
        saveDisabled={globalAgentsSaveDisabled}
        saveLabel={globalAgentsSaveLabel}
        onChange={onSetGlobalAgentsContent}
        onRefresh={onRefreshGlobalAgents}
        onSave={onSaveGlobalAgents}
        helpText={
          <>
            {t("settings.codex.storedAt")} <code>~/.opencrab/AGENTS.md</code>.
          </>
        }
        classNames={{
          container: "settings-field settings-agents",
          header: "settings-agents-header",
          title: "settings-field-label",
          actions: "settings-agents-actions",
          meta: "settings-help settings-help-inline",
          iconButton: "ghost settings-icon-button",
          error: "settings-agents-error",
          textarea: "settings-agents-textarea",
          help: "settings-help",
        }}
      />

      <FileEditorCard
        title={t("settings.codex.globalConfigToml")}
        meta={globalConfigMeta}
        error={globalConfigError}
        value={globalConfigContent}
        placeholder={t("settings.codex.globalConfigPlaceholder")}
        disabled={globalConfigLoading}
        refreshDisabled={globalConfigRefreshDisabled}
        saveDisabled={globalConfigSaveDisabled}
        saveLabel={globalConfigSaveLabel}
        onChange={onSetGlobalConfigContent}
        onRefresh={onRefreshGlobalConfig}
        onSave={onSaveGlobalConfig}
        helpText={
          <>
            {t("settings.codex.storedAt")} <code>~/.opencrab/config.toml</code>.
          </>
        }
        classNames={{
          container: "settings-field settings-agents",
          header: "settings-agents-header",
          title: "settings-field-label",
          actions: "settings-agents-actions",
          meta: "settings-help settings-help-inline",
          iconButton: "ghost settings-icon-button",
          error: "settings-agents-error",
          textarea: "settings-agents-textarea",
          help: "settings-help",
        }}
      />
    </SettingsSection>
  );
}
