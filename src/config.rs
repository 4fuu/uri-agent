use crate::catalog::{CatalogModel, ModelCatalog, ThinkingLevel, api_key_environment};
use crate::compaction;
use crate::keymap::KeyDisplayStyle;
use crate::oauth::{self, OauthToken};
use crate::session::SessionChoice;
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

const DEFAULT_OUTPUT_LIMIT: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AuthKind {
    #[default]
    None,
    ApiKey,
    Oauth,
}

#[derive(Clone, Parser, Debug)]
#[command(name = "uri-agent", version, about)]
pub struct Cli {
    /// Provider ID from the Pi model catalog, for example openai or anthropic.
    #[arg(long)]
    pub provider: Option<String>,

    /// Model ID from the selected provider.
    #[arg(long)]
    pub model: Option<String>,

    /// Provider API key for this invocation. Prefer auth.json or the provider environment variable.
    #[arg(long, hide_env_values = true)]
    pub api_key: Option<String>,

    /// Override the number of bytes returned inline before URI Agent spills output to a file.
    #[arg(long)]
    pub output_limit: Option<usize>,

    /// Reasoning effort for capable models (off, minimal, low, medium, high, xhigh, or max).
    #[arg(long, value_name = "LEVEL")]
    pub thinking: Option<ThinkingLevel>,

    /// Disable pi.dev model-catalog network requests and use the local cache only.
    #[arg(long)]
    pub offline: bool,

    /// Resume the most recently updated session.
    #[arg(long, conflicts_with = "session")]
    pub continue_session: bool,

    /// Resume a session by ID.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Working directory exposed to file and shell plugins.
    #[arg(long, value_name = "PATH")]
    pub cwd: Option<PathBuf>,
}

pub struct Config {
    pub manager: Arc<ConfigManager>,
    pub environment: Arc<AgentEnvironment>,
    pub catalog: Arc<ModelCatalog>,
    pub session: SessionChoice,
    pub cwd: PathBuf,
}

impl Config {
    pub async fn load(cli: Cli) -> Result<Self> {
        let cwd = cli.cwd.unwrap_or(env::current_dir()?).canonicalize()?;
        let directory = config_directory()?;
        fs::create_dir_all(&directory).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let environment = Arc::new(AgentEnvironment::load(&directory).await?);
        let offline = cli.offline
            || environment_truthy("URI_AGENT_OFFLINE")
            || environment_truthy("PI_OFFLINE");
        let catalog = Arc::new(ModelCatalog::load(&directory, offline).await?);
        let manager = Arc::new(
            ConfigManager::load(
                directory,
                &cwd,
                catalog.clone(),
                InvocationOverrides {
                    provider: cli.provider,
                    model: cli.model,
                    api_key: cli.api_key,
                    output_limit: cli.output_limit,
                    thinking: cli.thinking,
                },
            )
            .await?,
        );
        let session = if cli.continue_session {
            SessionChoice::Latest
        } else if let Some(id) = cli.session {
            SessionChoice::Existing(id)
        } else {
            SessionChoice::New
        };
        Ok(Self {
            manager,
            environment,
            catalog,
            session,
            cwd,
        })
    }
}

pub fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return text;
    };
    rest.strip_prefix(r"UNC\")
        .map(|unc| format!(r"\\{unc}"))
        .unwrap_or_else(|| rest.to_string())
}

