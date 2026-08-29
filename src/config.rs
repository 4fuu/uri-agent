use crate::catalog::{
    CatalogCredential, CatalogModel, CatalogRefreshReport, ModelCatalog, ThinkingLevel,
    api_key_environment, supports_live_discovery,
};
use crate::compaction;
use crate::keymap::KeyDisplayStyle;
use crate::oauth::{self, OauthToken};
#[cfg(windows)]
use crate::process::PWSH_STDIN_BOOTSTRAP;
use crate::process::ProcessTree;
use crate::session::SessionChoice;
use anyhow::{Context, Result, anyhow, bail};
#[cfg(windows)]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

const DEFAULT_OUTPUT_LIMIT: usize = 32 * 1024;
const CONFIG_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
pub const BUILTIN_MODEL_ROLES: [&str; 1] = ["small"];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AuthKind {
    #[default]
    None,
    ApiKey,
    Oauth,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRole {
    pub provider: String,
    pub model: String,
    pub thinking: ThinkingLevel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoleInfo {
    pub name: String,
    pub role: Option<ModelRole>,
    pub error: Option<String>,
    pub source: Option<ValueSource>,
    pub overrides_global: bool,
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

    /// Disable cloud and provider model-catalog requests and use the local cache only.
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
        migrate_legacy_macos_config(&directory).await?;
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

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_roles: BTreeMap<String, ModelRoleConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    plugin_settings: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_display: Option<KeyDisplayStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction: Option<CompactionFile>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelRoleConfig {
    provider: String,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingLevel>,
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
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
            .with_context(|| format!("invalid Agent environment in {}", display_path(&path)))?;
        if !path.exists() {
            write_json(&path, &values, true).await?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .await
                .with_context(|| format!("cannot secure {}", display_path(&path)))?;
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

struct ResolvedModelCredential {
    api_key: Option<String>,
    source: ValueSource,
    kind: AuthKind,
    environment: BTreeMap<String, String>,
}

struct DiscoveryCredentialCandidate {
    provider: String,
    value: String,
    oauth: bool,
    environment: BTreeMap<String, String>,
}

pub struct ConfigManager {
    directory: PathBuf,
    project_path: PathBuf,
    catalog: Arc<ModelCatalog>,
    invocation: InvocationOverrides,
    files: Mutex<ConfigFiles>,
    active: RwLock<ActiveSettings>,
    auth_update: Mutex<()>,
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
        let auth_lock = lock_auth_file(&directory).await?;
        let auth = read_json(&auth_path).await?;
        if !global_path.exists() {
            global.output_limit = Some(DEFAULT_OUTPUT_LIMIT);
            write_json(&global_path, &global, false).await?;
        }
        if !auth_path.exists() {
            write_json(&auth_path, &auth, true).await?;
        }
        drop(auth_lock);
        let files = ConfigFiles {
            global,
            project,
            auth,
        };
        let candidates = discovery_credential_candidates(&files, &catalog, &invocation).await;
        let credentials = resolve_discovery_credentials(candidates, false).await;
        catalog.activate_discovery(&credentials).await;
        let active = calculate_active(&files, &catalog, &invocation).await?;
        Ok(Self {
            directory,
            project_path,
            catalog,
            invocation,
            files: Mutex::new(files),
            active: RwLock::new(active),
            auth_update: Mutex::new(()),
        })
    }

    pub async fn current(&self) -> ActiveSettings {
        self.active.read().await.clone()
    }

    /// Resolve a configured model role for a linked or WASM plugin. Project
    /// settings override a same-named global role. Role lookup is dynamic and
    /// does not change the active conversation model.
    pub async fn model_role(&self, name: &str) -> Result<Option<ModelRole>> {
        validate_model_role_name(name)?;
        let configured = {
            let files = self.files.lock().await;
            files
                .project
                .model_roles
                .get(name)
                .or_else(|| files.global.model_roles.get(name))
                .cloned()
        };
        let Some(configured) = configured else {
            return Ok(None);
        };
        if configured.provider.trim().is_empty() || configured.model.trim().is_empty() {
            bail!("model role {name:?} requires nonempty provider and model values");
        }
        let thinking = if let Some(thinking) = configured.thinking {
            thinking
        } else {
            let files = self.files.lock().await;
            configured_thinking(&files, &configured.provider, &configured.model).0
        };
        let role = ModelRole {
            provider: configured.provider,
            model: configured.model,
            thinking,
        };
        if self
            .catalog
            .model(&role.provider, &role.model)
            .await
            .is_none()
        {
            bail!(
                "model role {name:?} selects unavailable model {}/{}",
                role.provider,
                role.model
            );
        }
        Ok(Some(role))
    }

    pub async fn model_roles(&self) -> Result<Vec<ModelRoleInfo>> {
        let (custom, sources) = {
            let files = self.files.lock().await;
            let mut custom = BTreeSet::new();
            custom.extend(files.global.model_roles.keys().cloned());
            custom.extend(files.project.model_roles.keys().cloned());
            for builtin in BUILTIN_MODEL_ROLES {
                custom.remove(builtin);
            }
            let mut sources = BTreeMap::new();
            for name in files.global.model_roles.keys() {
                sources.insert(name.clone(), (ValueSource::Global, false));
            }
            for name in files.project.model_roles.keys() {
                sources.insert(
                    name.clone(),
                    (
                        ValueSource::Project,
                        files.global.model_roles.contains_key(name),
                    ),
                );
            }
            (custom, sources)
        };
        let mut names = BUILTIN_MODEL_ROLES
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        names.extend(custom);
        let mut roles = Vec::with_capacity(names.len());
        for name in names {
            let (role, error) = match self.model_role(&name).await {
                Ok(role) => (role, None),
                Err(error) => (None, Some(error.to_string())),
            };
            let (source, overrides_global) = sources
                .get(&name)
                .cloned()
                .map_or((None, false), |(source, overrides)| {
                    (Some(source), overrides)
                });
            roles.push(ModelRoleInfo {
                role,
                error,
                name,
                source,
                overrides_global,
            });
        }
        Ok(roles)
    }

    /// Read one project-overridable value from a plugin-owned settings
    /// namespace. Plugin settings are independent from model-role routes.
    pub async fn plugin_setting(&self, plugin: &str, key: &str) -> Result<Option<Value>> {
        validate_plugin_setting_name("plugin", plugin)?;
        validate_plugin_setting_name("plugin setting key", key)?;
        let files = self.files.lock().await;
        Ok(files
            .project
            .plugin_settings
            .get(plugin)
            .and_then(|settings| settings.get(key))
            .or_else(|| {
                files
                    .global
                    .plugin_settings
                    .get(plugin)
                    .and_then(|settings| settings.get(key))
            })
            .cloned())
    }

    /// Persist one value in a plugin-owned settings namespace. The value is
    /// written to the project file when that file already exists, matching
    /// other interactive settings.
    pub async fn set_plugin_setting(&self, plugin: &str, key: &str, value: Value) -> Result<()> {
        validate_plugin_setting_name("plugin", plugin)?;
        validate_plugin_setting_name("plugin setting key", key)?;
        if serde_json::to_vec(&value)?.len() > 1024 * 1024 {
            bail!("plugin setting value exceeds 1 MiB");
        }
        let mut files = self.files.lock().await;
        let (settings, path) = if self.project_path.exists() {
            (&mut files.project, self.project_path.clone())
        } else {
            (&mut files.global, self.settings_path())
        };
        settings
            .plugin_settings
            .entry(plugin.to_string())
            .or_default()
            .insert(key.to_string(), value);
        write_json(&path, settings, false).await
    }

    pub async fn remove_plugin_setting(&self, plugin: &str, key: &str) -> Result<bool> {
        validate_plugin_setting_name("plugin", plugin)?;
        validate_plugin_setting_name("plugin setting key", key)?;
        let mut files = self.files.lock().await;
        let (settings, path) = if files
            .project
            .plugin_settings
            .get(plugin)
            .is_some_and(|settings| settings.contains_key(key))
        {
            (&mut files.project, self.project_path.clone())
        } else if files
            .global
            .plugin_settings
            .get(plugin)
            .is_some_and(|settings| settings.contains_key(key))
        {
            (&mut files.global, self.settings_path())
        } else {
            return Ok(false);
        };
        let plugin_is_empty =
            settings
                .plugin_settings
                .get_mut(plugin)
                .is_some_and(|plugin_settings| {
                    plugin_settings.remove(key);
                    plugin_settings.is_empty()
                });
        if plugin_is_empty {
            settings.plugin_settings.remove(plugin);
        }
        write_json(&path, settings, false).await?;
        Ok(true)
    }

    /// Return catalog providers that currently have a configured model
    /// credential source. Values are not expanded and OAuth is not refreshed
    /// while building a model-selection list.
    pub async fn model_providers_with_credentials(
        &self,
        current_provider: &str,
    ) -> BTreeSet<String> {
        let providers = self.catalog.providers().await;
        let files = self.files.lock().await;
        let mut configured = BTreeSet::new();
        for provider in providers {
            let include_generic_overrides = provider == current_provider;
            if resolve_model_credential(
                &files,
                &self.catalog,
                &self.invocation,
                &provider,
                include_generic_overrides,
            )
            .await
            .api_key
            .is_some()
            {
                configured.insert(provider);
            }
        }
        configured
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

    pub(crate) async fn for_model_role(
        &self,
        name: &str,
    ) -> Result<Option<(ModelRole, ActiveSettings)>> {
        let Some(role) = self.model_role(name).await? else {
            return Ok(None);
        };
        let current_provider = self.active.read().await.provider.clone();
        let files = self.files.lock().await;
        let mut invocation = self.invocation.clone();
        invocation.provider = Some(role.provider.clone());
        invocation.model = Some(role.model.clone());
        invocation.thinking = Some(role.thinking);
        let mut active = calculate_active(&files, &self.catalog, &invocation).await?;
        if role.provider != current_provider {
            let credential = resolve_model_credential(
                &files,
                &self.catalog,
                &self.invocation,
                &role.provider,
                false,
            )
            .await;
            active.api_key = credential.api_key;
            active.api_key_source = credential.source;
            active.auth_kind = credential.kind;
            active.credential_environment = credential.environment;
        }
        active.provider_source = ValueSource::Global;
        active.model_source = ValueSource::Global;
        active.thinking_source = ValueSource::Global;
        Ok(Some((role, active)))
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

    pub async fn refresh_catalog(&self, force: bool) -> Result<CatalogRefreshReport> {
        let candidates = {
            let files = self.files.lock().await;
            discovery_credential_candidates(&files, &self.catalog, &self.invocation).await
        };
        let credentials = resolve_discovery_credentials(candidates, force).await;
        let report = self.catalog.refresh(force, &credentials).await?;
        self.reload().await?;
        Ok(report)
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

    pub async fn set_model_role(
        &self,
        name: &str,
        provider: &str,
        model: &str,
        thinking: ThinkingLevel,
    ) -> Result<()> {
        validate_model_role_name(name)?;
        let Some(catalog_model) = self.catalog.model(provider, model).await else {
            bail!("model {provider}/{model} is not runnable in the current catalog");
        };
        if !catalog_model.supports_thinking_level(thinking) {
            bail!("model {provider}/{model} does not support thinking effort {thinking}");
        }
        let mut files = self.files.lock().await;
        let (settings, path) = if self.project_path.exists() {
            (&mut files.project, self.project_path.clone())
        } else {
            (&mut files.global, self.settings_path())
        };
        settings.model_roles.insert(
            name.to_string(),
            ModelRoleConfig {
                provider: provider.to_string(),
                model: model.to_string(),
                thinking: Some(thinking),
            },
        );
        write_json(&path, settings, false).await
    }

    pub async fn remove_model_role(&self, name: &str) -> Result<bool> {
        validate_model_role_name(name)?;
        let mut files = self.files.lock().await;
        let (settings, path) = if files.project.model_roles.contains_key(name) {
            (&mut files.project, self.project_path.clone())
        } else if files.global.model_roles.contains_key(name) {
            (&mut files.global, self.settings_path())
        } else {
            return Ok(false);
        };
        settings.model_roles.remove(name);
        write_json(&path, settings, false).await?;
        Ok(true)
    }

    /// Remove persisted default model selections that resolve through
    /// `provider`. Per-model thinking preferences are retained for a future
    /// login. The caller owns any current session selection.
    pub async fn clear_model_selection_for_provider(&self, provider: &str) -> Result<bool> {
        let mut files = self.files.lock().await;
        let mut global = files.global.clone();
        let mut project = files.project.clone();
        let global_selected = global.default_provider.as_deref() == Some(provider);
        let project_selected = project.default_provider.as_deref() == Some(provider);
        let project_model_inherits_provider = project.default_provider.is_none()
            && global_selected
            && project.default_model.is_some();

        if global_selected {
            global.default_provider = None;
            global.default_model = None;
        }
        if project_selected {
            project.default_provider = None;
            project.default_model = None;
        } else if project_model_inherits_provider {
            project.default_model = None;
        }

        let global_changed = global != files.global;
        let project_changed = project != files.project;
        if !global_changed && !project_changed {
            return Ok(false);
        }

        let write_result = async {
            if global_changed {
                write_json(&self.settings_path(), &global, false).await?;
            }
            if project_changed {
                write_json(&self.project_path, &project, false).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = write_result {
            files.global = read_json(&self.settings_path()).await?;
            files.project = read_json(&self.project_path).await?;
            self.recalculate(&files).await?;
            return Err(error);
        }
        files.global = global;
        files.project = project;
        self.recalculate(&files).await?;
        Ok(true)
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
        let provider = provider.to_string();
        self.update_auth(move |auth| {
            auth.0.insert(
                provider,
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
            Ok(())
        })
        .await
    }

    pub async fn set_oauth(&self, provider: &str, token: OauthToken) -> Result<ActiveSettings> {
        let provider = provider.to_string();
        self.update_auth(move |auth| {
            auth.0.insert(
                provider,
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
            Ok(())
        })
        .await
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

    /// Refresh one OAuth entry while serializing credential rotation across
    /// processes. If another login or refresh wins the race, its newer
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
        let observed = self.oauth_token(provider).await?;
        if !force && !observed.expired() {
            return Ok(observed);
        }

        let _update = self.auth_update.lock().await;
        let _file_lock = lock_auth_file(&self.directory).await?;
        let mut auth: AuthFile = read_json(&self.auth_path()).await?;
        let current = auth
            .0
            .get(provider)
            .filter(|entry| entry.kind == "oauth")
            .ok_or_else(|| anyhow!("{provider} OAuth credentials were removed during refresh"))?;
        let current_token = oauth_token_from_entry(provider, current)?;
        if current_token != observed || (!force && !current_token.expired()) {
            self.install_auth(auth).await?;
            return Ok(current_token);
        }

        let refreshed = oauth::refresh_token(provider, &current_token).await?;
        let entry = auth.0.get_mut(provider).expect("OAuth entry checked above");
        entry.access = Some(refreshed.access.clone());
        entry.refresh = Some(refreshed.refresh.clone());
        entry.expires = Some(refreshed.expires);
        entry.extra.clone_from(&refreshed.extra);
        write_json(&self.auth_path(), &auth, true).await?;
        self.install_auth(auth).await?;
        Ok(refreshed)
    }

    pub async fn clear_api_key(&self, provider: &str) -> Result<ActiveSettings> {
        let provider = provider.to_string();
        self.update_auth(move |auth| {
            auth.0.remove(&provider);
            Ok(())
        })
        .await
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
        let candidates =
            discovery_credential_candidates(files, &self.catalog, &self.invocation).await;
        let credentials = resolve_discovery_credentials(candidates, false).await;
        self.catalog.activate_discovery(&credentials).await;
        let active = calculate_active(files, &self.catalog, &self.invocation).await?;
        *self.active.write().await = active.clone();
        Ok(active)
    }

    async fn update_auth(
        &self,
        update: impl FnOnce(&mut AuthFile) -> Result<()>,
    ) -> Result<ActiveSettings> {
        let _update = self.auth_update.lock().await;
        let _file_lock = lock_auth_file(&self.directory).await?;
        let mut auth = read_json(&self.auth_path()).await?;
        update(&mut auth)?;
        write_json(&self.auth_path(), &auth, true).await?;
        self.install_auth(auth).await
    }

    async fn install_auth(&self, auth: AuthFile) -> Result<ActiveSettings> {
        let mut files = self.files.lock().await;
        files.auth = auth;
        self.recalculate(&files).await
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn catalog(&self) -> Arc<ModelCatalog> {
        self.catalog.clone()
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

async fn discovery_credential_candidates(
    files: &ConfigFiles,
    catalog: &ModelCatalog,
    invocation: &InvocationOverrides,
) -> Vec<DiscoveryCredentialCandidate> {
    let current_provider = selected_provider(files, invocation).0;
    let providers = catalog.providers().await;
    let mut candidates = Vec::new();
    for provider in providers {
        if !supports_live_discovery(&provider) {
            continue;
        }
        let credential = resolve_model_credential(
            files,
            catalog,
            invocation,
            &provider,
            provider == current_provider,
        )
        .await;
        let Some(value) = credential.api_key else {
            continue;
        };
        candidates.push(DiscoveryCredentialCandidate {
            provider,
            value,
            oauth: credential.kind == AuthKind::Oauth,
            environment: credential.environment,
        });
    }
    candidates
}

async fn resolve_discovery_credentials(
    candidates: Vec<DiscoveryCredentialCandidate>,
    allow_commands: bool,
) -> BTreeMap<String, CatalogCredential> {
    let mut credentials = BTreeMap::new();
    for candidate in candidates {
        let resolution = if !allow_commands {
            let value = candidate.value.trim_start();
            if let Some(command) = value.strip_prefix('!') {
                let command = command.trim();
                let Some(cache) = COMMAND_VALUE_CACHE.get() else {
                    continue;
                };
                let Some(secret) = cache.lock().await.get(command).cloned() else {
                    continue;
                };
                Ok(secret)
            } else {
                resolve_config_value(&candidate.value, &candidate.environment).await
            }
        } else {
            resolve_config_value(&candidate.value, &candidate.environment).await
        };
        if let Ok(secret) = resolution
            && !secret.trim().is_empty()
        {
            credentials.insert(
                candidate.provider,
                CatalogCredential {
                    secret,
                    oauth: candidate.oauth,
                },
            );
        }
    }
    credentials
}

fn selected_provider(
    files: &ConfigFiles,
    invocation: &InvocationOverrides,
) -> (String, ValueSource, String) {
    let (mut provider, mut source) = setting(
        String::new(),
        files.global.default_provider.clone(),
        files.project.default_provider.clone(),
    );
    let settings_provider = provider.clone();
    if let Ok(value) = env::var("URI_AGENT_PROVIDER")
        && !value.trim().is_empty()
    {
        provider = value;
        source = ValueSource::Environment("URI_AGENT_PROVIDER".to_string());
    }
    if let Some(value) = &invocation.provider {
        provider.clone_from(value);
        source = ValueSource::CommandLine;
    }
    (provider, source, settings_provider)
}

async fn calculate_active(
    files: &ConfigFiles,
    catalog: &ModelCatalog,
    invocation: &InvocationOverrides,
) -> Result<ActiveSettings> {
    let (provider, provider_source, settings_provider) = selected_provider(files, invocation);

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

    let credential = resolve_model_credential(files, catalog, invocation, &provider, true).await;

    Ok(ActiveSettings {
        provider,
        model,
        api_key: credential.api_key,
        auth_kind: credential.kind,
        output_limit,
        thinking,
        terminal,
        key_display,
        compaction,
        provider_source,
        model_source,
        api_key_source: credential.source,
        output_limit_source,
        thinking_source,
        terminal_source,
        credential_environment: credential.environment,
    })
}

async fn resolve_model_credential(
    files: &ConfigFiles,
    catalog: &ModelCatalog,
    invocation: &InvocationOverrides,
    provider: &str,
    include_generic_overrides: bool,
) -> ResolvedModelCredential {
    let configured_entry = files.auth.0.get(provider);
    let environment = configured_entry
        .map(|entry| entry.env.clone())
        .unwrap_or_default();
    let private_oauth = provider == "antigravity";
    let models_key = if private_oauth {
        None
    } else {
        catalog.configured_api_key(provider).await
    };
    let (mut api_key, mut source, mut kind) = match configured_entry {
        Some(entry) if entry.kind == "oauth" => {
            (entry.access.clone(), ValueSource::Global, AuthKind::Oauth)
        }
        Some(entry) if !private_oauth && entry.kind == "api_key" && entry.key.is_some() => {
            (entry.key.clone(), ValueSource::Global, AuthKind::ApiKey)
        }
        _ => (models_key, ValueSource::ModelsFile, AuthKind::None),
    };
    if kind == AuthKind::None && api_key.is_some() {
        kind = AuthKind::ApiKey;
    }
    if !private_oauth {
        let mut environments = vec![api_key_environment(provider)];
        if provider == "anthropic" {
            environments.insert(0, "ANTHROPIC_OAUTH_TOKEN".to_string());
            environments.insert(1, "ANTHROPIC_AUTH_TOKEN".to_string());
        }
        for name in environments {
            if let Ok(value) = env::var(&name)
                && !value.trim().is_empty()
            {
                api_key = Some(value);
                source = ValueSource::Environment(name.clone());
                kind = if name.contains("OAUTH") {
                    AuthKind::Oauth
                } else {
                    AuthKind::ApiKey
                };
            }
        }
        if include_generic_overrides {
            if let Ok(value) = env::var("URI_AGENT_API_KEY")
                && !value.trim().is_empty()
            {
                api_key = Some(value);
                source = ValueSource::Environment("URI_AGENT_API_KEY".to_string());
                kind = AuthKind::ApiKey;
            }
            if let Some(value) = &invocation.api_key {
                api_key = Some(value.clone());
                source = ValueSource::CommandLine;
                kind = AuthKind::ApiKey;
            }
        }
    }
    if api_key.is_none() {
        kind = AuthKind::None;
    }
    ResolvedModelCredential {
        api_key,
        source,
        kind,
        environment,
    }
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

pub fn validate_model_role_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid model role name {name:?}; use ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

fn validate_plugin_setting_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name
            .chars()
            .any(|character| character.is_control() || matches!(character, '.' | '/' | '\\'))
    {
        bail!("invalid {kind} {name:?}");
    }
    Ok(())
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
        let output = execute_config_command(command, CONFIG_COMMAND_TIMEOUT)
            .await?
            .context("credential command timed out after 10 seconds")?;
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

async fn execute_config_command(command: &str, timeout: Duration) -> Result<Option<Output>> {
    #[cfg(windows)]
    let (mut process, input) = {
        let mut process = Command::new("pwsh");
        process.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            PWSH_STDIN_BOOTSTRAP,
        ]);
        (process, Some(BASE64.encode(command)))
    };
    #[cfg(not(windows))]
    let (mut process, input) = {
        let mut process = Command::new("sh");
        process.args(["-c", command]);
        (process, None::<String>)
    };
    if input.is_some() {
        process.stdin(Stdio::piped());
    } else {
        process.stdin(Stdio::null());
    }
    process.stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut child, process_tree) = ProcessTree::spawn(&mut process)?;
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow!("credential command timeout is too large"))?;
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open credential command stdin"))?;
        let write = tokio::time::timeout_at(deadline, stdin.write_all(input.as_bytes())).await;
        drop(stdin);
        match write {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                process_tree.terminate_and_wait(&mut child).await?;
                return Err(error.into());
            }
            Err(_) => {
                process_tree.terminate_and_wait(&mut child).await?;
                return Ok(None);
            }
        }
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to open credential command stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to open credential command stderr"))?;
    let completion = async {
        let wait = async {
            let status = child.wait().await?;
            process_tree.terminate();
            Ok::<_, std::io::Error>(status)
        };
        let (status, stdout, stderr) = tokio::try_join!(
            wait,
            read_config_command_output(stdout),
            read_config_command_output(stderr),
        )?;
        Ok::<_, std::io::Error>(Output {
            status,
            stdout,
            stderr,
        })
    };
    match tokio::time::timeout_at(deadline, completion).await {
        Ok(output) => Ok(Some(output?)),
        Err(_) => {
            process_tree.terminate_and_wait(&mut child).await?;
            Ok(None)
        }
    }
}

async fn read_config_command_output(
    mut output: impl AsyncRead + Unpin,
) -> std::io::Result<Vec<u8>> {
    let mut content = Vec::new();
    output.read_to_end(&mut content).await?;
    Ok(content)
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

const CONFIG_DIRECTORY_NAME: &str = "uri-agent";

pub fn config_directory() -> Result<PathBuf> {
    if let Some(directory) = overridden_config_directory() {
        return Ok(directory);
    }
    default_config_directory_from(
        dirs::home_dir().as_deref(),
        dirs::config_dir().as_deref(),
        cfg!(target_os = "macos"),
    )
}

fn overridden_config_directory() -> Option<PathBuf> {
    env::var("URI_AGENT_CONFIG_DIR")
        .ok()
        .filter(|directory| !directory.trim().is_empty())
        .map(PathBuf::from)
}

fn default_config_directory_from(
    home_dir: Option<&Path>,
    platform_config_dir: Option<&Path>,
    use_home_config: bool,
) -> Result<PathBuf> {
    let base = if use_home_config {
        home_dir.map(|home| home.join(".config"))
    } else {
        platform_config_dir.map(Path::to_path_buf)
    };
    base.map(|directory| directory.join(CONFIG_DIRECTORY_NAME))
        .ok_or_else(|| anyhow!("cannot determine the platform config directory"))
}

async fn migrate_legacy_macos_config(new_directory: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") || overridden_config_directory().is_some() {
        return Ok(());
    }
    let Some(old_directory) =
        dirs::config_dir().map(|directory| directory.join(CONFIG_DIRECTORY_NAME))
    else {
        return Ok(());
    };
    migrate_legacy_directory(&old_directory, new_directory)
        .await
        .with_context(|| {
            format!(
                "cannot migrate macOS data from {} to {}",
                display_path(&old_directory),
                display_path(new_directory)
            )
        })
}

async fn migrate_legacy_directory(old_directory: &Path, new_directory: &Path) -> Result<()> {
    let old_directory = old_directory.to_path_buf();
    let new_directory = new_directory.to_path_buf();
    tokio::task::spawn_blocking(move || {
        migrate_legacy_directory_sync(&old_directory, &new_directory)
    })
    .await
    .context("configuration migration worker failed")?
}

fn migrate_legacy_directory_sync(old_directory: &Path, new_directory: &Path) -> Result<()> {
    if !old_directory.is_dir() || config_directories_match(old_directory, new_directory) {
        return Ok(());
    }

    if !new_directory.exists() {
        if let Some(parent) = new_directory.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "cannot create configuration directory: {}",
                    display_path(parent)
                )
            })?;
        }
        if std::fs::rename(old_directory, new_directory).is_ok() {
            return Ok(());
        }
    }

    std::fs::create_dir_all(new_directory).with_context(|| {
        format!(
            "cannot create configuration directory: {}",
            display_path(new_directory)
        )
    })?;
    merge_directory(old_directory, new_directory)?;
    remove_config_entry(old_directory)
}

fn merge_directory(from: &Path, to: &Path) -> Result<()> {
    for entry in
        std::fs::read_dir(from).with_context(|| format!("cannot read {}", display_path(from)))?
    {
        let entry = entry.with_context(|| format!("cannot read {}", display_path(from)))?;
        let name = entry.file_name();
        let source = from.join(&name);
        let dest = to.join(name);
        if source.is_dir() && dest.is_dir() {
            merge_directory(&source, &dest)?;
            continue;
        }
        if dest.exists() {
            continue;
        }
        move_config_entry(&source, &dest)?;
    }
    Ok(())
}

fn config_directories_match(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn move_config_entry(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        return Ok(());
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if !from.exists() || to.exists() {
        return Ok(());
    }
    copy_config_entry(from, to)?;
    remove_config_entry(from)
}

fn copy_config_entry(from: &Path, to: &Path) -> Result<()> {
    let metadata = std::fs::metadata(from)
        .with_context(|| format!("cannot inspect {}", display_path(from)))?;
    if metadata.is_dir() {
        std::fs::create_dir_all(to)
            .with_context(|| format!("cannot create {}", display_path(to)))?;
        for entry in std::fs::read_dir(from)
            .with_context(|| format!("cannot read {}", display_path(from)))?
        {
            let entry = entry.with_context(|| format!("cannot read {}", display_path(from)))?;
            let name = entry.file_name();
            copy_config_entry(&from.join(&name), &to.join(name))?;
        }
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", display_path(parent)))?;
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .with_context(|| format!("cannot copy {} to {}", display_path(from), display_path(to)))
}

fn remove_config_entry(path: &Path) -> Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", display_path(path)));
        }
    };
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .with_context(|| format!("cannot remove {}", display_path(path)))
}

async fn lock_auth_file(directory: &Path) -> Result<std::fs::File> {
    fs::create_dir_all(directory).await?;
    let path = directory.join("auth.json.lock");
    tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).with_context(|| {
            format!("cannot open OAuth credential lock {}", display_path(&path))
        })?;
        file.lock_exclusive()
            .with_context(|| format!("cannot lock OAuth credentials {}", display_path(&path)))?;
        Ok(file)
    })
    .await
    .context("OAuth credential lock worker failed")?
}

async fn read_json<T>(path: &Path) -> Result<T>
where
    T: Default + for<'de> Deserialize<'de>,
{
    match fs::read(path).await {
        Ok(content) => serde_json::from_slice(&content)
            .with_context(|| format!("cannot parse {}", display_path(path))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", display_path(path))),
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
        .ok_or_else(|| anyhow!("configuration path has no parent: {}", display_path(path)))?;
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
        return Err(error).with_context(|| format!("cannot replace {}", display_path(path)));
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    async fn read_http_request(socket: &mut TcpStream) -> String {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0, "client closed before finishing its request");
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .map(|index| index + 4)
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or_default();
            if request.len() >= header_end + content_length {
                return String::from_utf8(request).unwrap();
            }
        }
    }

    async fn token_server(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}"), server)
    }

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
    async fn model_roles_are_project_overridable_and_resolve_model_thinking() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let project = root.path().join("project");
        fs::create_dir_all(&directory).await.unwrap();
        fs::create_dir_all(project.join(".uri-agent"))
            .await
            .unwrap();
        fs::write(
            directory.join("models.json"),
            br#"{"providers":{"global-provider":{"baseUrl":"https://global.invalid/v1","api":"openai-responses","models":[{"id":"global-model","name":"Global"}]},"project-provider":{"baseUrl":"https://project.invalid/v1","api":"openai-responses","models":[{"id":"project-model","name":"Project"}]}}}"#,
        )
        .await
        .unwrap();
        fs::write(
            directory.join("settings.json"),
            br#"{"modelRoles":{"commit":{"provider":"global-provider","model":"global-model","thinking":"high"}}}"#,
        )
        .await
        .unwrap();
        fs::write(
            project.join(".uri-agent/settings.json"),
            br#"{"modelRoles":{"commit":{"provider":"project-provider","model":"project-model"}},"modelThinkingLevels":{"project-provider/project-model":"medium"}}"#,
        )
        .await
        .unwrap();

        let manager = ConfigManager::load_for_test(&directory, &project)
            .await
            .unwrap();
        assert_eq!(
            manager.model_role("commit").await.unwrap(),
            Some(ModelRole {
                provider: "project-provider".to_string(),
                model: "project-model".to_string(),
                thinking: ThinkingLevel::Medium,
            })
        );
        assert_eq!(manager.model_role("missing").await.unwrap(), None);
        assert!(manager.model_role("invalid role").await.is_err());
        let roles = manager.model_roles().await.unwrap();
        let commit = roles.iter().find(|role| role.name == "commit").unwrap();
        assert_eq!(commit.source, Some(ValueSource::Project));
        assert!(commit.overrides_global);
    }

    #[tokio::test]
    async fn the_small_role_requires_an_independent_model_assignment() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let project = root.path().join("project");
        fs::create_dir_all(&directory).await.unwrap();
        fs::create_dir_all(&project).await.unwrap();
        fs::write(
            directory.join("models.json"),
            br#"{"providers":{"example":{"baseUrl":"https://example.invalid/v1","api":"openai-responses","models":[{"id":"active-model","name":"Active"},{"id":"role-model","name":"Role"}]}}}"#,
        )
        .await
        .unwrap();
        fs::write(
            directory.join("settings.json"),
            br#"{"defaultProvider":"example","defaultModel":"active-model"}"#,
        )
        .await
        .unwrap();

        let manager = ConfigManager::load_for_test(&directory, &project)
            .await
            .unwrap();
        assert_eq!(manager.model_role("small").await.unwrap(), None);
        assert_eq!(manager.model_role("default").await.unwrap(), None);
        assert_eq!(manager.model_role("large").await.unwrap(), None);

        manager
            .set_model_role("small", "example", "role-model", ThinkingLevel::Off)
            .await
            .unwrap();
        assert_eq!(
            manager.model_role("small").await.unwrap(),
            Some(ModelRole {
                provider: "example".to_string(),
                model: "role-model".to_string(),
                thinking: ThinkingLevel::Off,
            })
        );
        let roles = manager.model_roles().await.unwrap();
        let small = roles.iter().find(|role| role.name == "small").unwrap();
        assert_eq!(small.source, Some(ValueSource::Global));
        assert!(!small.overrides_global);

        assert!(manager.remove_model_role("small").await.unwrap());
        assert_eq!(manager.model_role("small").await.unwrap(), None);
    }

    #[tokio::test]
    async fn custom_roles_and_plugin_settings_use_project_precedence() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let project = root.path().join("project");
        fs::create_dir_all(&directory).await.unwrap();
        fs::create_dir_all(project.join(".uri-agent"))
            .await
            .unwrap();
        fs::write(
            directory.join("models.json"),
            br#"{"providers":{"example":{"baseUrl":"https://example.invalid/v1","api":"openai-responses","models":[{"id":"active-model","name":"Active"},{"id":"custom-model","name":"Custom"}]}}}"#,
        )
        .await
        .unwrap();
        fs::write(
            directory.join("settings.json"),
            br#"{"defaultProvider":"example","defaultModel":"active-model","pluginSettings":{"terminal-title":{"role":"small","format":{"words":5}}}}"#,
        )
        .await
        .unwrap();
        fs::write(
            project.join(".uri-agent/settings.json"),
            br#"{"pluginSettings":{"terminal-title":{"role":"large"}}}"#,
        )
        .await
        .unwrap();

        let manager = ConfigManager::load_for_test(&directory, &project)
            .await
            .unwrap();
        assert_eq!(
            manager
                .plugin_setting("terminal-title", "role")
                .await
                .unwrap(),
            Some(Value::String("large".to_string()))
        );
        assert_eq!(
            manager
                .plugin_setting("terminal-title", "format")
                .await
                .unwrap(),
            Some(serde_json::json!({"words": 5}))
        );

        manager
            .set_model_role("title", "example", "custom-model", ThinkingLevel::Off)
            .await
            .unwrap();
        assert_eq!(
            manager
                .model_roles()
                .await
                .unwrap()
                .into_iter()
                .map(|role| role.name)
                .collect::<Vec<_>>(),
            ["small", "title"]
        );
        manager
            .set_plugin_setting("terminal-title", "role", Value::String("title".to_string()))
            .await
            .unwrap();
        assert_eq!(
            manager
                .plugin_setting("terminal-title", "role")
                .await
                .unwrap(),
            Some(Value::String("title".to_string()))
        );
        assert!(manager.remove_model_role("title").await.unwrap());
        assert_eq!(manager.model_role("title").await.unwrap(), None);
        assert_eq!(
            manager
                .plugin_setting("terminal-title", "role")
                .await
                .unwrap(),
            Some(Value::String("title".to_string()))
        );

        let project_settings: Value = serde_json::from_slice(
            &fs::read(project.join(".uri-agent/settings.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            project_settings["pluginSettings"]["terminal-title"]["role"],
            "title"
        );
        assert!(
            manager
                .remove_plugin_setting("terminal-title", "role")
                .await
                .unwrap()
        );
        assert_eq!(
            manager
                .plugin_setting("terminal-title", "role")
                .await
                .unwrap(),
            Some(Value::String("small".to_string()))
        );
    }

    #[tokio::test]
    async fn model_provider_filter_scopes_generic_keys_to_the_current_provider() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let project = root.path().join("project");
        fs::create_dir_all(&directory).await.unwrap();
        fs::create_dir_all(&project).await.unwrap();
        fs::write(
            directory.join("models.json"),
            br#"{"providers":{"saved-provider":{"baseUrl":"https://saved.invalid/v1","api":"openai-responses","models":[{"id":"saved-model","name":"Saved"}]},"file-provider":{"baseUrl":"https://file.invalid/v1","api":"openai-responses","apiKey":"file-key","models":[{"id":"file-model","name":"File"}]},"generic-provider":{"baseUrl":"https://generic.invalid/v1","api":"openai-responses","models":[{"id":"generic-model","name":"Generic"}]},"missing-provider":{"baseUrl":"https://missing.invalid/v1","api":"openai-responses","models":[{"id":"missing-model","name":"Missing"}]}}}"#,
        )
        .await
        .unwrap();
        let catalog = Arc::new(ModelCatalog::load(&directory, true).await.unwrap());
        let manager = ConfigManager::load(
            directory.clone(),
            &project,
            catalog,
            InvocationOverrides {
                provider: Some("generic-provider".to_string()),
                api_key: Some("generic-key".to_string()),
                ..InvocationOverrides::default()
            },
        )
        .await
        .unwrap();
        manager
            .set_api_key("saved-provider", "saved-key".to_string())
            .await
            .unwrap();

        assert_eq!(
            manager
                .model_providers_with_credentials("generic-provider")
                .await,
            BTreeSet::from([
                "file-provider".to_string(),
                "generic-provider".to_string(),
                "saved-provider".to_string()
            ])
        );
        assert_eq!(
            manager.model_providers_with_credentials("").await,
            BTreeSet::from(["file-provider".to_string(), "saved-provider".to_string()])
        );
        manager.clear_api_key("saved-provider").await.unwrap();
        assert_eq!(
            manager.model_providers_with_credentials("").await,
            BTreeSet::from(["file-provider".to_string()])
        );
    }

    #[tokio::test]
    async fn clearing_provider_selection_removes_inherited_model_but_keeps_preferences() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let project = root.path().join("project");
        fs::create_dir_all(&directory).await.unwrap();
        fs::create_dir_all(project.join(".uri-agent"))
            .await
            .unwrap();
        fs::write(
            directory.join("settings.json"),
            br#"{"defaultProvider":"openai","defaultModel":"global-model","modelThinkingLevels":{"openai/project-model":"high"}}"#,
        )
        .await
        .unwrap();
        fs::write(
            project.join(".uri-agent/settings.json"),
            br#"{"defaultModel":"project-model"}"#,
        )
        .await
        .unwrap();
        let manager = ConfigManager::load_for_test(&directory, &project)
            .await
            .unwrap();

        assert!(
            manager
                .clear_model_selection_for_provider("openai")
                .await
                .unwrap()
        );
        let global: Value =
            serde_json::from_slice(&fs::read(directory.join("settings.json")).await.unwrap())
                .unwrap();
        let project: Value = serde_json::from_slice(
            &fs::read(project.join(".uri-agent/settings.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(global.get("defaultProvider").is_none());
        assert!(global.get("defaultModel").is_none());
        assert!(project.get("defaultModel").is_none());
        assert_eq!(
            global["modelThinkingLevels"]["openai/project-model"],
            "high"
        );
        assert!(!manager.current().await.model_configured());
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
        assert_eq!(api_key_environment("tinyfish"), "TINYFISH_API_KEY");

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

    #[tokio::test]
    async fn concurrent_managers_do_not_lose_auth_updates() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let first = ConfigManager::load_for_test(&directory, &root.path().join("first"))
            .await
            .unwrap();
        let second = ConfigManager::load_for_test(&directory, &root.path().join("second"))
            .await
            .unwrap();

        let (first_result, second_result) = tokio::join!(
            first.set_api_key("provider-one", "first-key".to_string()),
            second.set_api_key("provider-two", "second-key".to_string()),
        );
        first_result.unwrap();
        second_result.unwrap();

        let saved: AuthFile = read_json(&directory.join("auth.json")).await.unwrap();
        assert_eq!(saved.0["provider-one"].key.as_deref(), Some("first-key"));
        assert_eq!(saved.0["provider-two"].key.as_deref(), Some("second-key"));
    }

    #[tokio::test]
    async fn concurrent_managers_share_one_refresh_and_preserve_omitted_token() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let first = ConfigManager::load_for_test(&directory, &root.path().join("first"))
            .await
            .unwrap();
        let (gateway, server) =
            token_server(200, r#"{"access_token":"fresh-access","expires_in":3600}"#).await;
        let original = OauthToken {
            kind: "oauth".to_string(),
            refresh: "old-refresh".to_string(),
            access: "expired-access".to_string(),
            expires: 0,
            extra: BTreeMap::from([("gateway".to_string(), Value::String(gateway.clone()))]),
        };
        first.set_oauth("radius", original).await.unwrap();
        let second = ConfigManager::load_for_test(&directory, &root.path().join("second"))
            .await
            .unwrap();

        let (first_token, second_token) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(
                    first.refresh_oauth("radius", false),
                    second.refresh_oauth("radius", false),
                )
            })
            .await
            .unwrap();
        let first_token = first_token.unwrap();
        let second_token = second_token.unwrap();
        assert_eq!(first_token.access, "fresh-access");
        assert_eq!(second_token.access, "fresh-access");
        assert_eq!(first_token.refresh, "old-refresh");
        assert_eq!(second_token.refresh, "old-refresh");

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/oauth/token HTTP/1.1"));
        assert!(request.contains("refresh_token=old-refresh"));
        let saved: AuthFile = read_json(&directory.join("auth.json")).await.unwrap();
        let saved = oauth_token_from_entry("radius", &saved.0["radius"]).unwrap();
        assert_eq!(saved.access, "fresh-access");
        assert_eq!(saved.refresh, "old-refresh");
    }

    #[tokio::test]
    async fn failed_refresh_preserves_stored_and_in_memory_credentials() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let manager = ConfigManager::load_for_test(&directory, &root.path().join("project"))
            .await
            .unwrap();
        let (gateway, server) = token_server(400, r#"{"error":"invalid_grant"}"#).await;
        let original = OauthToken {
            kind: "oauth".to_string(),
            refresh: "old-refresh".to_string(),
            access: "expired-access".to_string(),
            expires: 0,
            extra: BTreeMap::from([("gateway".to_string(), Value::String(gateway))]),
        };
        manager.set_oauth("radius", original.clone()).await.unwrap();

        assert!(manager.refresh_oauth("radius", false).await.is_err());
        server.await.unwrap();
        assert_eq!(manager.oauth_token("radius").await.unwrap(), original);
        let saved: AuthFile = read_json(&directory.join("auth.json")).await.unwrap();
        assert_eq!(
            oauth_token_from_entry("radius", &saved.0["radius"]).unwrap(),
            original
        );
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

    #[cfg(not(windows))]
    #[tokio::test]
    async fn background_discovery_skips_credential_commands_until_forced() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("discovery-command-ran");
        let command = format!("!touch '{}'; printf live-secret", marker.to_string_lossy());
        let candidate = || DiscoveryCredentialCandidate {
            provider: "opencode-go".to_string(),
            value: command.clone(),
            oauth: false,
            environment: BTreeMap::new(),
        };

        let credentials = resolve_discovery_credentials(vec![candidate()], false).await;
        assert!(credentials.is_empty());
        assert!(!marker.exists());

        let credentials = resolve_discovery_credentials(vec![candidate()], true).await;
        assert_eq!(credentials["opencode-go"].secret, "live-secret");
        assert!(marker.exists());

        fs::remove_file(&marker).await.unwrap();
        let credentials = resolve_discovery_credentials(vec![candidate()], false).await;
        assert_eq!(credentials["opencode-go"].secret, "live-secret");
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn discovery_silently_skips_credentials_that_are_not_currently_usable() {
        let unavailable_variable =
            format!("URI_AGENT_DISCOVERY_MISSING_{}", Uuid::new_v4().simple());
        let candidate = |value: String| DiscoveryCredentialCandidate {
            provider: "zai".to_string(),
            value,
            oauth: false,
            environment: BTreeMap::new(),
        };

        let credentials = resolve_discovery_credentials(
            vec![
                candidate(String::new()),
                candidate(format!("${{{unavailable_variable}}}")),
            ],
            true,
        )
        .await;

        assert!(credentials.is_empty());
    }

    #[tokio::test]
    async fn invocation_api_key_is_scoped_to_the_selected_discovery_provider() {
        let root = tempfile::tempdir().unwrap();
        let catalog = ModelCatalog::load(root.path(), true).await.unwrap();
        let files = ConfigFiles {
            global: SettingsFile::default(),
            project: SettingsFile::default(),
            auth: AuthFile::default(),
        };
        let invocation = InvocationOverrides {
            provider: Some("opencode-go".to_string()),
            api_key: Some("invocation-key".to_string()),
            ..InvocationOverrides::default()
        };

        let selected =
            resolve_model_credential(&files, &catalog, &invocation, "opencode-go", true).await;
        let unrelated = resolve_model_credential(
            &files,
            &catalog,
            &invocation,
            "uri-agent-discovery-unselected",
            false,
        )
        .await;

        assert_eq!(selected.api_key.as_deref(), Some("invocation-key"));
        assert_eq!(unrelated.api_key, None);
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn config_commands_cannot_read_stdin_or_extra_descriptors() {
        use std::os::fd::AsRawFd;

        let directory = tempfile::tempdir().unwrap();
        let inherited = std::fs::File::create(directory.path().join("inherited")).unwrap();
        let descriptor = inherited.as_raw_fd();
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
            0
        );
        let command = format!(
            "if read value; then printf stdin; elif [ -e /proc/self/fd/{descriptor} ]; then printf fd; else printf isolated; fi"
        );

        let output = execute_config_command(&command, Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        let restored = unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags) };

        assert_eq!(restored, 0);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"isolated");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_command_timeout_terminates_background_descendants_before_returning() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("leaked");
        let command = format!("sleep 0.2; printf leaked > '{}'", marker.display());

        let output = execute_config_command(&command, Duration::from_millis(30))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(output.is_none());
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_config_command_terminates_lingering_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("leaked");
        let command = format!(
            "printf secret; (sleep 0.2; printf leaked > '{}') &",
            marker.display()
        );

        let output = execute_config_command(&command, Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(output.status.success());
        assert_eq!(output.stdout, b"secret");
        assert!(!marker.exists());
    }

    #[cfg(windows)]
    fn windows_config_tests_require_pwsh() -> bool {
        std::process::Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn config_command_timeout_terminates_windows_processes_before_returning() {
        if !windows_config_tests_require_pwsh() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("leaked");
        let command = format!(
            "Start-Sleep -Seconds 3; Set-Content -Path '{}' -Value leaked",
            marker.display()
        );

        let output = execute_config_command(&command, Duration::from_millis(200))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(4)).await;

        assert!(output.is_none());
        assert!(!marker.exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn successful_config_command_terminates_lingering_windows_descendants() {
        if !windows_config_tests_require_pwsh() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("leaked");
        let leak_script = BASE64.encode(format!(
            "Start-Sleep -Seconds 3; Set-Content -LiteralPath '{}' -Value leaked",
            marker.display()
        ));
        let command = format!(
            "[Console]::Out.Write('secret'); Start-Process -WindowStyle Hidden pwsh -ArgumentList '-NoProfile', '-EncodedCommand', '{leak_script}' | Out-Null"
        );

        let output = execute_config_command(&command, Duration::from_secs(10))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_secs(4)).await;

        assert!(output.status.success());
        assert_eq!(output.stdout, b"secret");
        assert!(!marker.exists());
    }

    #[test]
    fn macos_default_config_directory_uses_home_config() {
        let home = PathBuf::from("/Users/ada");
        let directory = default_config_directory_from(
            Some(&home),
            Some(Path::new("/Users/ada/Library/Application Support")),
            true,
        )
        .unwrap();
        assert_eq!(directory, home.join(".config").join("uri-agent"));
    }

    #[test]
    fn platform_default_config_directory_uses_the_platform_config_root() {
        let directory = default_config_directory_from(
            Some(Path::new("/home/ada")),
            Some(Path::new("/home/ada/.config")),
            false,
        )
        .unwrap();
        assert_eq!(
            directory,
            PathBuf::from("/home/ada/.config").join("uri-agent")
        );
    }

    #[test]
    fn missing_home_directory_is_an_error_when_home_config_is_required() {
        let error = default_config_directory_from(None, Some(Path::new("/platform/config")), true)
            .unwrap_err();
        assert!(format!("{error:#}").contains("cannot determine the platform config directory"));
    }

    #[tokio::test]
    async fn macos_legacy_directory_moves_all_files_and_removes_the_source() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("Application Support").join("uri-agent");
        let new = root.path().join(".config").join("uri-agent");
        fs::create_dir_all(old.join("wasm-plugins")).await.unwrap();
        fs::write(old.join("settings.json"), "{}\n").await.unwrap();
        fs::write(old.join("auth.json"), "{\"providers\":{}}\n")
            .await
            .unwrap();
        fs::write(old.join("sessions-v2.db"), b"db").await.unwrap();
        fs::write(old.join("sessions-v2.db-wal"), b"wal")
            .await
            .unwrap();
        fs::write(old.join("wasm-plugins").join("demo.wasm"), b"wasm")
            .await
            .unwrap();

        migrate_legacy_directory(&old, &new).await.unwrap();

        assert_eq!(fs::read(new.join("settings.json")).await.unwrap(), b"{}\n");
        assert_eq!(
            fs::read(new.join("auth.json")).await.unwrap(),
            b"{\"providers\":{}}\n"
        );
        assert_eq!(fs::read(new.join("sessions-v2.db")).await.unwrap(), b"db");
        assert_eq!(
            fs::read(new.join("sessions-v2.db-wal")).await.unwrap(),
            b"wal"
        );
        assert_eq!(
            fs::read(new.join("wasm-plugins").join("demo.wasm"))
                .await
                .unwrap(),
            b"wasm"
        );
        assert!(!old.exists());
    }

    #[tokio::test]
    async fn macos_legacy_directory_does_not_overwrite_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old");
        let new = root.path().join("new");
        fs::create_dir_all(&old).await.unwrap();
        fs::create_dir_all(&new).await.unwrap();
        fs::write(old.join("settings.json"), "old").await.unwrap();
        fs::write(old.join("auth.json"), "old-auth").await.unwrap();
        fs::write(old.join("sessions-v2.db"), "old-db")
            .await
            .unwrap();
        fs::write(new.join("settings.json"), "new").await.unwrap();

        migrate_legacy_directory(&old, &new).await.unwrap();

        assert_eq!(
            fs::read_to_string(new.join("settings.json")).await.unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(new.join("auth.json")).await.unwrap(),
            "old-auth"
        );
        assert_eq!(
            fs::read_to_string(new.join("sessions-v2.db"))
                .await
                .unwrap(),
            "old-db"
        );
        assert!(!old.exists());
    }

    #[tokio::test]
    async fn macos_legacy_directory_is_a_noop_when_legacy_directory_is_missing() {
        let root = tempfile::tempdir().unwrap();
        migrate_legacy_directory(&root.path().join("missing"), &root.path().join("new"))
            .await
            .unwrap();
        assert!(!root.path().join("new").exists());
    }

    #[tokio::test]
    async fn macos_legacy_directory_is_a_noop_when_directories_match() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("uri-agent");
        fs::create_dir_all(&directory).await.unwrap();
        fs::write(directory.join("settings.json"), "keep")
            .await
            .unwrap();
        migrate_legacy_directory(&directory, &directory)
            .await
            .unwrap();
        assert!(directory.exists());
        assert_eq!(
            fs::read_to_string(directory.join("settings.json"))
                .await
                .unwrap(),
            "keep"
        );
    }
}
