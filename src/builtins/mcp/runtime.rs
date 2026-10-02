//! MCP connection runtime: client transport setup, connection reuse and
//! gating, status tracking, and tool, prompt, and resource result formatting.

use super::UNTRUSTED_MCP_CONTENT;
use super::config::{
    EffectiveServer, McpConfigStore, McpResolver, McpServerConfig, McpTransportConfig, McpValueMode,
};
use super::protocol::render_json;
use crate::config::display_path;
use crate::output::OutputStore;
use crate::plugin::PluginEnvironment;
use crate::process::ProcessTree;
use crate::protocol::ProtocolOutput;
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use http::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ContentBlock,
    GetPromptResult, Implementation, JsonObject, ProtocolVersion, ReadResourceResult,
    ResourceContents,
};
use rmcp::service::{RunningService, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::common::client_side_sse::NeverRetry;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, Transport};
use rmcp::{ClientLifecycleMode, ClientServiceExt, Peer, RoleClient};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::Duration;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

const MCP_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct McpStatusSnapshot {
    pub(super) configured: usize,
    pub(super) connected: usize,
    pub(super) failed: usize,
}

type McpService = RunningService<RoleClient, ClientConfig>;

pub(super) struct McpConnection {
    pub(super) config: McpServerConfig,
    pub(super) environment_revision: u64,
    pub(super) peer: Peer<RoleClient>,
    pub(super) service: Mutex<Option<McpService>>,
}

impl McpConnection {
    fn is_closed(&self) -> bool {
        self.peer.is_transport_closed()
    }

    async fn close(&self) {
        if let Some(mut service) = self.service.lock().await.take() {
            let _ = tokio::time::timeout(MCP_CLOSE_TIMEOUT, service.close()).await;
        }
    }
}

pub(super) struct McpRuntime {
    pub(super) store: McpConfigStore,
    pub(super) resolver: McpResolver,
    environment: PluginEnvironment,
    output: Arc<OutputStore>,
    pub(super) connections: Mutex<HashMap<String, Arc<McpConnection>>>,
    connection_gates: SyncMutex<HashMap<String, Arc<Mutex<()>>>>,
    status: SyncMutex<HashMap<String, Result<(), String>>>,
    configured: AtomicUsize,
}

impl McpRuntime {
    #[cfg(test)]
    pub(super) fn new(
        store: McpConfigStore,
        environment: PluginEnvironment,
        output: Arc<OutputStore>,
    ) -> Self {
        let resolver = McpResolver::Configured(store.clone());
        Self::new_with_resolver(store, resolver, environment, output)
    }

    pub(super) fn new_with_resolver(
        store: McpConfigStore,
        resolver: McpResolver,
        environment: PluginEnvironment,
        output: Arc<OutputStore>,
    ) -> Self {
        let configured = resolver
            .effective_sync()
            .map(|servers| servers.len())
            .unwrap_or_default();
        Self {
            store,
            resolver,
            environment,
            output,
            connections: Mutex::new(HashMap::new()),
            connection_gates: SyncMutex::new(HashMap::new()),
            status: SyncMutex::new(HashMap::new()),
            configured: AtomicUsize::new(configured),
        }
    }

    pub(super) fn status_snapshot(&self) -> McpStatusSnapshot {
        let status = self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        McpStatusSnapshot {
            configured: self.configured.load(Ordering::Relaxed),
            connected: status.values().filter(|result| result.is_ok()).count(),
            failed: status.values().filter(|result| result.is_err()).count(),
        }
    }

    pub(super) fn refresh_configured(&self, count: usize) {
        self.configured.store(count, Ordering::Relaxed);
    }

