use crate::catalog::{CatalogModel, ModelCatalog, api_key_environment};
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
const DEFAULT_EDITOR: &str = "hx";

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

    /// External editor command used for drafts and event details.
    #[arg(long, value_name = "COMMAND")]
    pub editor: Option<String>,

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
                    editor: cli.editor,
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
            catalog,
            session,
            cwd,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueSource {
    Default,
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
    pub output_limit: usize,
    pub editor: Option<String>,
    pub provider_source: ValueSource,
    pub model_source: ValueSource,
    pub api_key_source: ValueSource,
    pub output_limit_source: ValueSource,
    pub editor_source: ValueSource,
    pub credential_environment: BTreeMap<String, String>,
}

impl ActiveSettings {
    pub async fn catalog_model(&self, catalog: &ModelCatalog) -> Option<CatalogModel> {
        catalog.model(&self.provider, &self.model).await
    }
}

#[derive(Clone, Default)]
struct InvocationOverrides {
    provider: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    output_limit: Option<usize>,
    editor: Option<String>,
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
    editor: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
struct AuthFile(BTreeMap<String, AuthEntry>);

#[derive(Clone, Default, Deserialize, Serialize)]
struct AuthEntry {
    #[serde(rename = "type", default = "api_key_type")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
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
            global.default_provider = Some("openai".to_string());
            global.default_model = Some("gpt-5.2".to_string());
            global.output_limit = Some(DEFAULT_OUTPUT_LIMIT);
            global.editor = Some(DEFAULT_EDITOR.to_string());
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
        })
    }

    pub async fn current(&self) -> ActiveSettings {
        self.active.read().await.clone()
    }

    pub async fn reload(&self) -> Result<ActiveSettings> {
        self.catalog.reload_user_overrides().await?;
        let mut files = self.files.lock().await;
        files.global = read_json(&self.settings_path()).await?;
        files.project = read_json(&self.project_path).await?;
        files.auth = read_json(&self.auth_path()).await?;
        let active = calculate_active(&files, &self.catalog, &self.invocation).await?;
        *self.active.write().await = active.clone();
        Ok(active)
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

    pub async fn set_editor(&self, editor: Option<String>) -> Result<ActiveSettings> {
        let editor = editor.and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        });
        let mut files = self.files.lock().await;
        let (settings, path) = if self.project_path.exists() {
            (&mut files.project, self.project_path.clone())
        } else {
            (&mut files.global, self.settings_path())
        };
        settings.editor = editor;
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
                env: BTreeMap::new(),
                extra: BTreeMap::new(),
            },
        );
        write_json(&self.auth_path(), &files.auth, true).await?;
        self.recalculate(&files).await
    }

    pub async fn clear_api_key(&self, provider: &str) -> Result<ActiveSettings> {
        let mut files = self.files.lock().await;
        files.auth.0.remove(provider);
        write_json(&self.auth_path(), &files.auth, true).await?;
        self.recalculate(&files).await
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
        "openai".to_string(),
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

    let default_model = catalog
        .default_model(&provider)
        .await
        .map(|model| model.id)
        .unwrap_or_else(|| match provider.as_str() {
            "anthropic" => "claude-sonnet-4-6".to_string(),
            "google" => "gemini-3-flash-preview".to_string(),
            _ => "gpt-5.2".to_string(),
        });
    let (mut model, mut model_source) = if provider == settings_provider {
        setting(
            default_model,
            files.global.default_model.clone(),
            files.project.default_model.clone(),
        )
    } else {
        (default_model, ValueSource::Default)
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

    let (mut editor, mut editor_source) = setting(
        Some(DEFAULT_EDITOR.to_string()),
        files.global.editor.clone().map(Some),
        files.project.editor.clone().map(Some),
    );
    for environment in ["EDITOR", "VISUAL", "URI_AGENT_EDITOR"] {
        if let Ok(value) = env::var(environment)
            && !value.trim().is_empty()
        {
            editor = Some(value);
            editor_source = ValueSource::Environment(environment.to_string());
        }
    }
    if let Some(value) = &invocation.editor {
        editor = (!value.trim().is_empty()).then(|| value.trim().to_string());
        editor_source = ValueSource::CommandLine;
    }

    let configured_entry = files
        .auth
        .0
        .get(&provider)
        .filter(|entry| entry.kind == "api_key");
    let configured = configured_entry.and_then(|entry| entry.key.clone());
    let credential_environment = configured_entry
        .map(|entry| entry.env.clone())
        .unwrap_or_default();
    let models_key = catalog.configured_api_key(&provider).await;
    let (mut api_key, mut api_key_source) = if configured.is_some() {
        (configured, ValueSource::Global)
    } else {
        (models_key, ValueSource::ModelsFile)
    };
    let provider_environment = api_key_environment(&provider);
    for environment in [provider_environment.as_str(), "URI_AGENT_API_KEY"] {
        if let Ok(value) = env::var(environment)
            && !value.trim().is_empty()
        {
            api_key = Some(value);
            api_key_source = ValueSource::Environment(environment.to_string());
        }
    }
    if let Some(value) = &invocation.api_key {
        api_key = Some(value.clone());
        api_key_source = ValueSource::CommandLine;
    }
    api_key = match api_key {
        Some(value) => Some(resolve_config_value(&value, &credential_environment).await?),
        None => None,
    };

    Ok(ActiveSettings {
        provider,
        model,
        api_key,
        output_limit,
        editor,
        provider_source,
        model_source,
        api_key_source,
        output_limit_source,
        editor_source,
        credential_environment,
    })
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

    #[test]
    fn settings_file_preserves_pi_fields_it_does_not_own() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "defaultProvider": "openai",
            "defaultModel": "gpt-5.2",
            "thinkingLevel": "high"
        }))
        .unwrap();
        let value = serde_json::to_value(settings).unwrap();
        assert_eq!(value["thinkingLevel"], "high");
    }

    #[test]
    fn editor_is_a_text_setting_alongside_pi_fields() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "defaultProvider": "openai",
            "editor": "nvim -f"
        }))
        .unwrap();
        assert_eq!(settings.editor.as_deref(), Some("nvim -f"));
        assert_eq!(serde_json::to_value(settings).unwrap()["editor"], "nvim -f");
    }

    #[test]
    fn helix_is_the_default_editor_command() {
        let (editor, source) = setting(
            Some(DEFAULT_EDITOR.to_string()),
            None::<Option<String>>,
            None,
        );
        assert_eq!(editor.as_deref(), Some("hx"));
        assert_eq!(source, ValueSource::Default);
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