/// Compare containment after stripping Windows verbatim prefixes.
///
/// `canonicalize` on Windows yields `\\?\C:\...` while callers often still
/// hold `C:\...`. Component `starts_with` rejects that pair without this step.
pub fn path_is_within(path: &Path, root: &Path) -> bool {
    Path::new(&display_path(path)).starts_with(Path::new(&display_path(root)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueSource {
    Default,
    Session,
    Global,
    Project,
    Environment(String),
    CommandLine,
    ModelsFile,
}

impl ValueSource {
    pub fn label(&self) -> String {
        match self {
            Self::Default => "default".to_string(),
            Self::Session => "session".to_string(),
            Self::Global => "settings.json".to_string(),
            Self::Project => ".uri-agent/settings.json".to_string(),
            Self::Environment(name) => name.clone(),
            Self::CommandLine => "command line".to_string(),
            Self::ModelsFile => "models.json".to_string(),
        }
    }

    pub fn externally_overridden(&self) -> bool {
        matches!(self, Self::Environment(_) | Self::CommandLine)
    }
}

#[derive(Clone)]
pub struct ActiveSettings {
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    pub auth_kind: AuthKind,
    pub output_limit: usize,
    pub thinking: ThinkingLevel,
    pub terminal: Option<String>,
    pub key_display: KeyDisplayStyle,
    pub compaction: compaction::Settings,
    pub provider_source: ValueSource,
    pub model_source: ValueSource,
    pub api_key_source: ValueSource,
    pub output_limit_source: ValueSource,
    pub thinking_source: ValueSource,
    pub terminal_source: ValueSource,
    pub credential_environment: BTreeMap<String, String>,
}

impl ActiveSettings {
    pub fn model_configured(&self) -> bool {
        !self.provider.trim().is_empty() && !self.model.trim().is_empty()
    }

    pub async fn catalog_model(&self, catalog: &ModelCatalog) -> Option<CatalogModel> {
        if !self.model_configured() {
            return None;
        }
        catalog.model(&self.provider, &self.model).await
    }
}

#[derive(Clone, Default)]
struct InvocationOverrides {
    provider: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    output_limit: Option<usize>,
    thinking: Option<ThinkingLevel>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
struct SettingsFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    default_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_thinking_level: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_thinking_levels: BTreeMap<String, ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_display: Option<KeyDisplayStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction: Option<CompactionFile>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
struct CompactionFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reserve_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_recent_tokens: Option<usize>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
struct AuthFile(BTreeMap<String, AuthEntry>);

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
struct EnvironmentFile(BTreeMap<String, String>);

impl EnvironmentFile {
    fn validate(&self) -> Result<()> {
        for (name, value) in &self.0 {
            validate_environment_name(name)?;
            validate_environment_value(value)?;
        }
        Ok(())
    }
}

pub struct AgentEnvironment {
    path: PathBuf,
    values: RwLock<EnvironmentFile>,
}

impl AgentEnvironment {
    pub async fn load(directory: &Path) -> Result<Self> {
        let path = directory.join("environment.json");
        let values: EnvironmentFile = read_json(&path).await?;
        values
            .validate()
            .with_context(|| format!("invalid Agent environment in {}", path.display()))?;
        if !path.exists() {
            write_json(&path, &values, true).await?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .await
                .with_context(|| format!("cannot secure {}", path.display()))?;
        }
        Ok(Self {
            path,
            values: RwLock::new(values),
        })
    }

    pub async fn names(&self) -> Vec<String> {
        self.values.read().await.0.keys().cloned().collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<String>> {
        validate_environment_name(name)?;
        Ok(self.values.read().await.0.get(name).cloned())
    }

    pub async fn snapshot(&self) -> BTreeMap<String, String> {
        self.values.read().await.0.clone()
    }

    pub async fn set(&self, name: &str, value: String) -> Result<()> {
        validate_environment_name(name)?;
        validate_environment_value(&value)?;
        let mut values = self.values.write().await;
        let mut next = values.clone();
        next.0.insert(name.to_string(), value);
        write_json(&self.path, &next, true).await?;
        *values = next;
        Ok(())
    }

    pub async fn remove(&self, name: &str) -> Result<bool> {
        validate_environment_name(name)?;
        let mut values = self.values.write().await;
        let mut next = values.clone();
        let removed = next.0.remove(name).is_some();
        if removed {
            write_json(&self.path, &next, true).await?;
            *values = next;
        }
        Ok(removed)
    }
}

pub fn validate_environment_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        || !characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        bail!("environment variable name must match [A-Za-z_][A-Za-z0-9_]*");
    }
    Ok(())
}

fn validate_environment_value(value: &str) -> Result<()> {
    if value.contains('\0') {
        bail!("environment variable value cannot contain NUL");
    }
    Ok(())
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct AuthEntry {
    #[serde(rename = "type", default = "api_key_type")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires: Option<i64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCredential {
    pub provider: String,
    pub kind: String,
}

fn api_key_type() -> String {
    "api_key".to_string()
}

struct ConfigFiles {
    global: SettingsFile,
    project: SettingsFile,
    auth: AuthFile,
}

pub struct ConfigManager {
    directory: PathBuf,
    project_path: PathBuf,
    catalog: Arc<ModelCatalog>,
    invocation: InvocationOverrides,
    files: Mutex<ConfigFiles>,
    active: RwLock<ActiveSettings>,
    oauth_refresh: Mutex<()>,
}

impl ConfigManager {
    async fn load(
        directory: PathBuf,
        cwd: &Path,
        catalog: Arc<ModelCatalog>,
        invocation: InvocationOverrides,
    ) -> Result<Self> {
        let global_path = directory.join("settings.json");
        let project_path = cwd.join(".uri-agent/settings.json");
        let auth_path = directory.join("auth.json");
        let mut global: SettingsFile = read_json(&global_path).await?;
        let project = read_json(&project_path).await?;
        let auth = read_json(&auth_path).await?;
        if !global_path.exists() {
            global.output_limit = Some(DEFAULT_OUTPUT_LIMIT);
            write_json(&global_path, &global, false).await?;
        }
        if !auth_path.exists() {
            write_json(&auth_path, &auth, true).await?;
        }
        let files = ConfigFiles {
            global,
            project,
            auth,
        };
        let active = calculate_active(&files, &catalog, &invocation).await?;
        Ok(Self {
            directory,
            project_path,
            catalog,
            invocation,
            files: Mutex::new(files),
            active: RwLock::new(active),
            oauth_refresh: Mutex::new(()),
        })
    }

    pub async fn current(&self) -> ActiveSettings {
        self.active.read().await.clone()
    }

    /// Resolve a provider API key outside the active model selection.
    ///
    /// This is used by provider-backed protocols such as web access. The
    /// provider-specific process environment variable takes precedence over a
    /// key saved in `auth.json`, matching active model credential resolution.
    pub async fn provider_api_key(&self, provider: &str) -> Result<Option<String>> {
        let files = self.files.lock().await;
        let entry = files.auth.0.get(provider);
        let credential_environment = entry.map(|entry| entry.env.clone()).unwrap_or_default();
        let mut api_key = entry
            .filter(|entry| entry.kind == "api_key")
            .and_then(|entry| entry.key.clone());
        let environment = api_key_environment(provider);
        if let Ok(value) = env::var(environment)
            && !value.trim().is_empty()
        {
            api_key = Some(value);
        }
        match api_key {
            Some(value) => Ok(Some(
                resolve_config_value(&value, &credential_environment).await?,
            )),
            None => Ok(None),
        }
    }

    #[cfg(test)]
    pub(crate) async fn load_for_test(directory: &Path, cwd: &Path) -> Result<Arc<Self>> {
        fs::create_dir_all(directory).await?;
        fs::create_dir_all(cwd).await?;
        let catalog = Arc::new(ModelCatalog::load(directory, true).await?);
        Ok(Arc::new(
            Self::load(
                directory.to_path_buf(),
                cwd,
                catalog,
                InvocationOverrides::default(),
            )
            .await?,
        ))
    }

    /// Resolve credentials and model metadata for settings frozen in a
    /// session, without changing the defaults used by new sessions.
    pub async fn for_session(
        &self,
        provider: &str,
        model: &str,
        thinking: ThinkingLevel,
    ) -> Result<ActiveSettings> {
        let files = self.files.lock().await;
        let mut invocation = self.invocation.clone();
        invocation.provider = Some(provider.to_string());
        invocation.model = Some(model.to_string());
        invocation.thinking = Some(thinking);
        let mut active = calculate_active(&files, &self.catalog, &invocation).await?;
        active.provider_source = ValueSource::Session;
        active.model_source = ValueSource::Session;
        active.thinking_source = ValueSource::Session;
        Ok(active)
    }

    pub async fn thinking_for_model(&self, provider: &str, model: &str) -> ThinkingLevel {
        let files = self.files.lock().await;
        let (configured, _) = configured_thinking(&files, provider, model);
        drop(files);
        let active = self.active.read().await;
        if active.thinking_source.externally_overridden() {
            active.thinking
        } else {
            configured
        }
    }

    pub async fn reload(&self) -> Result<ActiveSettings> {
        self.catalog.reload_user_overrides().await?;
        let mut files = self.files.lock().await;
        files.global = read_json(&self.settings_path()).await?;
        files.project = read_json(&self.project_path).await?;
        files.auth = read_json(&self.auth_path()).await?;
        self.recalculate(&files).await
    }

    pub async fn set_model(&self, provider: &str, model: &str) -> Result<ActiveSettings> {
        if self.catalog.model(provider, model).await.is_none() {
            bail!("model {provider}/{model} is not runnable in the current catalog");
        }
        let mut files = self.files.lock().await;
        let (settings, path) = if self.project_path.exists() {
            (&mut files.project, self.project_path.clone())
        } else {
            (&mut files.global, self.settings_path())
        };
        settings.default_provider = Some(provider.to_string());
        settings.default_model = Some(model.to_string());
        write_json(&path, settings, false).await?;
        self.recalculate(&files).await
    }

    pub async fn set_output_limit(&self, output_limit: usize) -> Result<ActiveSettings> {
        if output_limit < 1024 {
            bail!("output limit must be at least 1024 bytes");
        }
        let mut files = self.files.lock().await;
        let (settings, path) = if self.project_path.exists() {
            (&mut files.project, self.project_path.clone())
        } else {
            (&mut files.global, self.settings_path())
        };
        settings.output_limit = Some(output_limit);
        write_json(&path, settings, false).await?;
        self.recalculate(&files).await
    }

    pub async fn set_model_thinking(
        &self,
        provider: &str,
        model: &str,
        thinking: ThinkingLevel,
    ) -> Result<ActiveSettings> {
        if provider.trim().is_empty() || model.trim().is_empty() {
            bail!("provider and model are required to save thinking effort");
        }
        let mut files = self.files.lock().await;
        let (settings, path) = if self.project_path.exists() {
            (&mut files.project, self.project_path.clone())
        } else {
            (&mut files.global, self.settings_path())
        };
        settings
            .model_thinking_levels
            .insert(model_setting_key(provider, model), thinking);
        write_json(&path, settings, false).await?;
        self.recalculate(&files).await
    }

    pub async fn set_terminal(&self, terminal: Option<String>) -> Result<ActiveSettings> {
        let terminal = terminal.and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        });
        let mut files = self.files.lock().await;
        let (settings, path) = if self.project_path.exists() {
            (&mut files.project, self.project_path.clone())
        } else {
            (&mut files.global, self.settings_path())
        };
        settings.terminal = terminal;
        write_json(&path, settings, false).await?;
        self.recalculate(&files).await
    }

    pub async fn set_api_key(&self, provider: &str, key: String) -> Result<ActiveSettings> {
        if key.trim().is_empty() {
            bail!("API key cannot be empty");
        }
        let mut files = self.files.lock().await;
        files.auth.0.insert(
            provider.to_string(),
            AuthEntry {
                kind: api_key_type(),
                key: Some(key),
                refresh: None,
                access: None,
                expires: None,
                env: BTreeMap::new(),
                extra: BTreeMap::new(),
            },
        );
        write_json(&self.auth_path(), &files.auth, true).await?;
        self.recalculate(&files).await
    }

    pub async fn set_oauth(&self, provider: &str, token: OauthToken) -> Result<ActiveSettings> {
        let mut files = self.files.lock().await;
        files.auth.0.insert(
            provider.to_string(),
            AuthEntry {
                kind: "oauth".to_string(),
                key: None,
                refresh: Some(token.refresh),
                access: Some(token.access),
                expires: Some(token.expires),
                env: BTreeMap::new(),
                extra: token.extra,
            },
        );
        write_json(&self.auth_path(), &files.auth, true).await?;
        self.recalculate(&files).await
    }

    pub async fn oauth_token(&self, provider: &str) -> Result<OauthToken> {
        let files = self.files.lock().await;
        let entry = files
            .auth
            .0
            .get(provider)
            .filter(|entry| entry.kind == "oauth")
            .ok_or_else(|| anyhow!("{provider} OAuth credentials are not configured"))?;
        oauth_token_from_entry(provider, entry)
    }

    /// Refresh one OAuth entry without holding the configuration lock during
    /// network I/O. If another login or refresh wins the race, its newer
    /// credential is returned instead of being overwritten.
    pub async fn force_refresh_oauth(&self, provider: &str) -> Result<OauthToken> {
        self.refresh_oauth(provider, true).await
    }

    pub(crate) async fn resolve_model_api_key(
        &self,
        settings: &ActiveSettings,
    ) -> Result<Option<String>> {
        let value = if settings.auth_kind == AuthKind::Oauth
            && settings.api_key_source == ValueSource::Global
        {
            Some(self.refresh_oauth(&settings.provider, false).await?.access)
        } else {
            settings.api_key.clone()
        };
        match value {
            Some(value) => Ok(Some(
                resolve_config_value(&value, &settings.credential_environment).await?,
            )),
            None => Ok(None),
        }
    }

    async fn refresh_oauth(&self, provider: &str, force: bool) -> Result<OauthToken> {
        let _refresh = self.oauth_refresh.lock().await;
        let before = self.oauth_token(provider).await?;
        if !force && !before.expired() {
            return Ok(before);
        }
        let refreshed = oauth::refresh_token(provider, &before).await?;
        let mut files = self.files.lock().await;
        let current = files
            .auth
            .0
            .get(provider)
            .filter(|entry| entry.kind == "oauth")
            .ok_or_else(|| anyhow!("{provider} OAuth credentials were removed during refresh"))?;
        let current_token = oauth_token_from_entry(provider, current)?;
        if current_token.access != before.access
            || current_token.refresh != before.refresh
            || current_token.expires != before.expires
        {
            return Ok(current_token);
        }
        let entry = files
            .auth
            .0
            .get_mut(provider)
            .expect("OAuth entry checked above");
        entry.access = Some(refreshed.access.clone());
        entry.refresh = Some(refreshed.refresh.clone());
        entry.expires = Some(refreshed.expires);
        entry.extra.clone_from(&refreshed.extra);
        write_json(&self.auth_path(), &files.auth, true).await?;
        self.recalculate(&files).await?;
        Ok(refreshed)
    }

    pub async fn clear_api_key(&self, provider: &str) -> Result<ActiveSettings> {
        let mut files = self.files.lock().await;
        files.auth.0.remove(provider);
        write_json(&self.auth_path(), &files.auth, true).await?;
        self.recalculate(&files).await
    }

    pub async fn stored_credentials(&self) -> Vec<StoredCredential> {
        self.files
            .lock()
            .await
            .auth
            .0
            .iter()
            .map(|(provider, entry)| StoredCredential {
                provider: provider.clone(),
                kind: entry.kind.clone(),
            })
            .collect()
    }

    pub async fn refresh_credentials(&self) -> Result<ActiveSettings> {
        let providers = self
            .files
            .lock()
            .await
            .auth
            .0
            .iter()
            .filter(|(_, entry)| entry.kind == "oauth")
            .map(|(provider, _)| provider.clone())
            .collect::<Vec<_>>();
        for provider in providers {
            let _ = self.refresh_oauth(&provider, false).await;
        }
        Ok(self.current().await)
    }

    async fn recalculate(&self, files: &ConfigFiles) -> Result<ActiveSettings> {
        let active = calculate_active(files, &self.catalog, &self.invocation).await?;
        *self.active.write().await = active.clone();
        Ok(active)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn settings_path(&self) -> PathBuf {
        self.directory.join("settings.json")
    }

    pub fn auth_path(&self) -> PathBuf {
        self.directory.join("auth.json")
    }

    pub fn project_settings_path(&self) -> &Path {
        &self.project_path
    }
}

async fn calculate_active(
    files: &ConfigFiles,
    catalog: &ModelCatalog,
    invocation: &InvocationOverrides,
) -> Result<ActiveSettings> {
    let (mut provider, mut provider_source) = setting(
        String::new(),
        files.global.default_provider.clone(),
        files.project.default_provider.clone(),
    );
    let settings_provider = provider.clone();
    if let Ok(value) = env::var("URI_AGENT_PROVIDER")
        && !value.trim().is_empty()
    {
        provider = value;
        provider_source = ValueSource::Environment("URI_AGENT_PROVIDER".to_string());
    }
    if let Some(value) = &invocation.provider {
        provider.clone_from(value);
        provider_source = ValueSource::CommandLine;
    }

    let (mut model, mut model_source) = if provider == settings_provider {
        setting(
            String::new(),
            files.global.default_model.clone(),
            files.project.default_model.clone(),
        )
    } else {
        (String::new(), ValueSource::Default)
    };
    if let Ok(value) = env::var("URI_AGENT_MODEL")
        && !value.trim().is_empty()
    {
        model = value;
        model_source = ValueSource::Environment("URI_AGENT_MODEL".to_string());
    }
    if let Some(value) = &invocation.model {
        model.clone_from(value);
        model_source = ValueSource::CommandLine;
    }

    let (mut output_limit, mut output_limit_source) = setting(
        DEFAULT_OUTPUT_LIMIT,
        files.global.output_limit,
        files.project.output_limit,
    );
    if let Ok(value) = env::var("URI_AGENT_OUTPUT_LIMIT") {
        output_limit = value
            .parse()
            .context("URI_AGENT_OUTPUT_LIMIT must be a positive integer")?;
        output_limit_source = ValueSource::Environment("URI_AGENT_OUTPUT_LIMIT".to_string());
    }
    if let Some(value) = invocation.output_limit {
        output_limit = value;
        output_limit_source = ValueSource::CommandLine;
    }
    if output_limit < 1024 {
        bail!("output limit must be at least 1024 bytes");
    }

    let (mut thinking, mut thinking_source) = configured_thinking(files, &provider, &model);
    if let Ok(value) = env::var("URI_AGENT_THINKING")
        && !value.trim().is_empty()
    {
        thinking = value.parse().context("invalid URI_AGENT_THINKING")?;
        thinking_source = ValueSource::Environment("URI_AGENT_THINKING".to_string());
    }
    if let Some(value) = invocation.thinking {
        thinking = value;
        thinking_source = ValueSource::CommandLine;
    }

    let (mut terminal, mut terminal_source) = setting(
        String::new(),
        files.global.terminal.clone(),
        files.project.terminal.clone(),
    );
    if let Ok(value) = env::var("URI_AGENT_TERMINAL")
        && !value.trim().is_empty()
    {
        terminal = value;
        terminal_source = ValueSource::Environment("URI_AGENT_TERMINAL".to_string());
    }
    let terminal = (!terminal.trim().is_empty()).then_some(terminal.trim().to_string());

    let (mut key_display, _) = setting(
        KeyDisplayStyle::Auto,
        files.global.key_display,
        files.project.key_display,
    );
    if let Ok(value) = env::var("URI_AGENT_KEY_DISPLAY")
        && !value.trim().is_empty()
    {
        key_display = value.parse().context("invalid URI_AGENT_KEY_DISPLAY")?;
    }

    let compaction = compaction_settings(&files.global, &files.project)?;

    let configured_entry = files.auth.0.get(&provider);
    let credential_environment = configured_entry
        .map(|entry| entry.env.clone())
        .unwrap_or_default();
    let private_oauth = provider == "antigravity";
    let models_key = if private_oauth {
        None
    } else {
        catalog.configured_api_key(&provider).await
    };
    let (mut api_key, mut api_key_source, mut auth_kind) = match configured_entry {
        Some(entry) if entry.kind == "oauth" => {
            (entry.access.clone(), ValueSource::Global, AuthKind::Oauth)
        }
        Some(entry) if !private_oauth && entry.kind == "api_key" && entry.key.is_some() => {
            (entry.key.clone(), ValueSource::Global, AuthKind::ApiKey)
        }
        _ => (models_key, ValueSource::ModelsFile, AuthKind::None),
    };
    if auth_kind == AuthKind::None && api_key.is_some() {
        auth_kind = AuthKind::ApiKey;
    }
    if !private_oauth {
        let provider_environment = api_key_environment(&provider);
        let mut environments = vec![
            provider_environment.clone(),
            "URI_AGENT_API_KEY".to_string(),
        ];
        if provider == "anthropic" {
            environments.insert(0, "ANTHROPIC_OAUTH_TOKEN".to_string());
            environments.insert(1, "ANTHROPIC_AUTH_TOKEN".to_string());
        }
        for environment in environments {
            if let Ok(value) = env::var(&environment)
                && !value.trim().is_empty()
            {
                api_key = Some(value);
                api_key_source = ValueSource::Environment(environment.clone());
                auth_kind = if environment.contains("OAUTH") {
                    AuthKind::Oauth
                } else {
                    AuthKind::ApiKey
                };
            }
        }
        if let Some(value) = &invocation.api_key {
            api_key = Some(value.clone());
            api_key_source = ValueSource::CommandLine;
            auth_kind = AuthKind::ApiKey;
        }
    }
    if api_key.is_none() {
        auth_kind = AuthKind::None;
    }

    Ok(ActiveSettings {
        provider,
        model,
        api_key,
        auth_kind,
        output_limit,
        thinking,
        terminal,
        key_display,
        compaction,
        provider_source,
        model_source,
        api_key_source,
        output_limit_source,
        thinking_source,
        terminal_source,
        credential_environment,
    })
}

fn oauth_token_from_entry(provider: &str, entry: &AuthEntry) -> Result<OauthToken> {
    let refresh = entry
        .refresh
        .clone()
        .ok_or_else(|| anyhow!("{provider} OAuth credential has no refresh token"))?;
    let access = entry
        .access
        .clone()
        .ok_or_else(|| anyhow!("{provider} OAuth credential has no access token"))?;
    Ok(OauthToken {
        kind: "oauth".to_string(),
        refresh,
        access,
        expires: entry.expires.unwrap_or(0),
        extra: entry.extra.clone(),
    })
}

fn configured_thinking(
    files: &ConfigFiles,
    provider: &str,
    model: &str,
) -> (ThinkingLevel, ValueSource) {
    let (mut thinking, mut source) = setting(
        ThinkingLevel::Off,
        files.global.default_thinking_level,
        files.project.default_thinking_level,
    );
    let key = model_setting_key(provider, model);
    if let Some(value) = files.global.model_thinking_levels.get(&key) {
        thinking = *value;
        source = ValueSource::Global;
    }
    if let Some(value) = files.project.model_thinking_levels.get(&key) {
        thinking = *value;
        source = ValueSource::Project;
    }
    (thinking, source)
}

fn setting<T: Clone>(default: T, global: Option<T>, project: Option<T>) -> (T, ValueSource) {
    if let Some(value) = project {
        (value, ValueSource::Project)
    } else if let Some(value) = global {
        (value, ValueSource::Global)
    } else {
        (default, ValueSource::Default)
    }
}

fn compaction_settings(
    global: &SettingsFile,
    project: &SettingsFile,
) -> Result<compaction::Settings> {
    let global = global.compaction.as_ref();
    let project = project.compaction.as_ref();
    let settings = compaction::Settings {
        enabled: project
            .and_then(|settings| settings.enabled)
            .or_else(|| global.and_then(|settings| settings.enabled))
            .unwrap_or(true),
        reserve_tokens: project
            .and_then(|settings| settings.reserve_tokens)
            .or_else(|| global.and_then(|settings| settings.reserve_tokens))
            .unwrap_or(compaction::DEFAULT_RESERVE_TOKENS),
        keep_recent_tokens: project
            .and_then(|settings| settings.keep_recent_tokens)
            .or_else(|| global.and_then(|settings| settings.keep_recent_tokens))
            .unwrap_or(compaction::DEFAULT_KEEP_RECENT_TOKENS),
    };
    if settings.reserve_tokens == 0 || settings.keep_recent_tokens == 0 {
        bail!("compaction reserveTokens and keepRecentTokens must be greater than zero");
    }
    Ok(settings)
}

fn model_setting_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

static COMMAND_VALUE_CACHE: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();

pub(crate) async fn resolve_config_value(
    value: &str,
    environment: &BTreeMap<String, String>,
) -> Result<String> {
    if let Some(command) = value.strip_prefix('!') {
        let command = command.trim();
        if command.is_empty() {
            bail!("empty command in configuration value");
        }
        let cache = COMMAND_VALUE_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
        if let Some(value) = cache.lock().await.get(command).cloned() {
            return Ok(value);
        }
        #[cfg(windows)]
        let child = Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command", command])
            .output();
        #[cfg(not(windows))]
        let child = Command::new("sh").args(["-c", command]).output();
        let output = tokio::time::timeout(Duration::from_secs(10), child)
            .await
            .context("credential command timed out after 10 seconds")??;
        if !output.status.success() {
            bail!("credential command failed with {}", output.status);
        }
        let resolved = String::from_utf8(output.stdout)?.trim().to_string();
        if resolved.is_empty() {
            bail!("credential command returned an empty value");
        }
        cache
            .lock()
            .await
            .insert(command.to_string(), resolved.clone());
        return Ok(resolved);
    }
    interpolate_environment(value, environment)
}

fn interpolate_environment(value: &str, environment: &BTreeMap<String, String>) -> Result<String> {
    let mut result = String::new();
    let mut characters = value.char_indices().peekable();
    while let Some((_, character)) = characters.next() {
        if character != '$' {
            result.push(character);
            continue;
        }
        if characters.peek().is_some_and(|(_, next)| *next == '$') {
            characters.next();
            result.push('$');
            continue;
        }
        if characters.peek().is_some_and(|(_, next)| *next == '!') {
            characters.next();
            result.push('!');
            continue;
        }
        let braced = characters.peek().is_some_and(|(_, next)| *next == '{');
        if braced {
            characters.next();
        }
        let mut name = String::new();
        if !characters
            .peek()
            .is_some_and(|(_, next)| next.is_ascii_alphabetic() || *next == '_')
        {
            result.push('$');
            if braced {
                result.push('{');
            }
            continue;
        }
        while let Some((_, next)) = characters.peek() {
            if next.is_ascii_alphanumeric() || *next == '_' {
                name.push(*next);
                characters.next();
            } else {
                break;
            }
        }
        if braced {
            match characters.next() {
                Some((_, '}')) => {}
                _ => bail!("unterminated environment variable in configuration value"),
            }
        }
        if name.is_empty() {
            result.push('$');
        } else {
            let value = environment
                .get(&name)
                .filter(|value| !value.is_empty())
                .cloned()
                .or_else(|| env::var(&name).ok().filter(|value| !value.is_empty()))
                .with_context(|| format!("environment variable {name} is not set"))?;
            result.push_str(&value);
        }
    }
    Ok(result)
}

fn environment_truthy(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub fn config_directory() -> Result<PathBuf> {
    if let Ok(directory) = env::var("URI_AGENT_CONFIG_DIR")
        && !directory.trim().is_empty()
    {
        return Ok(PathBuf::from(directory));
    }
    dirs::config_dir()
        .map(|directory| directory.join("uri-agent"))
        .ok_or_else(|| anyhow!("cannot determine the platform config directory"))
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

async fn write_json<T>(path: &Path, value: &T, private: bool) -> Result<()>
where
    T: Serialize,
{
    #[cfg(not(unix))]
    let _ = private;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).await?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let temporary = parent.join(format!(".{filename}.{}.tmp", Uuid::now_v7().simple()));
    let mut content = serde_json::to_vec_pretty(value)?;
    content.push(b'\n');

    #[cfg(unix)]
    {
        let mode = if private { 0o600 } else { 0o644 };
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true).mode(mode);
        use tokio::io::AsyncWriteExt;
        let mut file = options.open(&temporary).await?;
        file.write_all(&content).await?;
        file.sync_all().await?;
    }
    #[cfg(not(unix))]
    fs::write(&temporary, content).await?;

    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).await?;
    }
    if let Err(error) = fs::rename(&temporary, path).await {
        let _ = fs::remove_file(&temporary).await;
        return Err(error).with_context(|| format!("cannot replace {}", path.display()));
    }
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_environment_persists_private_values_and_validates_names() {
        let directory = tempfile::tempdir().unwrap();
        let environment = AgentEnvironment::load(directory.path()).await.unwrap();
        #[cfg(unix)]
        let path = directory.path().join("environment.json");

        environment
            .set("NPM_TOKEN", "managed-secret".to_string())
            .await
            .unwrap();
        environment
            .set("SECOND_TOKEN", "second-secret".to_string())
            .await
            .unwrap();
        assert_eq!(
            environment.names().await,
            vec!["NPM_TOKEN".to_string(), "SECOND_TOKEN".to_string()]
        );
        assert_eq!(
            environment.get("NPM_TOKEN").await.unwrap().as_deref(),
            Some("managed-secret")
        );
        assert_eq!(environment.snapshot().await.len(), 2);
        assert!(environment.remove("NPM_TOKEN").await.unwrap());
        assert!(!environment.remove("NPM_TOKEN").await.unwrap());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .await
                .unwrap();
        }
        let reloaded = AgentEnvironment::load(directory.path()).await.unwrap();
        assert_eq!(reloaded.get("NPM_TOKEN").await.unwrap(), None);
        assert_eq!(
            reloaded.get("SECOND_TOKEN").await.unwrap().as_deref(),
            Some("second-secret")
        );
        assert!(reloaded.set("1INVALID", String::new()).await.is_err());
        assert!(reloaded.set("INVALID-NAME", String::new()).await.is_err());
        assert!(
            reloaded
                .set("VALID_NAME", "nul\0value".to_string())
                .await
                .is_err()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path).await.unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn agent_environment_rejects_invalid_persisted_entries() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("environment.json"),
            br#"{"INVALID-NAME":"secret"}"#,
        )
        .await
        .unwrap();

        let error = match AgentEnvironment::load(directory.path()).await {
            Ok(_) => panic!("invalid environment file was accepted"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("[A-Za-z_][A-Za-z0-9_]*"));
    }

    #[tokio::test]
    async fn agent_environment_does_not_expose_unmanaged_process_variables() {
        let directory = tempfile::tempdir().unwrap();
        let environment = AgentEnvironment::load(directory.path()).await.unwrap();
        let name = format!("URI_AGENT_UNMANAGED_ENV_TEST_{}", Uuid::now_v7().simple());
        // SAFETY: the process-unique variable is removed before this test returns.
        unsafe { env::set_var(&name, "process-secret") };

        assert_eq!(environment.get(&name).await.unwrap(), None);
        environment
            .set(&name, "managed-secret".to_string())
            .await
            .unwrap();
        assert_eq!(
            environment.get(&name).await.unwrap().as_deref(),
            Some("managed-secret")
        );

        // SAFETY: the process-unique variable is no longer used.
        unsafe { env::remove_var(&name) };
    }

    #[test]
    fn windows_verbatim_path_prefix_is_not_displayed() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\4fu\project")),
            r"C:\Users\4fu\project"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share")),
            r"\\server\share"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_verbatim_paths_compare_inside_their_root() {
        assert!(path_is_within(
            Path::new(r"\\?\C:\Users\4fu\project\screen.png"),
            Path::new(r"C:\Users\4fu\project"),
        ));
        assert!(path_is_within(
            Path::new(r"C:\Users\4fu\project\screen.png"),
            Path::new(r"\\?\C:\Users\4fu\project"),
        ));
        assert!(!path_is_within(
            Path::new(r"\\?\C:\Users\4fu\other\screen.png"),
            Path::new(r"C:\Users\4fu\project"),
        ));
        assert!(!path_is_within(
            Path::new(r"\\?\C:\Users\4fu\project-extra\screen.png"),
            Path::new(r"C:\Users\4fu\project"),
        ));
    }

    #[test]
    fn settings_file_uses_pi_thinking_fields() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "defaultProvider": "openai",
            "defaultModel": "gpt-5.2",
            "defaultThinkingLevel": "high",
            "modelThinkingLevels": {
                "openai/gpt-5.2": "medium"
            }
        }))
        .unwrap();
        let value = serde_json::to_value(settings).unwrap();
        assert_eq!(value["defaultThinkingLevel"], "high");
        assert_eq!(value["modelThinkingLevels"]["openai/gpt-5.2"], "medium");
    }

    #[tokio::test]
    async fn model_thinking_preferences_are_persisted_by_provider_and_model() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let project = root.path().join("project");
        fs::create_dir_all(&directory).await.unwrap();
        fs::create_dir_all(&project).await.unwrap();
        let catalog = Arc::new(ModelCatalog::load(&directory, true).await.unwrap());
        let manager = ConfigManager::load(
            directory.clone(),
            &project,
            catalog,
            InvocationOverrides::default(),
        )
        .await
        .unwrap();

        manager
            .set_model_thinking("openai", "gpt-5.2", ThinkingLevel::High)
            .await
            .unwrap();
        manager
            .set_model_thinking("anthropic", "claude-opus-4-6", ThinkingLevel::Medium)
            .await
            .unwrap();

        let saved: Value =
            serde_json::from_slice(&fs::read(directory.join("settings.json")).await.unwrap())
                .unwrap();
        assert_eq!(saved["modelThinkingLevels"]["openai/gpt-5.2"], "high");
        assert_eq!(
            saved["modelThinkingLevels"]["anthropic/claude-opus-4-6"],
            "medium"
        );
        assert_eq!(
            manager.thinking_for_model("openai", "gpt-5.2").await,
            ThinkingLevel::High
        );
        assert_eq!(
            manager
                .thinking_for_model("anthropic", "claude-opus-4-6")
                .await,
            ThinkingLevel::Medium
        );

        let files = manager.files.lock().await;
        let openai = calculate_active(
            &files,
            &manager.catalog,
            &InvocationOverrides {
                provider: Some("openai".to_string()),
                model: Some("gpt-5.2".to_string()),
                ..InvocationOverrides::default()
            },
        )
        .await
        .unwrap();
        let anthropic = calculate_active(
            &files,
            &manager.catalog,
            &InvocationOverrides {
                provider: Some("anthropic".to_string()),
                model: Some("claude-opus-4-6".to_string()),
                ..InvocationOverrides::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(openai.thinking, ThinkingLevel::High);
        assert_eq!(anthropic.thinking, ThinkingLevel::Medium);
        drop(files);

        let defaults = manager.current().await;
        let resumed = manager
            .for_session("anthropic", "claude-opus-4-6", ThinkingLevel::Low)
            .await
            .unwrap();
        assert_eq!(resumed.provider, "anthropic");
        assert_eq!(resumed.model, "claude-opus-4-6");
        assert_eq!(resumed.thinking, ThinkingLevel::Low);
        assert_eq!(resumed.provider_source, ValueSource::Session);
        assert_eq!(resumed.model_source, ValueSource::Session);
        assert_eq!(resumed.thinking_source, ValueSource::Session);
        assert_eq!(manager.current().await.provider, defaults.provider);
        assert_eq!(manager.current().await.model, defaults.model);
    }

    #[test]
    fn model_stays_unconfigured_without_saved_or_invoked_values() {
        let (provider, provider_source) = setting(String::new(), None::<String>, None);
        let (model, model_source) = setting(String::new(), None::<String>, None);
        assert!(provider.is_empty());
        assert!(model.is_empty());
        assert_eq!(provider_source, ValueSource::Default);
        assert_eq!(model_source, ValueSource::Default);
        let settings = ActiveSettings {
            provider,
            model,
            api_key: None,
            auth_kind: AuthKind::None,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            thinking: ThinkingLevel::Off,
            terminal: None,
            key_display: KeyDisplayStyle::Auto,
            compaction: compaction::Settings::default(),
            provider_source,
            model_source,
            api_key_source: ValueSource::Default,
            output_limit_source: ValueSource::Default,
            thinking_source: ValueSource::Default,
            terminal_source: ValueSource::Default,
            credential_environment: BTreeMap::new(),
        };
        assert!(!settings.model_configured());
    }

    #[test]
    fn terminal_command_is_a_settings_field() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "terminal": "pwsh -NoLogo"
        }))
        .unwrap();
        assert_eq!(settings.terminal.as_deref(), Some("pwsh -NoLogo"));
        assert_eq!(
            serde_json::to_value(settings).unwrap()["terminal"],
            "pwsh -NoLogo"
        );
    }

    #[test]
    fn key_display_is_a_settings_field() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "keyDisplay": "macos"
        }))
        .unwrap();
        assert_eq!(settings.key_display, Some(KeyDisplayStyle::Macos));
        assert_eq!(
            serde_json::to_value(settings).unwrap()["keyDisplay"],
            "macos"
        );
    }

    #[test]
    fn compaction_settings_merge_nested_global_and_project_fields() {
        let global: SettingsFile = serde_json::from_value(serde_json::json!({
            "compaction": {
                "enabled": false,
                "reserveTokens": 12_000,
                "keepRecentTokens": 18_000
            }
        }))
        .unwrap();
        let project: SettingsFile = serde_json::from_value(serde_json::json!({
            "compaction": {
                "enabled": true,
                "keepRecentTokens": 9_000
            }
        }))
        .unwrap();

        assert_eq!(
            compaction_settings(&global, &project).unwrap(),
            compaction::Settings {
                enabled: true,
                reserve_tokens: 12_000,
                keep_recent_tokens: 9_000,
            }
        );
        assert!(
            compaction_settings(&SettingsFile::default(), &SettingsFile::default())
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn unknown_settings_fields_are_preserved() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "defaultProvider": "openai",
            "editor": "nvim -f"
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(settings).unwrap()["editor"], "nvim -f");
    }

    #[test]
    fn oauth_auth_entries_round_trip() {
        let auth: AuthFile = serde_json::from_value(serde_json::json!({
            "anthropic": {
                "type": "oauth",
                "refresh": "refresh-token",
                "access": "access-token",
                "expires": 1
            }
        }))
        .unwrap();
        let entry = auth.0.get("anthropic").unwrap();
        assert_eq!(entry.kind, "oauth");
        assert_eq!(entry.access.as_deref(), Some("access-token"));
        assert_eq!(
            serde_json::to_value(&auth).unwrap()["anthropic"]["type"],
            "oauth"
        );
    }

    #[tokio::test]
    async fn antigravity_accepts_only_its_stored_oauth_credential() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let project = root.path().join("project");
        fs::create_dir_all(&directory).await.unwrap();
        fs::create_dir_all(&project).await.unwrap();
        fs::write(
            directory.join("models.json"),
            br#"{"providers":{"antigravity":{"apiKey":"models-key"}}}"#,
        )
        .await
        .unwrap();
        let catalog = Arc::new(ModelCatalog::load(&directory, true).await.unwrap());
        let manager = ConfigManager::load(
            directory,
            &project,
            catalog,
            InvocationOverrides {
                provider: Some("antigravity".to_string()),
                model: Some("gemini-3.1-pro-high".to_string()),
                api_key: Some("command-line-key".to_string()),
                ..InvocationOverrides::default()
            },
        )
        .await
        .unwrap();

        manager
            .set_api_key("antigravity", "stored-api-key".to_string())
            .await
            .unwrap();
        assert_eq!(manager.current().await.api_key, None);

        let token = OauthToken {
            kind: "oauth".to_string(),
            refresh: "refresh-token".to_string(),
            access: "oauth-access".to_string(),
            expires: i64::MAX,
            extra: BTreeMap::from([(
                "projectId".to_string(),
                Value::String("project-1".to_string()),
            )]),
        };
        let active = manager
            .set_oauth("antigravity", token.clone())
            .await
            .unwrap();
        assert_eq!(active.api_key.as_deref(), Some("oauth-access"));
        assert_eq!(active.auth_kind, AuthKind::Oauth);
        assert_eq!(manager.oauth_token("antigravity").await.unwrap(), token);
    }

    #[tokio::test]
    async fn provider_api_keys_resolve_saved_and_process_environment_credentials() {
        let root = tempfile::tempdir().unwrap();
        let manager = ConfigManager::load_for_test(root.path(), root.path())
            .await
            .unwrap();
        let provider = "uri-agent-credential-test";
        let environment = api_key_environment(provider);
        assert_eq!(api_key_environment("parallel"), "PARALLEL_API_KEY");

        manager
            .set_api_key(provider, "saved-key".to_string())
            .await
            .unwrap();
        assert_eq!(
            manager.provider_api_key(provider).await.unwrap().as_deref(),
            Some("saved-key")
        );

        // SAFETY: this test uses a process-unique variable and no other test reads it.
        unsafe { env::set_var(&environment, "process-key") };
        assert_eq!(
            manager.provider_api_key(provider).await.unwrap().as_deref(),
            Some("process-key")
        );
        // SAFETY: this process-unique variable is no longer used.
        unsafe { env::remove_var(environment) };
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn active_model_command_credential_is_resolved_only_when_requested() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let project = root.path().join("project");
        let marker = root.path().join("credential-command-ran");
        fs::create_dir_all(&directory).await.unwrap();
        fs::create_dir_all(&project).await.unwrap();
        let command = format!("!touch '{}'; printf lazy-secret", marker.to_string_lossy());
        let catalog = Arc::new(ModelCatalog::load(&directory, true).await.unwrap());
        let manager = ConfigManager::load(
            directory,
            &project,
            catalog,
            InvocationOverrides {
                provider: Some("example".to_string()),
                model: Some("example-model".to_string()),
                api_key: Some(command.clone()),
                ..InvocationOverrides::default()
            },
        )
        .await
        .unwrap();

        let active = manager.current().await;
        assert_eq!(active.api_key.as_deref(), Some(command.as_str()));
        assert!(!marker.exists());
        assert_eq!(
            manager
                .resolve_model_api_key(&active)
                .await
                .unwrap()
                .as_deref(),
            Some("lazy-secret")
        );
        assert!(marker.exists());
    }

    #[test]
    fn config_values_expand_environment_and_dollar_escaping() {
        // SAFETY: this test uses a process-unique variable and no other test reads it.
        unsafe { env::set_var("URI_AGENT_CONFIG_TEST_VALUE", "secret") };
        assert_eq!(
            interpolate_environment(
                "Bearer ${URI_AGENT_CONFIG_TEST_VALUE} $$5 $!literal",
                &BTreeMap::new(),
            )
            .unwrap(),
            "Bearer secret $5 !literal"
        );
        assert_eq!(
            interpolate_environment(
                "$URI_AGENT_CONFIG_TEST_VALUE",
                &BTreeMap::from([(
                    "URI_AGENT_CONFIG_TEST_VALUE".to_string(),
                    "explicit".to_string(),
                )]),
            )
            .unwrap(),
            "explicit"
        );
        // SAFETY: this process-unique variable is no longer used.
        unsafe { env::remove_var("URI_AGENT_CONFIG_TEST_VALUE") };
    }
}
