//! Read/write helpers for `~/.opencrab/config.toml` provider + model fields.
//!
//! These helpers expose a structured view over the subset of the global
//! `config.toml` that controls *which model and provider* OpenCrab talks to.
//! The UI in Settings → Codex uses this to render the Base URL / Model
//! controls and to refresh the model list when the user changes the URL.
//!
//! Only the following keys are touched — every other field in `config.toml`
//! is preserved as-is by `toml_edit`:
//!
//! ```text
//! model = "<model>"
//! model_provider = "<provider name>"
//!
//! [model_providers.<provider name>]
//! name = "..."
//! base_url = "..."
//! experimental_bearer_token = "<api key>"
//! wire_api = "responses"
//! ```
//!
//! `experimental_bearer_token` is codex's native field for "stash the API key
//! directly in config.toml" — see `codex-rs/model-provider-info`. We expose it
//! as `apiKey` to the UI so users don't have to deal with environment
//! variables.
//!
//! The `OPENCRAB_HOME` / `CODEX_HOME` env vars are intentionally ignored —
//! we always operate against `~/.opencrab` (see `codex::home`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use toml_edit::{value, Document, Item, Table};

use crate::shared::config_toml_core;

/// Default provider key used when the user has not chosen one yet.
const DEFAULT_PROVIDER_KEY: &str = "opencrab";
/// Default friendly display name written when the user leaves the field
/// blank. codex rejects an empty `name` (see `validate_model_providers`).
const DEFAULT_PROVIDER_NAME: &str = "OpenCrab Provider";
/// Default wire API to use for new providers.
///
/// codex 已经移除了 `chat`,反序列化时只接受 `"responses"`
/// (见 `codex-rs/model-provider-info/src/lib.rs::WireApi`)。
const DEFAULT_WIRE_API: &str = "responses";
/// Wire API that codex still recognizes. 历史 config.toml 中可能残留
/// `wire_api = "chat"`,我们读时把它归一化为 responses 避免后端起不来。
const SUPPORTED_WIRE_APIS: &[&str] = &["responses"];

/// Snapshot of the model + provider settings exposed to the UI.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelProviderSettings {
    /// Top-level `model` field in `config.toml`.
    pub model: Option<String>,
    /// Top-level `model_provider` key — selects which `[model_providers.*]`
    /// block is active. Defaults to `"opencrab"` when absent.
    pub provider_key: Option<String>,
    /// Friendly name written to `[model_providers.<key>].name`.
    pub provider_name: Option<String>,
    /// Provider base URL, e.g. `https://api.openai.com/v1`.
    pub base_url: Option<String>,
    /// Direct API key written to `[model_providers.<key>].experimental_bearer_token`.
    /// codex uses this for the `Authorization: Bearer <token>` header without
    /// requiring an environment variable.
    pub api_key: Option<String>,
    /// Wire API: `"chat"` (OpenAI Chat Completions) or `"responses"`.
    pub wire_api: Option<String>,
    /// Resolved path to the config.toml that was read (for display only).
    pub config_path: Option<String>,
}

