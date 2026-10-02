//! MCP configuration: server profiles, layered user/project configuration
//! files with locked updates, validation, and protocol-name derivation.

use super::OWNER;
use crate::atomic_file::resolve_write_path;
use crate::builtins::atomic_write;
use crate::config::{display_path, validate_environment_name};
use crate::plugin::PluginEnvironment;
use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use http::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) const PROJECT_CONFIG: &str = ".agents/mcp.json";
pub(crate) const GLOBAL_CONFIG: &str = "mcp.json";
const SESSION_PROFILE_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionMcpProfile {
    version: u8,
    pub servers: BTreeMap<String, SessionMcpServer>,
}

impl SessionMcpProfile {
    pub fn new(servers: BTreeMap<String, SessionMcpServer>) -> Self {
        Self {
            version: SESSION_PROFILE_VERSION,
            servers,
        }
    }

    fn into_configs(self) -> Result<BTreeMap<String, McpServerConfig>> {
        if self.version != SESSION_PROFILE_VERSION {
            bail!("unsupported private MCP session profile version");
        }
        self.servers
            .into_iter()
            .map(|(name, server)| {
                let config = McpServerConfig {
                    description: format!("MCP server {name} provided by the session frontend"),
                    enabled: true,
                    transport: match server.transport {
                        SessionMcpTransport::Stdio {
                            command,
                            args,
                            environment,
                        } => McpTransportConfig::Stdio {
                            command,
                            args,
                            cwd: None,
                            environment,
                        },
                        SessionMcpTransport::StreamableHttp { url, headers } => {
                            McpTransportConfig::StreamableHttp { url, headers }
                        }
                    },
                };
                config.validate_session(&name)?;
                Ok((name, config))
            })
            .collect()
    }

