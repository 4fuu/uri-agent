//! Model-tool registration and the linked-plugin boundary. A plugin declares
//! the tools it installs; `PluginRegistry::install` verifies the declaration
//! against the installed registry so a tool cannot appear or disappear
//! silently.

use crate::config::AgentEnvironment;
use crate::tool_download::BinaryDownloader;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use rig::completion::ToolDefinition;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub use crate::tool_download::{BinaryDownload, DownloadArchive};

#[derive(Clone, Debug, PartialEq)]
pub struct ModelToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ModelToolDescriptor {
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelToolOutput {
    output: String,
}

impl ModelToolOutput {
    pub fn new(output: String) -> Self {
        Self { output }
    }

    pub fn text(&self) -> &str {
        &self.output
    }

    pub(crate) fn into_text(self) -> String {
        self.output
    }
}

impl From<String> for ModelToolOutput {
    fn from(output: String) -> Self {
        Self::new(output)
    }
}

impl From<&str> for ModelToolOutput {
    fn from(output: &str) -> Self {
        Self::new(output.to_string())
    }
}

#[async_trait]
pub trait ModelTool: Send + Sync {
    fn descriptor(&self) -> ModelToolDescriptor;

    async fn execute(&self, arguments: &Value) -> Result<ModelToolOutput>;
}

#[derive(Default)]
pub struct ModelToolRegistry {
    tools: BTreeMap<String, Arc<dyn ModelTool>>,
    allowed: RwLock<Option<HashSet<String>>>,
}

impl ModelToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: impl ModelTool + 'static) -> Result<()> {
        self.register_arc(Arc::new(tool))
    }

    fn register_arc(&mut self, tool: Arc<dyn ModelTool>) -> Result<()> {
        let descriptor = tool.descriptor();
        validate_model_tool_descriptor(&descriptor)?;
        if self.tools.contains_key(&descriptor.name) {
            bail!("model tool name is already registered: {}", descriptor.name);
        }
        self.tools.insert(descriptor.name, tool);
        Ok(())
    }

    pub fn select(&self, names: Option<&[String]>) -> Result<()> {
        let selected = self.selection(names)?;
        *self
            .allowed
            .write()
            .expect("model tool selection lock poisoned") = selected;
        Ok(())
    }

    pub fn validate_selection(&self, names: &[String]) -> Result<()> {
        self.selection(Some(names)).map(|_| ())
    }

    fn selection(&self, names: Option<&[String]>) -> Result<Option<HashSet<String>>> {
        match names {
            None => Ok(None),
            Some(names) => {
                let selected = names.iter().cloned().collect::<HashSet<_>>();
                if selected.len() != names.len() {
                    bail!("Agent model tool is selected more than once");
                }
                if let Some(name) = selected.iter().find(|name| !self.tools.contains_key(*name)) {
                    bail!("unknown Agent model tool: {name}");
                }
                Ok(Some(selected))
            }
        }
    }

    fn all_descriptors(&self) -> Vec<ModelToolDescriptor> {
        self.tools
            .values()
            .map(|tool| tool.descriptor())
            .collect::<Vec<_>>()
    }

    pub(crate) fn descriptors_for(
        &self,
        names: Option<&[String]>,
    ) -> Result<Vec<ModelToolDescriptor>> {
        let selected = names
            .map(|names| {
                let selected = names.iter().cloned().collect::<HashSet<_>>();
                if selected.len() != names.len() {
                    bail!("Agent model tool is selected more than once");
                }
                if let Some(name) = selected.iter().find(|name| !self.tools.contains_key(*name)) {
                    bail!("unknown Agent model tool: {name}");
                }
                Ok::<_, anyhow::Error>(selected)
            })
            .transpose()?;
        let mut descriptors = self.all_descriptors();
        if let Some(selected) = selected {
            descriptors.retain(|descriptor| selected.contains(&descriptor.name));
        }
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(descriptors)
    }

    pub fn descriptors(&self) -> Vec<ModelToolDescriptor> {
        let mut descriptors = self.all_descriptors();
        let allowed = self
            .allowed
            .read()
            .expect("model tool selection lock poisoned");
        if let Some(allowed) = allowed.as_ref() {
            descriptors.retain(|descriptor| allowed.contains(&descriptor.name));
        }
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        descriptors
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.descriptors()
            .into_iter()
            .map(|descriptor| descriptor.definition())
            .collect()
    }

    pub async fn dispatch(&self, name: &str, arguments: &Value) -> Result<ModelToolOutput> {
        {
            let allowed = self
                .allowed
                .read()
                .expect("model tool selection lock poisoned");
            if let Some(selected) = allowed.as_ref()
                && !selected.contains(name)
            {
                bail!("unknown model tool: {name}");
            }
        }
        let tool = self
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown model tool: {name}"))?;
        tool.execute(arguments).await
    }
}