/// Read the current model + provider settings from `~/.opencrab/config.toml`.
pub(crate) fn read_settings() -> Result<ModelProviderSettings, String> {
    let Some(root) = resolve_default_codex_home() else {
        return Err("Unable to resolve OPENCRAB_HOME".to_string());
    };
    let (_, mut document) = config_toml_core::load_global_config_document(&root)?;

    let model = config_toml_core::read_top_level_string(&document, "model");
    let provider_key = config_toml_core::read_top_level_string(&document, "model_provider");
    let lookup_key = provider_key
        .clone()
        .unwrap_or_else(|| DEFAULT_PROVIDER_KEY.to_string());

    let provider_table = document
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|t| t.get(&lookup_key))
        .and_then(Item::as_table_like);

    let provider_name = provider_table
        .and_then(|t| t.get("name"))
        .and_then(Item::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let base_url = provider_table
        .and_then(|t| t.get("base_url"))
        .and_then(Item::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // API key is stored under codex's native `experimental_bearer_token` so
    // that codex picks it up for `Authorization: Bearer <token>` without
    // requiring the user to set an environment variable.
    let api_key = provider_table
        .and_then(|t| t.get("experimental_bearer_token"))
        .and_then(Item::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let raw_wire_api = provider_table
        .and_then(|t| t.get("wire_api"))
        .and_then(Item::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Auto-migrate any legacy `chat` value (codex rejects it now) and any
    // unknown variant to the supported `responses` so the UI surfaces a value
    // that actually parses.
    let needs_wire_api_migration = matches!(
        raw_wire_api.as_deref(),
        Some(v) if !SUPPORTED_WIRE_APIS.iter().any(|expected| *expected == v),
    );
    let wire_api = match raw_wire_api {
        Some(value) if SUPPORTED_WIRE_APIS.iter().any(|v| *v == value) => Some(value),
        Some(_) => Some(DEFAULT_WIRE_API.to_string()),
        None => None,
    };

    // If the existing config.toml has fields that codex would reject at
    // parse / validation time (e.g. `wire_api = "chat"`, `name = ""`),
    // silently rewrite them so the next codex spawn succeeds without
    // requiring the user to know they have to click Save.
    let provider_block_exists = provider_table.is_some();
    let needs_name_migration = provider_block_exists && provider_name.is_none();
    if provider_block_exists && (needs_wire_api_migration || needs_name_migration) {
        if let Some(providers_table) =
            document.get_mut("model_providers").and_then(Item::as_table_mut)
        {
            if needs_wire_api_migration {
                set_or_remove_subtable_string(
                    providers_table,
                    &lookup_key,
                    "wire_api",
                    Some(DEFAULT_WIRE_API),
                );
            }
            if needs_name_migration {
                set_or_remove_subtable_string(
                    providers_table,
                    &lookup_key,
                    "name",
                    Some(DEFAULT_PROVIDER_NAME),
                );
            }
        }
        if let Err(err) = config_toml_core::persist_global_config_document(&root, &document) {
            // Don't fail the read if the migration write fails — surface the
            // logical settings so the UI can still render and the user can
            // manually save.
            eprintln!("opencrab: failed to auto-migrate config.toml: {err}");
        }
    }

    let config_path = root.join("config.toml").to_string_lossy().to_string();

    // Reflect the on-disk migration in the in-memory snapshot so the UI shows
    // the value we just persisted.
    let effective_name = if needs_name_migration {
        Some(DEFAULT_PROVIDER_NAME.to_string())
    } else {
        provider_name
    };

    Ok(ModelProviderSettings {
        model,
        provider_key,
        provider_name: effective_name,
        base_url,
        api_key,
        wire_api,
        config_path: Some(config_path),
    })
}

/// Write/merge the provided settings into `~/.opencrab/config.toml`.
///
/// Empty / whitespace-only string fields are *removed* from the document so
/// that users can clear values from the UI.
pub(crate) fn write_settings(settings: &ModelProviderSettings) -> Result<(), String> {
    let Some(root) = resolve_default_codex_home() else {
        return Err("Unable to resolve OPENCRAB_HOME".to_string());
    };
    let (_, mut document) = config_toml_core::load_global_config_document(&root)?;

    config_toml_core::set_top_level_string(&mut document, "model", settings.model.as_deref());

    // Determine the provider table key to write to.
    let provider_key = settings
        .provider_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PROVIDER_KEY)
        .to_string();
    config_toml_core::set_top_level_string(
        &mut document,
        "model_provider",
        Some(provider_key.as_str()),
    );

    let providers_table = config_toml_core::ensure_table(&mut document, "model_providers")?;

    // codex's `validate_model_providers` rejects an empty `name`; fall back
    // to a sensible default so saving with the field blank is still valid.
    let provider_name = settings
        .provider_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string());
    set_or_remove_subtable_string(
        providers_table,
        &provider_key,
        "name",
        Some(provider_name.as_str()),
    );
    set_or_remove_subtable_string(providers_table, &provider_key, "base_url", settings.base_url.as_deref());
    // Direct API key — codex's `experimental_bearer_token` field lets us
    // store the secret in config.toml instead of going through env vars.
    set_or_remove_subtable_string(
        providers_table,
        &provider_key,
        "experimental_bearer_token",
        settings.api_key.as_deref(),
    );
    // Make sure no leftover `env_key` lingers from previous installs — the UI
    // no longer manages it.
    set_or_remove_subtable_string(providers_table, &provider_key, "env_key", None);

    // codex 当前只接受 `responses`;若用户/历史配置传入 `chat` 或其他
    // 未知值,我们直接归一化到 responses,避免写出来的 config.toml 让
    // codex 在反序列化阶段就报错从而让 thread/start 失败。
    let raw_wire = settings
        .wire_api
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_WIRE_API);
    let wire_api = if SUPPORTED_WIRE_APIS.iter().any(|v| *v == raw_wire) {
        raw_wire
    } else {
        DEFAULT_WIRE_API
    };
    set_or_remove_subtable_string(providers_table, &provider_key, "wire_api", Some(wire_api));

    config_toml_core::persist_global_config_document(&root, &document)
}

/// Set or remove a string field on `[model_providers.<provider_key>]`.
fn set_or_remove_subtable_string(
    providers: &mut Table,
    provider_key: &str,
    field: &str,
    value_raw: Option<&str>,
) {
    let trimmed = value_raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Make sure the nested table exists when we have something to write.
    if trimmed.is_some() && providers.get(provider_key).is_none() {
        providers[provider_key] = Item::Table(Table::new());
    }

    let Some(provider_item) = providers.get_mut(provider_key) else {
        return;
    };
    let Some(provider_tbl) = provider_item.as_table_mut() else {
        return;
    };

    match trimmed {
        Some(s) => {
            provider_tbl[field] = value(s);
        }
        None => {
            let _ = provider_tbl.remove(field);
        }
    }
}