    fn validate(&self) -> Result<()> {
        let configs = self.clone().into_configs()?;
        let mut protocols = HashSet::new();
        for name in configs.keys() {
            let protocol = protocol_name(name)?;
            if !protocols.insert(protocol.clone()) {
                bail!("MCP server protocol name collides: {protocol}://");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionMcpServer {
    #[serde(flatten)]
    pub transport: SessionMcpTransport,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum SessionMcpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        environment: BTreeMap<String, String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

pub fn session_profile_record(profile: SessionMcpProfile) -> Result<(String, Value)> {
    profile.validate()?;
    Ok((OWNER.to_string(), serde_json::to_value(profile)?))
}

pub fn session_profile_owner() -> &'static str {
    OWNER
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum McpScope {
    User,
    Project,
}

impl McpScope {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Project => "Project",
        }
    }

    pub(super) fn other(self) -> Self {
        match self {
            Self::User => Self::Project,
            Self::Project => Self::User,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct McpServerConfig {
    pub(super) description: String,
    #[serde(default = "enabled_by_default")]
    pub(super) enabled: bool,
    #[serde(flatten)]
    pub(super) transport: McpTransportConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub(super) enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        environment: BTreeMap<String, String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

impl McpServerConfig {
    pub(super) fn validate(&self, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            bail!("MCP server name cannot be empty");
        }
        if self.description.trim().is_empty() {
            bail!("MCP server {name:?} requires a description");
        }
        match &self.transport {
            McpTransportConfig::Stdio {
                command,
                environment,
                ..
            } => {
                if command.trim().is_empty() {
                    bail!("MCP server {name:?} requires a stdio command");
                }
                for (target, source) in environment {
                    validate_environment_name(target).with_context(|| {
                        format!("invalid MCP server {name:?} process environment name")
                    })?;
                    validate_environment_name(source).with_context(|| {
                        format!("invalid MCP server {name:?} Agent Environment reference")
                    })?;
                }
            }
            McpTransportConfig::StreamableHttp { url, headers } => {
                validate_http_url(url)
                    .with_context(|| format!("invalid MCP server {name:?} URL"))?;
                for (header, template) in headers {
                    header.parse::<HeaderName>().with_context(|| {
                        format!("invalid MCP server {name:?} HTTP header {header:?}")
                    })?;
                    if sensitive_header(header) && !template_references_environment(template) {
                        bail!(
                            "MCP server {name:?} credential header {header:?} must reference Agent Environment with ${{NAME}}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_session(&self, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            bail!("MCP server name cannot be empty");
        }
        match &self.transport {
            McpTransportConfig::Stdio {
                command,
                environment,
                ..
            } => {
                if command.trim().is_empty() {
                    bail!("MCP server {name:?} requires a stdio command");
                }
                for target in environment.keys() {
                    validate_environment_name(target).with_context(|| {
                        format!("invalid MCP server {name:?} process environment name")
                    })?;
                }
            }
            McpTransportConfig::StreamableHttp { url, headers } => {
                validate_http_url(url)
                    .with_context(|| format!("invalid MCP server {name:?} URL"))?;
                for (header, value) in headers {
                    header.parse::<HeaderName>().with_context(|| {
                        format!("invalid MCP server {name:?} HTTP header {header:?}")
                    })?;
                    HeaderValue::from_str(value).with_context(|| {
                        format!("invalid MCP server {name:?} HTTP header {header:?}")
                    })?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn transport_label(&self) -> &'static str {
        match self.transport {
            McpTransportConfig::Stdio { .. } => "stdio",
            McpTransportConfig::StreamableHttp { .. } => "Streamable HTTP",
        }
    }
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Debug)]
pub(super) struct EffectiveServer {
    pub(super) name: String,
    pub(super) scope: McpScope,
    pub(super) raw: Value,
    pub(super) value_mode: McpValueMode,
}

impl EffectiveServer {
    pub(super) fn parse(&self) -> Result<McpServerConfig> {
        let config: McpServerConfig = serde_json::from_value(self.raw.clone())
            .with_context(|| format!("invalid MCP server configuration for {:?}", self.name))?;
        match self.value_mode {
            McpValueMode::EnvironmentReferences => config.validate(&self.name)?,
            McpValueMode::Literal => config.validate_session(&self.name)?,
        }
        Ok(config)
    }
}

#[derive(Clone)]
pub(super) struct McpConfigStore {
    pub(super) project: PathBuf,
    global: PathBuf,
    updates: Arc<Mutex<()>>,
}

impl McpConfigStore {
    pub(super) fn new(cwd: &Path, config_directory: &Path) -> Self {
        Self {
            project: cwd.join(PROJECT_CONFIG),
            global: config_directory.join(GLOBAL_CONFIG),
            updates: Arc::new(Mutex::new(())),
        }
    }

    fn path(&self, scope: McpScope) -> &Path {
        match scope {
            McpScope::User => &self.global,
            McpScope::Project => &self.project,
        }
    }

    pub(super) fn effective_sync(&self) -> Result<BTreeMap<String, EffectiveServer>> {
        let global = read_servers_sync(&self.global)?;
        let project = read_servers_sync(&self.project)?;
        Ok(layer_servers(global, project))
    }

    pub(super) async fn effective(&self) -> Result<BTreeMap<String, EffectiveServer>> {
        let global = read_servers(&self.global).await?;
        let project = read_servers(&self.project).await?;
        Ok(layer_servers(global, project))
    }

    pub(super) async fn resolve(&self, name: &str) -> Result<EffectiveServer> {
        self.effective()
            .await?
            .remove(name)
            .ok_or_else(|| anyhow!("MCP server {name:?} is no longer configured"))
    }

    pub(super) async fn raw_at(&self, scope: McpScope, name: &str) -> Result<Option<Value>> {
        Ok(read_servers(self.path(scope)).await?.remove(name))
    }

    pub(super) async fn write(&self, scope: McpScope, name: &str, value: Value) -> Result<()> {
        let path = resolve_mcp_write_path(self.path(scope)).await?;
        let _update = self.updates.lock().await;
        let _file = lock_config_files([path.as_path()]).await?;
        let mut document = read_document(&path).await?;
        servers_object_mut(&mut document, &path)?.insert(name.to_string(), value);
        write_document(&path, &document).await
    }

    pub(super) async fn remove(&self, scope: McpScope, name: &str) -> Result<Option<Value>> {
        let path = resolve_mcp_write_path(self.path(scope)).await?;
        let _update = self.updates.lock().await;
        let _file = lock_config_files([path.as_path()]).await?;
        let mut document = read_document(&path).await?;
        let removed = servers_object_mut(&mut document, &path)?.remove(name);
        if removed.is_some() {
            write_document(&path, &document).await?;
        }
        Ok(removed)
    }

    pub(super) async fn move_server(
        &self,
        from: McpScope,
        to: McpScope,
        name: &str,
        value: Value,
    ) -> Result<()> {
        if from == to {
            return self.write(to, name, value).await;
        }
        let from_path = resolve_mcp_write_path(self.path(from)).await?;
        let to_path = resolve_mcp_write_path(self.path(to)).await?;
        let _update = self.updates.lock().await;
        let _files = lock_config_files([from_path.as_path(), to_path.as_path()]).await?;
        let mut from_document = read_document(&from_path).await?;
        let mut to_document = read_document(&to_path).await?;
        if servers_object_mut(&mut to_document, &to_path)?.contains_key(name) {
            bail!("MCP server {name:?} already exists in {} scope", to.label());
        }
        if !servers_object_mut(&mut from_document, &from_path)?.contains_key(name) {
            bail!(
                "MCP server {name:?} no longer exists in {} scope",
                from.label()
            );
        }
        let target_before = to_document.clone();
        servers_object_mut(&mut to_document, &to_path)?.insert(name.to_string(), value);
        write_document(&to_path, &to_document).await?;
        servers_object_mut(&mut from_document, &from_path)?.remove(name);
        if let Err(error) = write_document(&from_path, &from_document).await {
            let rollback = write_document(&to_path, &target_before).await;
            return match rollback {
                Ok(()) => Err(error).context("could not remove the MCP server from its old scope"),
                Err(rollback) => Err(error).context(format!(
                    "could not remove the MCP server from its old scope; rollback also failed: {rollback:#}"
                )),
            };
        }
        Ok(())
    }
}

async fn resolve_mcp_write_path(path: &Path) -> Result<PathBuf> {
    resolve_write_path(path)
        .await
        .with_context(|| format!("cannot resolve MCP configuration {}", display_path(path)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum McpValueMode {
    EnvironmentReferences,
    Literal,
}

#[derive(Clone)]
pub(super) enum McpResolver {
    Configured(McpConfigStore),
    Session {
        project: PathBuf,
        servers: Arc<BTreeMap<String, McpServerConfig>>,
    },
}

impl McpResolver {
    pub(super) fn new(cwd: &Path, store: McpConfigStore, profile: Option<Value>) -> Result<Self> {
        let Some(profile) = profile else {
            return Ok(Self::Configured(store));
        };
        let profile: SessionMcpProfile =
            serde_json::from_value(profile).context("invalid private MCP session profile")?;
        Ok(Self::Session {
            project: cwd.to_path_buf(),
            servers: Arc::new(profile.into_configs()?),
        })
    }

    pub(super) fn effective_sync(&self) -> Result<BTreeMap<String, EffectiveServer>> {
        match self {
            Self::Configured(store) => store.effective_sync(),
            Self::Session { servers, .. } => servers
                .iter()
                .map(|(name, config)| {
                    Ok((
                        name.clone(),
                        EffectiveServer {
                            name: name.clone(),
                            scope: McpScope::Project,
                            raw: serde_json::to_value(config)?,
                            value_mode: McpValueMode::Literal,
                        },
                    ))
                })
                .collect(),
        }
    }

    pub(super) async fn resolve(&self, name: &str) -> Result<(McpServerConfig, McpValueMode)> {
        match self {
            Self::Configured(store) => Ok((
                store.resolve(name).await?.parse()?,
                McpValueMode::EnvironmentReferences,
            )),
            Self::Session { servers, .. } => servers
                .get(name)
                .cloned()
                .map(|config| (config, McpValueMode::Literal))
                .ok_or_else(|| anyhow!("MCP server {name:?} is not part of this session")),
        }
    }

    pub(super) fn project_directory(&self) -> PathBuf {
        match self {
            Self::Configured(store) => store
                .project
                .parent()
                .and_then(Path::parent)
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            Self::Session { project, .. } => project.clone(),
        }
    }

    pub(super) fn environment_revision(&self, environment: &PluginEnvironment) -> u64 {
        match self {
            Self::Configured(_) => environment.revision(),
            Self::Session { .. } => 0,
        }
    }

    pub(super) fn session_scoped(&self) -> bool {
        matches!(self, Self::Session { .. })
    }
}

async fn lock_config_files<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> Result<Vec<std::fs::File>> {
    let mut paths = paths.into_iter().map(config_lock_path).collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    tokio::task::spawn_blocking(move || {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "cannot create MCP configuration directory {}",
                        display_path(parent)
                    )
                })?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.create(true).truncate(false).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&path).with_context(|| {
                format!("cannot open MCP configuration lock {}", display_path(&path))
            })?;
            file.lock_exclusive().with_context(|| {
                format!("cannot lock MCP configuration {}", display_path(&path))
            })?;
            files.push(file);
        }
        Ok(files)
    })
    .await
    .context("MCP configuration lock worker failed")?
}

fn config_lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp.json");
    if let Some(agents) = path.parent()
        && agents.file_name().is_some_and(|name| name == ".agents")
        && let Some(project) = agents.parent()
    {
        return project.join(".uri-agent").join(format!("{name}.lock"));
    }
    path.with_file_name(format!("{name}.lock"))
}

fn layer_servers(
    global: BTreeMap<String, Value>,
    project: BTreeMap<String, Value>,
) -> BTreeMap<String, EffectiveServer> {
    let mut effective = global
        .into_iter()
        .map(|(name, raw)| {
            (
                name.clone(),
                EffectiveServer {
                    name,
                    scope: McpScope::User,
                    raw,
                    value_mode: McpValueMode::EnvironmentReferences,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    for (name, raw) in project {
        effective.insert(
            name.clone(),
            EffectiveServer {
                name,
                scope: McpScope::Project,
                raw,
                value_mode: McpValueMode::EnvironmentReferences,
            },
        );
    }
    effective
}

fn read_servers_sync(path: &Path) -> Result<BTreeMap<String, Value>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("cannot read MCP configuration {}", display_path(path)))?;
    parse_servers(&bytes, path)
}

async fn read_servers(path: &Path) -> Result<BTreeMap<String, Value>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("cannot read MCP configuration {}", display_path(path)))?;
    parse_servers(&bytes, path)
}

fn parse_servers(bytes: &[u8], path: &Path) -> Result<BTreeMap<String, Value>> {
    let document: Value = serde_json::from_slice(bytes)
        .with_context(|| format!("cannot parse MCP configuration {}", display_path(path)))?;
    let object = document.as_object().ok_or_else(|| {
        anyhow!(
            "MCP configuration must be a JSON object: {}",
            display_path(path)
        )
    })?;
    let Some(servers) = object.get("servers") else {
        return Ok(BTreeMap::new());
    };
    let servers = servers.as_object().ok_or_else(|| {
        anyhow!(
            "MCP configuration servers must be an object: {}",
            display_path(path)
        )
    })?;
    Ok(servers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect())
}

pub(super) async fn read_document(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({ "servers": {} }));
    }
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("cannot read MCP configuration {}", display_path(path)))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("cannot parse MCP configuration {}", display_path(path)))
}

fn servers_object_mut<'a>(
    document: &'a mut Value,
    path: &Path,
) -> Result<&'a mut Map<String, Value>> {
    let object = document.as_object_mut().ok_or_else(|| {
        anyhow!(
            "MCP configuration must be a JSON object: {}",
            display_path(path)
        )
    })?;
    if !object.contains_key("servers") {
        object.insert("servers".to_string(), Value::Object(Map::new()));
    }
    object
        .get_mut("servers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            anyhow!(
                "MCP configuration servers must be an object: {}",
                display_path(path)
            )
        })
}

async fn write_document(path: &Path, document: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(document)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes).await
}

pub(super) fn protocol_name(name: &str) -> Result<String> {
    crate::skill::protocol_slug(name, "-mcp", "MCP server")
}

pub(super) fn validate_http_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).context("URL is not valid")?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("MCP URLs cannot contain credentials; use Agent Environment header templates");
    }
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        "http" => bail!("remote MCP URLs must use HTTPS"),
        scheme => bail!("unsupported MCP URL scheme {scheme:?}"),
    }
}

fn sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "api-key"
            | "x-auth-token"
    )
}

fn template_references_environment(template: &str) -> bool {
    template
        .find("${")
        .is_some_and(|start| template[start + 2..].find('}').is_some_and(|end| end > 0))
}