    pub(super) fn connection_status(&self, name: &str) -> Option<Result<(), String>> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
    }

    fn set_status(&self, name: &str, result: Result<(), String>) {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_string(), result);
    }

    fn connection_gate(&self, name: &str) -> Arc<Mutex<()>> {
        self.connection_gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub(super) async fn connection(&self, name: &str) -> Result<Arc<McpConnection>> {
        let gate = self.connection_gate(name);
        let _connecting = gate.lock().await;
        let (config, value_mode) = match async {
            let (config, value_mode) = self.resolver.resolve(name).await?;
            if !config.enabled {
                bail!("MCP server {name:?} is disabled");
            }
            Ok::<_, anyhow::Error>((config, value_mode))
        }
        .await
        {
            Ok(config) => config,
            Err(error) => {
                self.remove_connection(name).await;
                self.set_status(name, Err(format!("{error:#}")));
                return Err(error);
            }
        };
        let environment_revision = self.resolver.environment_revision(&self.environment);
        let connection = self.connections.lock().await.get(name).cloned();
        if let Some(connection) = connection
            && connection.config == config
            && connection.environment_revision == environment_revision
            && !connection.is_closed()
        {
            return Ok(connection);
        }
        let stale = self.connections.lock().await.remove(name);
        if let Some(connection) = stale {
            connection.close().await;
        }
        match self
            .connect_with_timeout_mode(name, config.clone(), value_mode, MCP_CONNECTION_TIMEOUT)
            .await
        {
            Ok(connection) => {
                let connection = Arc::new(connection);
                self.connections
                    .lock()
                    .await
                    .insert(name.to_string(), connection.clone());
                self.set_status(name, Ok(()));
                Ok(connection)
            }
            Err(error) => {
                self.set_status(name, Err(format!("{error:#}")));
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn connect_with_timeout(
        &self,
        name: &str,
        config: McpServerConfig,
        timeout: Duration,
    ) -> Result<McpConnection> {
        self.connect_with_timeout_mode(name, config, McpValueMode::EnvironmentReferences, timeout)
            .await
    }

    pub(super) async fn connect_with_timeout_mode(
        &self,
        name: &str,
        config: McpServerConfig,
        value_mode: McpValueMode,
        timeout: Duration,
    ) -> Result<McpConnection> {
        let result = tokio::time::timeout(timeout, self.connect(name, config, value_mode))
            .await
            .map_err(|_| {
                anyhow!("MCP server {name:?} initialization timed out after {timeout:?}")
            })?;
        if value_mode == McpValueMode::Literal {
            result.map_err(|_| {
                anyhow!(
                    "could not initialize session MCP server {name:?}; connection details are hidden because the session configuration may contain credentials"
                )
            })
        } else {
            result
        }
    }

    async fn connect(
        &self,
        name: &str,
        config: McpServerConfig,
        value_mode: McpValueMode,
    ) -> Result<McpConnection> {
        let (environment_values, environment_revision) = match value_mode {
            McpValueMode::EnvironmentReferences => self.environment.snapshot_with_revision().await,
            McpValueMode::Literal => (BTreeMap::new(), 0),
        };
        let client = mcp_client_info();
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let service = match &config.transport {
            McpTransportConfig::Stdio {
                command,
                args,
                cwd,
                environment,
            } => {
                let process_environment = environment
                    .iter()
                    .map(|(target, source_or_value)| {
                        let value = match value_mode {
                            McpValueMode::EnvironmentReferences => environment_values
                                .get(source_or_value)
                                .ok_or_else(|| {
                                    anyhow!(
                                        "MCP server {name:?} requires missing Agent environment variable {source_or_value:?}"
                                    )
                                })?,
                            McpValueMode::Literal => source_or_value,
                        };
                        Ok((target.clone(), value.clone()))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
                let transport = self
                    .stdio_transport(name, command, args, cwd.as_deref(), &process_environment)
                    .await?;
                Box::pin(client.serve_with_lifecycle(transport, lifecycle))
                    .await
                    .with_context(|| format!("could not initialize MCP server {name:?}"))?
            }
            McpTransportConfig::StreamableHttp { url, headers } => {
                let headers =
                    self.expand_headers(name, headers, &environment_values, value_mode)?;
                // TODO: Add OAuth when URI Agent has an MCP OAuth credential flow.
                let mut transport_config =
                    StreamableHttpClientTransportConfig::with_uri(url.clone())
                        .custom_headers(headers)
                        .reinit_on_expired_session(false);
                transport_config.retry_config = Arc::new(NeverRetry::default());
                let transport = StreamableHttpClientTransport::from_config(transport_config);
                Box::pin(client.serve_with_lifecycle(transport, lifecycle))
                    .await
                    .with_context(|| format!("could not initialize MCP server {name:?}"))?
            }
        };
        let peer = service.peer().clone();
        Ok(McpConnection {
            config,
            environment_revision,
            peer,
            service: Mutex::new(Some(service)),
        })
    }

    async fn stdio_transport(
        &self,
        name: &str,
        executable: &str,
        args: &[String],
        cwd: Option<&Path>,
        environment: &BTreeMap<String, String>,
    ) -> Result<ProcessTreeTransport> {
        let mut command = Command::new(executable);
        command
            .args(args)
            .current_dir(cwd.map_or_else(
                || self.project_directory(),
                |cwd| {
                    if cwd.is_absolute() {
                        cwd.to_path_buf()
                    } else {
                        self.project_directory().join(cwd)
                    }
                },
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (target, value) in environment {
            command.env(target, value);
        }
        let (mut child, tree) = ProcessTree::spawn(&mut command)
            .with_context(|| format!("could not start MCP server {name:?}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP server {name:?} stdout is unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP server {name:?} stdin is unavailable"))?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            });
        }
        Ok(ProcessTreeTransport {
            inner: AsyncRwTransport::new_client(stdout, stdin),
            child,
            tree,
        })
    }

    fn project_directory(&self) -> PathBuf {
        self.resolver.project_directory()
    }

    pub(super) async fn effective_servers(&self) -> Result<Vec<EffectiveServer>> {
        let servers = match &self.resolver {
            McpResolver::Configured(_) => self.store.effective().await?,
            McpResolver::Session { .. } => self.resolver.effective_sync()?,
        };
        Ok(servers.into_values().collect())
    }

    fn expand_headers(
        &self,
        name: &str,
        templates: &BTreeMap<String, String>,
        environment: &BTreeMap<String, String>,
        value_mode: McpValueMode,
    ) -> Result<HashMap<HeaderName, HeaderValue>> {
        let mut headers = HashMap::new();
        for (header, template) in templates {
            let header_name = header
                .parse::<HeaderName>()
                .with_context(|| format!("invalid MCP server {name:?} header {header:?}"))?;
            let value = match value_mode {
                McpValueMode::EnvironmentReferences => expand_template(template, environment)
                    .with_context(|| {
                        format!("cannot resolve MCP server {name:?} header {header:?}")
                    })?,
                McpValueMode::Literal => template.clone(),
            };
            let value = HeaderValue::from_str(&value)
                .with_context(|| format!("invalid MCP server {name:?} header {header:?}"))?;
            headers.insert(header_name, value);
        }
        Ok(headers)
    }

    pub(super) async fn invalidate(&self, name: &str) {
        let gate = self.connection_gate(name);
        let _connecting = gate.lock().await;
        self.remove_connection(name).await;
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(name);
    }

    async fn remove_connection(&self, name: &str) {
        let connection = self.connections.lock().await.remove(name);
        if let Some(connection) = connection {
            connection.close().await;
        }
    }

    pub(super) async fn shutdown(&self) {
        let connections = self
            .connections
            .lock()
            .await
            .drain()
            .map(|(_, connection)| connection)
            .collect::<Vec<_>>();
        for connection in connections {
            connection.close().await;
        }
    }

    pub(super) async fn test_config(&self, name: &str, config: &McpServerConfig) -> Result<()> {
        let value_mode = if self.resolver.session_scoped() {
            config.validate_session(name)?;
            McpValueMode::Literal
        } else {
            config.validate(name)?;
            McpValueMode::EnvironmentReferences
        };
        let connection = self
            .connect_with_timeout_mode(name, config.clone(), value_mode, MCP_CONNECTION_TIMEOUT)
            .await?;
        connection.close().await;
        Ok(())
    }
}

pub(super) fn mcp_client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("uri-agent", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::V_2026_07_28)
}

fn expand_template(template: &str, environment: &BTreeMap<String, String>) -> Result<String> {
    let mut output = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let variable = &rest[start + 2..];
        let end = variable
            .find('}')
            .ok_or_else(|| anyhow!("unterminated environment variable template"))?;
        let name = &variable[..end];
        if name.is_empty() {
            bail!("environment variable template name cannot be empty");
        }
        let value = environment
            .get(name)
            .ok_or_else(|| anyhow!("missing Agent environment variable {name:?}"))?;
        output.push_str(value);
        rest = &variable[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

struct ProcessTreeTransport {
    inner: AsyncRwTransport<RoleClient, ChildStdout, ChildStdin>,
    child: Child,
    tree: ProcessTree,
}

impl Transport<RoleClient> for ProcessTreeTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.inner.receive().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await?;
        self.tree.terminate_and_wait(&mut self.child).await?;
        Ok(())
    }
}

impl McpRuntime {
    pub(super) async fn call_tool(
        self: Arc<Self>,
        identity: &str,
        name: String,
        arguments: JsonObject,
    ) -> Result<ProtocolOutput> {
        let connection = self.connection(identity).await?;
        let result = connection
            .peer
            .call_tool(CallToolRequestParams::new(name.clone()).with_arguments(arguments))
            .await?;
        self.format_tool_result(&name, result).await
    }

    async fn format_tool_result(
        &self,
        name: &str,
        result: CallToolResult,
    ) -> Result<ProtocolOutput> {
        let json = tool_call_json(&result);
        let mut output = self.format_content_blocks(name, result.content).await?;
        if let Some(structured) = result.structured_content {
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            output.push_str(&render_json(&structured)?);
        }
        if output.is_empty() {
            output.push_str("(no output)");
        }
        let output = format!("{UNTRUSTED_MCP_CONTENT}\n\n{output}");
        if result.is_error.unwrap_or(false) {
            bail!(output);
        }
        Ok(ProtocolOutput::new(output.into_bytes(), json, Vec::new()))
    }

    pub(super) async fn format_prompt_result(&self, result: GetPromptResult) -> Result<Vec<u8>> {
        let mut output = result
            .description
            .map(|description| format!("{description}\n\n"))
            .unwrap_or_default();
        for message in result.messages {
            output.push_str(&format!("## {:?}\n\n", message.role));
            output.push_str(
                &self
                    .format_content_blocks("mcp-prompt", vec![message.content])
                    .await?,
            );
            output.push_str("\n\n");
        }
        Ok(format!("{UNTRUSTED_MCP_CONTENT}\n\n{}", output.trim_end()).into_bytes())
    }

    pub(super) async fn format_resource_result(
        &self,
        result: ReadResourceResult,
    ) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        for content in result.contents {
            match content {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    output.push(format!("## {uri}\n\n{text}"));
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    let bytes = BASE64
                        .decode(blob)
                        .with_context(|| format!("invalid base64 MCP resource {uri}"))?;
                    let extension = extension_for_mime(mime_type.as_deref());
                    let path = self
                        .output
                        .preserve_with_extension(&bytes, "mcp-resource", extension)
                        .await?;
                    output.push(format!("## {uri}\n\nfile://{}", display_path(&path)));
                }
                other => output.push(render_json(&other)?),
            }
        }
        Ok(format!("{UNTRUSTED_MCP_CONTENT}\n\n{}", output.join("\n\n")).into_bytes())
    }

    async fn format_content_blocks(&self, hint: &str, blocks: Vec<ContentBlock>) -> Result<String> {
        let mut output = Vec::new();
        for block in blocks {
            match block {
                ContentBlock::Text(text) => output.push(text.text),
                ContentBlock::Image(image) => {
                    output.push(
                        self.preserve_media(hint, &image.data, &image.mime_type, "image")
                            .await?,
                    );
                }
                ContentBlock::Audio(audio) => {
                    output.push(
                        self.preserve_media(hint, &audio.data, &audio.mime_type, "audio")
                            .await?,
                    );
                }
                ContentBlock::Resource(resource) => {
                    output.push(
                        String::from_utf8(
                            self.format_resource_result(ReadResourceResult::new(vec![
                                resource.resource,
                            ]))
                            .await?,
                        )
                        .context("MCP resource output was not UTF-8")?,
                    );
                }
                ContentBlock::ResourceLink(resource) => {
                    output.push(format!("{} ({})", resource.name, resource.uri));
                }
                other => output.push(render_json(&other)?),
            }
        }
        Ok(output.join("\n\n"))
    }

    /// Decodes base64 media content and preserves it as a bounded output file.
    async fn preserve_media(
        &self,
        hint: &str,
        data: &str,
        mime_type: &str,
        label: &str,
    ) -> Result<String> {
        let bytes = BASE64
            .decode(data)
            .with_context(|| format!("invalid base64 MCP {label}"))?;
        let path = self
            .output
            .preserve_with_extension(&bytes, hint, extension_for_mime(Some(mime_type)))
            .await?;
        Ok(format!("file://{}", display_path(&path)))
    }
}

/// The structured output a tool call exposes as step `.json`:
/// `structuredContent` when the server provides one; otherwise the parsed
/// single text content block when its text is JSON; otherwise none.
pub(super) fn tool_call_json(result: &CallToolResult) -> Option<Value> {
    if let Some(structured) = result.structured_content.as_ref() {
        return Some(structured.clone());
    }
    let mut blocks = result.content.iter();
    match (blocks.next(), blocks.next()) {
        (Some(ContentBlock::Text(text)), None) => serde_json::from_str(&text.text).ok(),
        _ => None,
    }
}

fn extension_for_mime(mime: Option<&str>) -> &'static str {
    match mime
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
    {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "application/json" => "json",
        "text/plain" => "txt",
        _ => "bin",
    }
}