fn resolve_default_codex_home() -> Option<PathBuf> {
    crate::codex::home::resolve_default_codex_home()
}

// ---------------------------------------------------------------------------
// HTTP fetch — query the configured base URL for the list of available models.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FetchedModel {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FetchModelsResponse {
    pub models: Vec<FetchedModel>,
    /// Endpoint that was actually queried (helpful for debugging).
    pub endpoint: String,
}

/// Fetch `<base_url>/models` and return the parsed list of model ids.
///
/// Compatible with the OpenAI-style `GET /v1/models` response shape:
///
/// ```json
/// { "data": [ { "id": "...", "object": "model", "owned_by": "..." } ] }
/// ```
pub(crate) async fn fetch_models(
    base_url: &str,
    api_key: Option<String>,
) -> Result<FetchModelsResponse, String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return Err("Base URL is empty".to_string());
    }

    let endpoint = build_models_endpoint(trimmed);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|err| format!("Failed to build HTTP client: {err}"))?;

    let mut request = client.get(&endpoint);
    if let Some(key) = api_key.as_ref() {
        let trimmed_key = key.trim();
        if !trimmed_key.is_empty() {
            request = request.bearer_auth(trimmed_key);
        }
    }

    let response = request
        .send()
        .await
        .map_err(|err| format!("Request failed: {err}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unable to read body>".to_string());
        return Err(format!("HTTP {status}: {body}"));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|err| format!("Failed to parse response JSON: {err}"))?;

    let models = parse_models_payload(&body);
    Ok(FetchModelsResponse { models, endpoint })
}

fn build_models_endpoint(base_url: &str) -> String {
    let stripped = base_url.trim_end_matches('/');
    if stripped.ends_with("/models") {
        stripped.to_string()
    } else {
        format!("{stripped}/models")
    }
}

fn parse_models_payload(value: &serde_json::Value) -> Vec<FetchedModel> {
    // Accept several shapes:
    //   { "data": [ { "id": "..." }, ... ] }   (OpenAI-compatible)
    //   { "models": [ { "id": "..." }, ... ] } (some local servers)
    //   [ { "id": "..." }, ... ]               (bare array)
    let candidate = value
        .get("data")
        .or_else(|| value.get("models"))
        .or(Some(value));

    let Some(items) = candidate.and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|entry| {
            // Items can be strings (just an id) or objects.
            if let Some(s) = entry.as_str() {
                let id = s.trim().to_string();
                if id.is_empty() {
                    return None;
                }
                return Some(FetchedModel {
                    id,
                    object: None,
                    owned_by: None,
                });
            }
            let id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| entry.get("name").and_then(|v| v.as_str()))?
                .trim()
                .to_string();
            if id.is_empty() {
                return None;
            }
            Some(FetchedModel {
                id,
                object: entry
                    .get("object")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                owned_by: entry
                    .get("owned_by")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_models_endpoint_appends_models_when_missing() {
        assert_eq!(
            build_models_endpoint("https://api.example.com/v1"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            build_models_endpoint("https://api.example.com/v1/"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn build_models_endpoint_keeps_existing_models_suffix() {
        assert_eq!(
            build_models_endpoint("https://api.example.com/v1/models"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn parse_models_payload_handles_openai_shape() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"id":"gpt-4","object":"model","owned_by":"openai"},{"id":"gpt-3.5-turbo","object":"model","owned_by":"openai"}]}"#,
        )
        .unwrap();
        let parsed = parse_models_payload(&body);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "gpt-4");
        assert_eq!(parsed[1].id, "gpt-3.5-turbo");
    }

    #[test]
    fn parse_models_payload_handles_array_shape() {
        let body: serde_json::Value = serde_json::from_str(r#"["model-a","model-b"]"#).unwrap();
        let parsed = parse_models_payload(&body);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "model-a");
        assert_eq!(parsed[1].id, "model-b");
    }

    #[test]
    fn parse_models_payload_handles_models_key() {
        let body: serde_json::Value =
            serde_json::from_str(r#"{"models":[{"name":"local-7b"}]}"#).unwrap();
        let parsed = parse_models_payload(&body);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "local-7b");
    }

    #[test]
    fn supported_wire_apis_only_responses() {
        assert!(SUPPORTED_WIRE_APIS.iter().any(|v| *v == "responses"));
        assert!(!SUPPORTED_WIRE_APIS.iter().any(|v| *v == "chat"));
        assert_eq!(DEFAULT_WIRE_API, "responses");
    }

    #[test]
    fn default_provider_name_is_non_empty() {
        // codex's `validate_model_providers` rejects an empty `name`.
        assert!(!DEFAULT_PROVIDER_NAME.trim().is_empty());
    }
}