fn validate_model_tool_descriptor(descriptor: &ModelToolDescriptor) -> Result<()> {
    if descriptor.name.is_empty()
        || !descriptor
            .name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        bail!(
            "model tool name must be a nonempty identifier: {}",
            descriptor.name
        );
    }
    if descriptor.description.trim().is_empty() {
        bail!("model tool {} has an empty description", descriptor.name);
    }
    let Some(schema) = descriptor.parameters.as_object() else {
        bail!("model tool {} schema must be an object", descriptor.name);
    };
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        bail!(
            "model tool {} schema must have type object",
            descriptor.name
        );
    }
    if schema.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        bail!(
            "model tool {} schema must set additionalProperties false",
            descriptor.name
        );
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        bail!("model tool {} schema must have properties", descriptor.name);
    };
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        bail!(
            "model tool {} schema must have a required array",
            descriptor.name
        );
    };
    let mut names = HashSet::new();
    for value in required {
        let Some(name) = value.as_str() else {
            bail!(
                "model tool {} required entries must be strings",
                descriptor.name
            );
        };
        if !properties.contains_key(name) {
            bail!(
                "model tool {} requires undeclared property {name}",
                descriptor.name
            );
        }
        if !names.insert(name) {
            bail!(
                "model tool {} requires property {name} more than once",
                descriptor.name
            );
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct PluginEnvironment {
    environment: Arc<AgentEnvironment>,
}

impl PluginEnvironment {
    pub(crate) fn new(environment: Arc<AgentEnvironment>) -> Self {
        Self { environment }
    }

    #[cfg(test)]
    pub async fn get(&self, name: &str) -> Result<Option<String>> {
        self.environment.get(name).await
    }

    pub async fn snapshot(&self) -> BTreeMap<String, String> {
        self.environment.snapshot().await
    }

    pub fn revision(&self) -> u64 {
        self.environment.revision()
    }

    pub async fn snapshot_with_revision(&self) -> (BTreeMap<String, String>, u64) {
        self.environment.snapshot_with_revision().await
    }
}

#[derive(Clone)]
pub struct PluginDownloads {
    downloader: BinaryDownloader,
}

impl PluginDownloads {
    fn new() -> Self {
        Self {
            downloader: BinaryDownloader::default(),
        }
    }

    pub async fn ensure(&self, spec: &BinaryDownload) -> Result<PathBuf> {
        self.downloader.ensure(spec).await
    }
}

pub struct PluginHost<'a> {
    pub model_tools: &'a mut ModelToolRegistry,
    environment: Arc<AgentEnvironment>,
    downloads: PluginDownloads,
}

impl<'a> PluginHost<'a> {
    pub fn new(model_tools: &'a mut ModelToolRegistry, environment: Arc<AgentEnvironment>) -> Self {
        Self {
            model_tools,
            environment,
            downloads: PluginDownloads::new(),
        }
    }

    pub fn environment(&self) -> Result<PluginEnvironment> {
        Ok(PluginEnvironment::new(self.environment.clone()))
    }

    pub fn downloads(&self) -> Result<PluginDownloads> {
        Ok(self.downloads.clone())
    }
}

#[async_trait]
pub trait Plugin: Send + Sync {
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        Vec::new()
    }

    fn register(&self, _host: &mut PluginHost<'_>) -> Result<()> {
        Ok(())
    }

    fn system_prompt_fragments(&self) -> Vec<String> {
        Vec::new()
    }

    fn startup_notices(&self) -> Vec<String> {
        Vec::new()
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct PluginRegistry {
    plugins: Vec<Arc<dyn Plugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, plugin: impl Plugin + 'static) {
        self.plugins.push(Arc::new(plugin));
    }

    pub fn model_tool_descriptors(&self) -> Result<Vec<ModelToolDescriptor>> {
        let mut descriptors = Vec::new();
        let mut names = HashSet::new();
        for plugin in &self.plugins {
            for descriptor in plugin.model_tool_descriptors() {
                validate_model_tool_descriptor(&descriptor)?;
                if !names.insert(descriptor.name.clone()) {
                    bail!(
                        "plugin model tool name is declared more than once: {}",
                        descriptor.name
                    );
                }
                descriptors.push(descriptor);
            }
        }
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(descriptors)
    }

    pub fn system_prompt_fragments(&self) -> Result<Vec<String>> {
        Ok(self
            .plugins
            .iter()
            .flat_map(|plugin| plugin.system_prompt_fragments())
            .collect())
    }

    pub fn startup_notices(&self) -> Vec<String> {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.startup_notices())
            .collect()
    }

    pub fn install(&self, host: &mut PluginHost<'_>) -> Result<()> {
        let expected_tools = self
            .model_tool_descriptors()?
            .into_iter()
            .map(|descriptor| (descriptor.name.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        let before = host
            .model_tools
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<HashSet<_>>();
        if let Some(name) = expected_tools.keys().find(|name| before.contains(*name)) {
            bail!("plugin model tool name is already registered: {name}");
        }
        for plugin in &self.plugins {
            plugin.register(host)?;
        }
        let installed = host
            .model_tools
            .descriptors()
            .into_iter()
            .filter(|descriptor| !before.contains(&descriptor.name))
            .map(|descriptor| (descriptor.name.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        if installed != expected_tools {
            bail!("plugin model tool declarations do not match installed model tools");
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut failure = None;
        for plugin in self.plugins.iter().rev() {
            if let Err(error) = plugin.shutdown().await
                && failure.is_none()
            {
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl ModelTool for Echo {
        fn descriptor(&self) -> ModelToolDescriptor {
            ModelToolDescriptor {
                name: "echo".to_string(),
                description: "Echo text.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                }),
            }
        }

        async fn execute(&self, arguments: &Value) -> Result<ModelToolOutput> {
            Ok(format!("echo:{}", arguments["text"].as_str().unwrap_or_default()).into())
        }
    }

    struct EchoPlugin;

    impl Plugin for EchoPlugin {
        fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
            vec![Echo.descriptor()]
        }

        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            host.model_tools.register(Echo)
        }
    }

    #[tokio::test]
    async fn registry_selects_and_dispatches_only_installed_tools() {
        let mut plugins = PluginRegistry::new();
        plugins.add(EchoPlugin);
        let mut tools = ModelToolRegistry::new();
        let temp = tempfile::tempdir().unwrap();
        let environment = Arc::new(AgentEnvironment::load(temp.path()).await.unwrap());
        plugins
            .install(&mut PluginHost::new(&mut tools, environment))
            .unwrap();
        tools.select(Some(&["echo".to_string()])).unwrap();
        let output = tools
            .dispatch("echo", &json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(output.text(), "echo:hi");
        assert!(tools.dispatch("missing", &json!({})).await.is_err());
    }
}
