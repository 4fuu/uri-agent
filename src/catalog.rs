use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use futures_util::{StreamExt, stream};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::RwLock;
use uuid::Uuid;

const PROVIDERS_URL: &str = "https://pi.dev/api/models/providers";
const REFRESH_INTERVAL_MS: i64 = 4 * 60 * 60 * 1000;
const REQUEST_CONCURRENCY: usize = 8;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

impl CatalogModel {
    pub fn supported(&self) -> bool {
        matches!(
            self.api.as_str(),
            "openai-responses"
                | "openai-codex-responses"
                | "openai-completions"
                | "anthropic-messages"
                | "google-generative-ai"
                | "antigravity"
        )
    }

    pub fn context_window(&self) -> usize {
        self.metadata
            .get("contextWindow")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .unwrap_or(128_000)
    }

    pub fn reasoning(&self) -> bool {
        self.metadata
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn hidden(&self) -> bool {
        self.metadata
            .get("hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn accepts_input(&self, modality: &str) -> bool {
        self.metadata
            .get("input")
            .and_then(Value::as_array)
            .map_or(modality == "text", |modalities| {
                modalities.iter().any(|value| value == modality)
            })
    }

    pub fn compat(&self, key: &str) -> Option<&Value> {
        self.metadata.get("compat").and_then(|value| value.get(key))
    }

    pub fn thinking_level(&self, level: ThinkingLevel) -> Option<&Value> {
        self.metadata
            .get("thinkingLevelMap")
            .and_then(|value| value.get(level.as_str()))
    }

    pub fn supports_thinking_level(&self, level: ThinkingLevel) -> bool {
        if !self.reasoning() {
            return level == ThinkingLevel::Off;
        }
        match self.thinking_level(level) {
            Some(Value::Null) => false,
            Some(_) => true,
            None => !matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max),
        }
    }

    pub fn sampling_params(&self) -> Option<&serde_json::Map<String, Value>> {
        self.metadata
            .get("samplingParams")
            .and_then(Value::as_object)
    }

    /// Output ceiling from the catalog's `maxTokens`; the provider adapter
    /// selects the corresponding wire field. The fallback matches pi's default
    /// for user-defined models.
    pub fn max_tokens(&self) -> u64 {
        self.metadata
            .get("maxTokens")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .unwrap_or(16_384)
    }

    pub fn cost(&self) -> ModelCost {
        let rates = self.metadata.get("cost");
        let rate = |key: &str| {
            rates
                .and_then(|cost| cost.get(key))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        let tiers = rates
            .and_then(|cost| cost.get("tiers"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tier| {
                let rate = |key: &str| tier.get(key).and_then(Value::as_f64);
                Some(ModelCostTier {
                    input_tokens_above: tier.get("inputTokensAbove")?.as_u64()?,
                    input: rate("input")?,
                    output: rate("output")?,
                    cache_read: rate("cacheRead")?,
                    cache_write: rate("cacheWrite")?,
                })
            })
            .collect();
        ModelCost {
            input: rate("input"),
            output: rate("output"),
            cache_read: rate("cacheRead"),
            cache_write: rate("cacheWrite"),
            tiers,
        }
    }

    pub fn limits(&self) -> ModelLimits {
        ModelLimits {
            context_window: self.context_window(),
            max_tokens: self.max_tokens(),
            cost: self.cost(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub const fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub const fn budget(self) -> u64 {
        match self {
            Self::Off => 0,
            Self::Minimal => 1_024,
            Self::Low => 2_048,
            Self::Medium => 8_192,
            Self::High | Self::Xhigh | Self::Max => 16_384,
        }
    }
}

impl std::fmt::Display for ThinkingLevel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => bail!(
                "thinking level must be one of off, minimal, low, medium, high, xhigh, or max"
            ),
        }
    }
}

/// Per-million-token USD rates from the catalog's `cost` object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub tiers: Vec<ModelCostTier>,
}

impl ModelCost {
    pub fn total(&self, input: u64, output: u64, cache_read: u64, cache_write: u64) -> f64 {
        let total_input = input.saturating_add(cache_read).saturating_add(cache_write);
        let rates = self
            .tiers
            .iter()
            .filter(|tier| total_input > tier.input_tokens_above)
            .max_by_key(|tier| tier.input_tokens_above);
        let (input_rate, output_rate, cache_read_rate, cache_write_rate) = rates.map_or(
            (self.input, self.output, self.cache_read, self.cache_write),
            |tier| (tier.input, tier.output, tier.cache_read, tier.cache_write),
        );
        (input as f64 * input_rate
            + output as f64 * output_rate
            + cache_read as f64 * cache_read_rate
            + cache_write as f64 * cache_write_rate)
            / 1_000_000.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelCostTier {
    pub input_tokens_above: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Clone, Debug)]
pub struct ModelLimits {
    pub context_window: usize,
    pub max_tokens: u64,
    pub cost: ModelCost,
}

impl Default for ModelLimits {
    fn default() -> Self {
        Self {
            context_window: 128_000,
            max_tokens: 16_384,
            cost: ModelCost::default(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreEntry {
    models: Vec<CatalogModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_modified: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checked_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct ModelsFile {
    providers: BTreeMap<String, UserProvider>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct UserProvider {
    name: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    api: Option<String>,
    headers: BTreeMap<String, String>,
    compat: Option<Value>,
    auth_header: Option<bool>,
    models: Vec<Value>,
    model_overrides: BTreeMap<String, Value>,
}

struct CatalogState {
    store: BTreeMap<String, StoreEntry>,
    user: ModelsFile,
    models: BTreeMap<String, Vec<CatalogModel>>,
    warnings: Vec<String>,
}

#[derive(Clone)]
pub struct ModelCatalog {
    inner: Arc<RwLock<CatalogState>>,
    store_path: PathBuf,
    user_path: PathBuf,
    client: reqwest::Client,
    offline: bool,
}

impl ModelCatalog {
    pub async fn load(directory: &Path, offline: bool) -> Result<Self> {
        let store_path = directory.join("models-store.json");
        let user_path = directory.join("models.json");
        let store = read_json(&store_path).await?;
        let user = read_json(&user_path).await?;
        let (models, warnings) = merge_catalog(&store, &user);
        let catalog = Self {
            inner: Arc::new(RwLock::new(CatalogState {
                store,
                user,
                models,
                warnings,
            })),
            store_path,
            user_path,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(4))
                .user_agent(concat!("uri-agent/", env!("CARGO_PKG_VERSION")))
                .build()?,
            offline,
        };
        Ok(catalog)
    }

    pub fn networking_enabled(&self) -> bool {
        !self.offline
    }

    pub async fn refresh(&self, force: bool) -> Result<()> {
        if self.offline {
            bail!("model catalog networking is disabled");
        }
        let providers = self
            .client
            .get(PROVIDERS_URL)
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<String>>()
            .await?;
        let current = self.inner.read().await.store.clone();
        let now = Utc::now().timestamp_millis();
        let client = self.client.clone();
        let refreshed = stream::iter(providers.into_iter().map(|provider| {
            let client = client.clone();
            let cached = current.get(&provider).cloned().unwrap_or_default();
            async move {
                if !force
                    && cached
                        .checked_at
                        .is_some_and(|checked| now - checked < REFRESH_INTERVAL_MS)
                    && cached.last_modified.is_some()
                {
                    return (provider, cached.clone(), Ok(cached));
                }
                let fallback = cached.clone();
                let result = fetch_provider(&client, &provider, cached, now).await;
                (provider, fallback, result)
            }
        }))
        .buffer_unordered(REQUEST_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

        let mut state = self.inner.write().await;
        for (provider, mut fallback, result) in refreshed {
            match result {
                Ok(entry) => {
                    state.store.insert(provider, entry);
                }
                Err(error) => {
                    fallback.checked_at = Some(now);
                    state.store.insert(provider.clone(), fallback);
                    state
                        .warnings
                        .push(format!("catalog {provider}: {error:#}"));
                }
            }
        }
        write_json(&self.store_path, &state.store).await?;
        state.user = read_json(&self.user_path).await?;
        let (models, warnings) = merge_catalog(&state.store, &state.user);
        state.models = models;
        state.warnings.extend(warnings);
        Ok(())
    }

    pub async fn reload_user_overrides(&self) -> Result<()> {
        let user = read_json(&self.user_path).await?;
        let mut state = self.inner.write().await;
        let (models, warnings) = merge_catalog(&state.store, &user);
        state.user = user;
        state.models = models;
        state.warnings.extend(warnings);
        Ok(())
    }

    pub async fn providers(&self) -> Vec<String> {
        self.inner
            .read()
            .await
            .models
            .iter()
            .filter(|(_, models)| models.iter().any(CatalogModel::supported))
            .map(|(provider, _)| provider.clone())
            .collect()
    }

    pub async fn models(&self, provider: &str) -> Vec<CatalogModel> {
        self.inner
            .read()
            .await
            .models
            .get(provider)
            .into_iter()
            .flatten()
            .filter(|model| model.supported() && !model.hidden())
            .cloned()
            .collect()
    }

    pub async fn model(&self, provider: &str, model: &str) -> Option<CatalogModel> {
        self.inner
            .read()
            .await
            .models
            .get(provider)
            .into_iter()
            .flatten()
            .filter(|entry| entry.supported())
            .find(|entry| entry.id == model)
            .cloned()
    }

    pub async fn configured_api_key(&self, provider: &str) -> Option<String> {
        self.inner
            .read()
            .await
            .user
            .providers
            .get(provider)
            .and_then(|entry| entry.api_key.clone())
    }

    pub async fn warnings(&self) -> Vec<String> {
        self.inner.read().await.warnings.clone()
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub fn user_path(&self) -> &Path {
        &self.user_path
    }
}

async fn fetch_provider(
    client: &reqwest::Client,
    provider: &str,
    cached: StoreEntry,
    now: i64,
) -> Result<StoreEntry> {
    let url = format!("{PROVIDERS_URL}/{provider}");
    let mut request = client.get(url).header("Accept", "application/json");
    if !cached.models.is_empty()
        && let Some(etag) = &cached.etag
    {
        request = request.header("If-None-Match", etag);
    }
    let response = request.send().await?;
    if response.status() == StatusCode::NOT_MODIFIED {
        return Ok(StoreEntry {
            checked_at: Some(now),
            ..cached
        });
    }
    if matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::NOT_IMPLEMENTED
    ) {
        return Ok(StoreEntry {
            checked_at: Some(now),
            last_modified: Some(0),
            etag: None,
            ..cached
        });
    }
    let response = response.error_for_status()?;
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
        .map_or(0, |value| value.timestamp_millis());
    let value = response.json::<Value>().await?;
    let models = parse_provider_payload(provider, value)?;
    Ok(StoreEntry {
        models,
        checked_at: Some(now),
        last_modified: Some(last_modified),
        etag,
    })
}

fn parse_provider_payload(provider: &str, value: Value) -> Result<Vec<CatalogModel>> {
    let values = match value {
        Value::Array(models) => models,
        Value::Object(mut object) => match object.remove("models") {
            Some(Value::Array(models)) => models,
            Some(_) => bail!("provider models field is not an array"),
            None => object.into_values().collect(),
        },
        _ => bail!("provider catalog must be an array or object"),
    };
    values
        .into_iter()
        .map(|mut value| {
            value
                .as_object_mut()
                .ok_or_else(|| anyhow!("catalog model is not an object"))?
                .insert("provider".to_string(), Value::String(provider.to_string()));
            serde_json::from_value(value).context("invalid Pi catalog model")
        })
        .collect()
}

fn merge_catalog(
    store: &BTreeMap<String, StoreEntry>,
    user: &ModelsFile,
) -> (BTreeMap<String, Vec<CatalogModel>>, Vec<String>) {
    let mut raw = built_in_catalog();
    for (provider, entry) in store {
        let models = raw.entry(provider.clone()).or_default();
        models.extend(
            entry
                .models
                .iter()
                .filter_map(|model| serde_json::to_value(model).ok())
                .filter_map(|model| {
                    let id = model.get("id").and_then(Value::as_str)?.to_string();
                    Some((id, model))
                }),
        );
    }
    let mut warnings = Vec::new();

    for (provider_id, provider) in &user.providers {
        let models = raw.entry(provider_id.clone()).or_default();
        for model in models.values_mut() {
            apply_provider_defaults(model, provider);
        }
        for custom in &provider.models {
            let Some(id) = custom.get("id").and_then(Value::as_str) else {
                warnings.push(format!(
                    "models.json provider {provider_id} has a model without id"
                ));
                continue;
            };
            let mut model = models.get(id).cloned().unwrap_or_else(|| {
                serde_json::json!({
                    "id": id,
                    "name": id,
                    "provider": provider_id,
                    "reasoning": false,
                    "input": ["text"],
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                    "contextWindow": 128000,
                    "maxTokens": 16384
                })
            });
            apply_provider_defaults(&mut model, provider);
            merge_value(&mut model, custom.clone());
            if let Some(object) = model.as_object_mut() {
                object.insert("id".to_string(), Value::String(id.to_string()));
                object.insert("provider".to_string(), Value::String(provider_id.clone()));
            }
            models.insert(id.to_string(), model);
        }
        for (id, patch) in &provider.model_overrides {
            let Some(model) = models.get_mut(id) else {
                continue;
            };
            let mut patch = patch.clone();
            if let Some(object) = patch.as_object_mut() {
                for immutable in ["id", "provider", "api", "baseUrl"] {
                    object.remove(immutable);
                }
            }
            merge_value(model, patch);
        }
    }

    let models = raw
        .into_iter()
        .map(|(provider, entries)| {
            let mut parsed = entries
                .into_values()
                .filter_map(
                    |value| match serde_json::from_value::<CatalogModel>(value) {
                        Ok(model) if !model.supported() => Some(model),
                        Ok(model)
                            if !model.id.is_empty()
                                && !model.api.is_empty()
                                && !model.base_url.is_empty() =>
                        {
                            Some(model)
                        }
                        Ok(model) => {
                            warnings.push(format!(
                                "catalog skipped {provider}/{} without api or baseUrl",
                                model.id
                            ));
                            None
                        }
                        Err(error) => {
                            warnings
                                .push(format!("catalog skipped invalid {provider} model: {error}"));
                            None
                        }
                    },
                )
                .collect::<Vec<_>>();
            parsed.sort_by(|left, right| left.id.cmp(&right.id));
            (provider, parsed)
        })
        .collect();
    (models, warnings)
}

fn built_in_catalog() -> BTreeMap<String, BTreeMap<String, Value>> {
    let tiered = |id: &str, name: &str, context_window: u64, max_tokens: u64, routes: Value| {
        antigravity_model(
            id,
            name,
            true,
            (context_window, max_tokens),
            false,
            serde_json::json!({
                "off": null,
                "minimal": null,
                "low": "low",
                "medium": "medium",
                "high": "high",
                "xhigh": null,
                "max": null
            }),
            routes,
        )
    };
    let gemini_37_routes = serde_json::json!({
        "low": {"model": "gemini-3.7-flash-low", "thinkingBudget": 1000, "maxOutputTokens": 65536},
        "medium": {"model": "gemini-3.7-flash-medium", "thinkingBudget": 4000, "maxOutputTokens": 65536},
        "high": {"model": "gemini-3.7-flash-high", "thinkingBudget": 10000, "maxOutputTokens": 65536}
    });
    let gemini_35_routes = serde_json::json!({
        "low": {"model": "gemini-3.5-flash-extra-low", "thinkingBudget": 1000, "maxOutputTokens": 65536},
        "medium": {"model": "gemini-3.5-flash-low", "thinkingBudget": 4000, "maxOutputTokens": 65536},
        "high": {"model": "gemini-3-flash-agent", "thinkingBudget": 10000, "maxOutputTokens": 65536}
    });
    let gemini_31_routes = serde_json::json!({
        "low": {"model": "gemini-3.1-pro-low", "thinkingBudget": 1001, "maxOutputTokens": 65535},
        "medium": {"model": "gemini-pro-agent", "thinkingBudget": 10001, "maxOutputTokens": 65535},
        "high": {"model": "gemini-pro-agent", "thinkingBudget": 10001, "maxOutputTokens": 65535}
    });
    let claude_sonnet_routes = serde_json::json!({
        "off": {"model": "claude-sonnet-4-6", "thinkingBudget": 0, "maxOutputTokens": 64000, "includeThoughts": false},
        "low": {"model": "claude-sonnet-4-6", "thinkingBudget": 8192, "maxOutputTokens": 64000},
        "medium": {"model": "claude-sonnet-4-6", "thinkingBudget": 16384, "maxOutputTokens": 64000},
        "high": {"model": "claude-sonnet-4-6", "thinkingBudget": 24576, "maxOutputTokens": 64000},
        "max": {"model": "claude-sonnet-4-6", "thinkingBudget": 32768, "maxOutputTokens": 64000}
    });
    let claude_opus_routes = serde_json::json!({
        "low": {"model": "claude-opus-4-6-thinking", "thinkingBudget": 8192, "maxOutputTokens": 64000},
        "medium": {"model": "claude-opus-4-6-thinking", "thinkingBudget": 16384, "maxOutputTokens": 64000},
        "high": {"model": "claude-opus-4-6-thinking", "thinkingBudget": 24576, "maxOutputTokens": 64000},
        "max": {"model": "claude-opus-4-6-thinking", "thinkingBudget": 32768, "maxOutputTokens": 64000}
    });

    let models = BTreeMap::from([
        (
            "gemini-3.7-flash".to_string(),
            tiered(
                "gemini-3.7-flash",
                "Gemini 3.7 Flash",
                1_000_000,
                65_536,
                gemini_37_routes.clone(),
            ),
        ),
        (
            "gemini-3.5-flash".to_string(),
            tiered(
                "gemini-3.5-flash",
                "Gemini 3.5 Flash",
                1_000_000,
                65_536,
                gemini_35_routes.clone(),
            ),
        ),
        (
            "gemini-3.1-pro".to_string(),
            tiered(
                "gemini-3.1-pro",
                "Gemini 3.1 Pro",
                1_048_576,
                65_535,
                gemini_31_routes.clone(),
            ),
        ),
        (
            "gemini-3.1-flash-lite".to_string(),
            antigravity_model(
                "gemini-3.1-flash-lite",
                "Gemini 3.1 Flash Lite",
                false,
                (1_048_576, 16_384),
                false,
                serde_json::json!({}),
                serde_json::json!({
                    "off": {"model": "gemini-3.1-flash-lite", "thinkingBudget": 0, "maxOutputTokens": 16384, "includeThoughts": false}
                }),
            ),
        ),
        (
            "claude-sonnet-4-6".to_string(),
            antigravity_model(
                "claude-sonnet-4-6",
                "Claude Sonnet 4.6",
                true,
                (200_000, 64_000),
                false,
                serde_json::json!({
                    "off": "off", "minimal": null, "low": "low", "medium": "medium",
                    "high": "high", "xhigh": null, "max": "max"
                }),
                claude_sonnet_routes.clone(),
            ),
        ),
        (
            "claude-opus-4-6".to_string(),
            antigravity_model(
                "claude-opus-4-6",
                "Claude Opus 4.6",
                true,
                (200_000, 64_000),
                false,
                serde_json::json!({
                    "off": null, "minimal": null, "low": "low", "medium": "medium",
                    "high": "high", "xhigh": null, "max": "max"
                }),
                claude_opus_routes.clone(),
            ),
        ),
    ]);
    BTreeMap::from([("antigravity".to_string(), models)])
}

fn antigravity_model(
    id: &str,
    name: &str,
    reasoning: bool,
    limits: (u64, u64),
    hidden: bool,
    thinking_level_map: Value,
    routes: Value,
) -> Value {
    let (context_window, max_tokens) = limits;
    serde_json::json!({
        "id": id,
        "name": name,
        "api": "antigravity",
        "provider": "antigravity",
        "baseUrl": "https://cloudcode-pa.googleapis.com",
        "reasoning": reasoning,
        "hidden": hidden,
        "input": ["text", "image"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": context_window,
        "maxTokens": max_tokens,
        "thinkingLevelMap": thinking_level_map,
        "compat": {
            "antigravityRoutes": routes
        }
    })
}

fn apply_provider_defaults(model: &mut Value, provider: &UserProvider) {
    let Some(object) = model.as_object_mut() else {
        return;
    };
    if let Some(name) = &provider.name {
        object
            .entry("providerName")
            .or_insert_with(|| Value::String(name.clone()));
    }
    if let Some(base_url) = &provider.base_url {
        object.insert("baseUrl".to_string(), Value::String(base_url.clone()));
    }
    if let Some(api) = &provider.api {
        object.insert("api".to_string(), Value::String(api.clone()));
    }
    if !provider.headers.is_empty() {
        let headers = object
            .entry("headers")
            .or_insert_with(|| Value::Object(Map::new()));
        merge_value(
            headers,
            serde_json::to_value(&provider.headers).unwrap_or_default(),
        );
    }
    if let Some(compat) = &provider.compat {
        let model_compat = object
            .entry("compat")
            .or_insert_with(|| Value::Object(Map::new()));
        merge_value(model_compat, compat.clone());
    }
    if let Some(auth_header) = provider.auth_header {
        object.insert("authHeader".to_string(), Value::Bool(auth_header));
    }
}

fn merge_value(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_value(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

pub fn api_key_environment(provider: &str) -> String {
    match provider {
        "amazon-bedrock" => "AWS_BEARER_TOKEN_BEDROCK",
        "ant-ling" => "ANT_LING_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "azure-openai-responses" => "AZURE_OPENAI_API_KEY",
        "baseten" => "BASETEN_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => "CLOUDFLARE_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "fireworks" => "FIREWORKS_API_KEY",
        "github-copilot" => "COPILOT_GITHUB_TOKEN",
        "google" => "GEMINI_API_KEY",
        "google-vertex" => "GOOGLE_CLOUD_API_KEY",
        "groq" => "GROQ_API_KEY",
        "huggingface" => "HF_TOKEN",
        "kimi-coding" => "KIMI_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "minimax-cn" => "MINIMAX_CN_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "moonshotai" | "moonshotai-cn" => "MOONSHOT_API_KEY",
        "nvidia" => "NVIDIA_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "opencode" | "opencode-go" => "OPENCODE_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "qwen-token-plan" | "qwen-token-plan-individual" => "QWEN_TOKEN_PLAN_API_KEY",
        "qwen-token-plan-cn" => "QWEN_TOKEN_PLAN_CN_API_KEY",
        "together" => "TOGETHER_API_KEY",
        "vercel-ai-gateway" => "AI_GATEWAY_API_KEY",
        "xai" => "XAI_API_KEY",
        "xiaomi" => "XIAOMI_API_KEY",
        "xiaomi-token-plan-ams" => "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
        "xiaomi-token-plan-cn" => "XIAOMI_TOKEN_PLAN_CN_API_KEY",
        "xiaomi-token-plan-sgp" => "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
        "zai" => "ZAI_API_KEY",
        "zai-coding-cn" => "ZAI_CODING_CN_API_KEY",
        other => {
            return format!(
                "{}_API_KEY",
                other
                    .chars()
                    .map(|character| if character.is_ascii_alphanumeric() {
                        character.to_ascii_uppercase()
                    } else {
                        '_'
                    })
                    .collect::<String>()
            );
        }
    }
    .to_string()
}

async fn read_json<T>(path: &Path) -> Result<T>
where
    T: Default + for<'de> Deserialize<'de>,
{
    match fs::read(path).await {
        Ok(content) => serde_json::from_slice(&content)
            .with_context(|| format!("cannot parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

async fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("catalog path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).await?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("catalog");
    let temporary = parent.join(format!(".{filename}.{}.tmp", Uuid::now_v7().simple()));
    let mut content = serde_json::to_vec_pretty(value)?;
    content.push(b'\n');
    fs::write(&temporary, content).await?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).await?;
    }
    if let Err(error) = fs::rename(&temporary, path).await {
        let _ = fs::remove_file(&temporary).await;
        return Err(error).with_context(|| format!("cannot replace {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_models_are_built_in_and_user_overrides_remain_effective() {
        let user: ModelsFile = serde_json::from_value(serde_json::json!({
            "providers": {
                "antigravity": {
                    "modelOverrides": {
                        "gemini-3.7-flash": {"name": "Private Flash"}
                    }
                }
            }
        }))
        .unwrap();
        let (merged, warnings) = merge_catalog(&BTreeMap::new(), &user);
        assert!(warnings.is_empty());
        let models = &merged["antigravity"];
        let gemini = models
            .iter()
            .find(|model| model.id == "gemini-3.7-flash")
            .unwrap();
        assert_eq!(gemini.name, "Private Flash");
        assert_eq!(gemini.api, "antigravity");
        assert_eq!(gemini.limits().context_window, 1_000_000);
        assert_eq!(gemini.limits().max_tokens, 65_536);
        assert_eq!(
            gemini
                .compat("antigravityRoutes")
                .and_then(|routes| routes.get("medium"))
                .and_then(|route| route.get("model"))
                .and_then(Value::as_str),
            Some("gemini-3.7-flash-medium")
        );
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "gemini-3.1-flash-lite",
                "gemini-3.1-pro",
                "gemini-3.5-flash",
                "gemini-3.7-flash",
            ]
        );
        assert!(models.iter().all(|model| !model.hidden()));
    }

    #[test]
    fn pi_provider_payloads_and_user_overrides_merge_by_model_id() {
        let models = parse_provider_payload(
            "openai",
            serde_json::json!({
                "one": {
                    "id": "one", "name": "One", "api": "openai-responses",
                    "provider": "wrong", "baseUrl": "https://example.test/v1"
                }
            }),
        )
        .unwrap();
        let store = BTreeMap::from([(
            "openai".to_string(),
            StoreEntry {
                models,
                ..StoreEntry::default()
            },
        )]);
        let user: ModelsFile = serde_json::from_value(serde_json::json!({
            "providers": {
                "openai": {
                    "baseUrl": "https://override.test/v1",
                    "models": [{"id": "two", "api": "openai-completions"}],
                    "modelOverrides": {"one": {"name": "Overridden"}}
                }
            }
        }))
        .unwrap();
        let (merged, warnings) = merge_catalog(&store, &user);
        assert!(warnings.is_empty());
        assert_eq!(merged["openai"].len(), 2);
        assert_eq!(merged["openai"][0].name, "Overridden");
        assert_eq!(merged["openai"][0].base_url, "https://override.test/v1");
        assert_eq!(merged["openai"][1].api, "openai-completions");
    }

    #[test]
    fn unsupported_pi_apis_remain_cached_but_are_not_runnable() {
        let model: CatalogModel = serde_json::from_value(serde_json::json!({
            "id": "bedrock", "name": "Bedrock", "api": "bedrock-converse-stream",
            "provider": "amazon-bedrock", "baseUrl": "https://example.test"
        }))
        .unwrap();
        assert!(!model.supported());
        assert_eq!(api_key_environment("openrouter"), "OPENROUTER_API_KEY");
        assert_eq!(api_key_environment("my-provider"), "MY_PROVIDER_API_KEY");

        let incomplete: CatalogModel = serde_json::from_value(serde_json::json!({
            "id": "azure", "name": "Azure", "api": "azure-openai-responses",
            "provider": "azure-openai-responses", "baseUrl": ""
        }))
        .unwrap();
        let store = BTreeMap::from([(
            "azure-openai-responses".to_string(),
            StoreEntry {
                models: vec![incomplete],
                ..StoreEntry::default()
            },
        )]);
        let (merged, warnings) = merge_catalog(&store, &ModelsFile::default());
        assert!(warnings.is_empty());
        assert_eq!(merged["azure-openai-responses"].len(), 1);
    }

    #[test]
    fn openai_codex_catalog_models_are_runnable() {
        let model: CatalogModel = serde_json::from_value(serde_json::json!({
            "id": "gpt-5.4", "name": "GPT-5.4", "api": "openai-codex-responses",
            "provider": "openai-codex", "baseUrl": "https://chatgpt.com/backend-api"
        }))
        .unwrap();

        assert!(model.supported());
    }

    #[test]
    fn max_tokens_and_cost_come_from_the_remote_catalog_with_safe_fallbacks() {
        let model: CatalogModel = serde_json::from_value(serde_json::json!({
            "id": "k3", "name": "Kimi K3", "api": "anthropic-messages",
            "provider": "kimi-coding", "baseUrl": "https://api.kimi.com/coding",
            "contextWindow": 1048576,
            "maxTokens": 131072,
            "cost": {"input": 3, "output": 15, "cacheRead": 0.3, "cacheWrite": 0}
        }))
        .unwrap();
        assert_eq!(model.max_tokens(), 131_072);
        let cost = model.cost();
        assert_eq!(cost.input, 3.0);
        assert_eq!(cost.cache_read, 0.3);
        assert_eq!(cost.total(1_000_000, 100_000, 0, 0), 4.5);
        let limits = model.limits();
        assert_eq!(limits.context_window, 1_048_576);
        assert_eq!(limits.max_tokens, 131_072);

        let tiered: CatalogModel = serde_json::from_value(serde_json::json!({
            "id": "tiered", "name": "Tiered", "api": "openai-responses",
            "provider": "openai", "baseUrl": "https://api.openai.com/v1",
            "cost": {
                "input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 0.2,
                "tiers": [{
                    "inputTokensAbove": 1000,
                    "input": 3, "output": 4, "cacheRead": 0.3, "cacheWrite": 0.4
                }]
            }
        }))
        .unwrap();
        assert_eq!(tiered.cost().total(800, 100, 100, 100), 0.001_03);
        assert_eq!(tiered.cost().total(801, 100, 100, 100), 0.002_873);

        let bare: CatalogModel = serde_json::from_value(serde_json::json!({
            "id": "one", "name": "One", "api": "openai-responses",
            "provider": "openai", "baseUrl": "https://example.test/v1"
        }))
        .unwrap();
        assert_eq!(bare.max_tokens(), 16_384);
        assert_eq!(bare.cost(), ModelCost::default());
    }

    #[test]
    fn every_remote_model_field_round_trips_without_loss() {
        let original = serde_json::json!({
            "id": "complete",
            "name": "Complete",
            "api": "openai-completions",
            "provider": "test",
            "baseUrl": "https://example.test/v1",
            "headers": {"x-custom": "value"},
            "reasoning": true,
            "input": ["text", "image"],
            "cost": {
                "input": 1,
                "output": 2,
                "cacheRead": 0.1,
                "cacheWrite": 0.2,
                "tiers": [{
                    "inputTokensAbove": 200000,
                    "input": 3,
                    "output": 4,
                    "cacheRead": 0.3,
                    "cacheWrite": 0.4
                }]
            },
            "contextWindow": 262144,
            "maxTokens": 65536,
            "compat": {
                "maxTokensField": "max_tokens",
                "forceAdaptiveThinking": true,
                "futureCompatFlag": {"nested": true}
            },
            "thinkingLevelMap": {"off": null, "minimal": 512, "high": "max"},
            "samplingParams": {"top_p": 0.9},
            "futureTopLevelField": [1, {"opaque": true}]
        });
        let model: CatalogModel = serde_json::from_value(original.clone()).unwrap();
        assert!(model.reasoning());
        assert!(model.accepts_input("image"));
        assert_eq!(
            model.thinking_level(ThinkingLevel::Minimal),
            Some(&serde_json::json!(512))
        );
        assert_eq!(serde_json::to_value(model).unwrap(), original);
    }

    #[test]
    fn context_window_uses_pi_metadata_with_a_safe_fallback() {
        let mut model: CatalogModel = serde_json::from_value(serde_json::json!({
            "id": "one", "name": "One", "api": "openai-responses",
            "provider": "openai", "baseUrl": "https://example.test/v1",
            "contextWindow": 131072
        }))
        .unwrap();
        assert_eq!(model.context_window(), 131_072);
        model.metadata.remove("contextWindow");
        assert_eq!(model.context_window(), 128_000);
    }
}
