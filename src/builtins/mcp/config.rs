//! Layered user and project MCP server configuration, with validation.

use crate::config::{display_path, validate_environment_name};
use crate::plugin::PluginEnvironment;
use anyhow::{Context, Result, anyhow, bail};
use http::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) const PROJECT_CONFIG: &str = ".agents/mcp.json";
pub(crate) const GLOBAL_CONFIG: &str = "mcp.json";
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
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Debug)]
pub(super) struct EffectiveServer {
    pub(super) name: String,
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
}

impl McpConfigStore {
    pub(super) fn new(cwd: &Path, config_directory: &Path) -> Self {
        Self {
            project: cwd.join(PROJECT_CONFIG),
            global: config_directory.join(GLOBAL_CONFIG),
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum McpValueMode {
    EnvironmentReferences,
    Literal,
}

#[derive(Clone)]
pub(super) struct McpResolver {
    store: McpConfigStore,
}

impl McpResolver {
    pub(super) fn new(store: McpConfigStore) -> Self {
        Self { store }
    }

    pub(super) async fn resolve(&self, name: &str) -> Result<(McpServerConfig, McpValueMode)> {
        Ok((
            self.store.resolve(name).await?.parse()?,
            McpValueMode::EnvironmentReferences,
        ))
    }

    pub(super) fn project_directory(&self) -> PathBuf {
        self.store
            .project
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    }

    pub(super) fn environment_revision(&self, environment: &PluginEnvironment) -> u64 {
        environment.revision()
    }
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
