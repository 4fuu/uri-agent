use crate::config::{AgentEnvironment, ConfigManager};
use crate::protocol::{ProtocolDescriptor, ProtocolRegistry, validate_descriptor};
use crate::tool_download::BinaryDownloader;
pub use crate::tool_download::{BinaryDownload, DownloadArchive};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rig::completion::ToolDefinition;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreCommand {
    Compose,
    Copy,
    Tasks,
    Protocols,
    Status,
    Models,
    RefreshCatalog,
    Effort,
    Settings,
    Login,
    Logout,
    Resume,
    Search,
    NewSession,
    Compact,
    Help,
    Quit,
    SetEnvironment,
    SetTerminal,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandTarget {
    Core(CoreCommand),
    Panel(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub id: String,
    pub title: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub target: CommandTarget,
}

impl CommandSpec {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
        aliases: impl IntoIterator<Item = impl Into<String>>,
        target: CommandTarget,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: description.into(),
            aliases: aliases.into_iter().map(Into::into).collect(),
            target,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedCommand {
    pub spec: CommandSpec,
    pub arguments: String,
}

#[derive(Default)]
pub struct CommandRegistry {
    commands: BTreeMap<String, CommandSpec>,
    names: HashMap<String, String>,
}

impl CommandRegistry {
    pub fn with_core_commands() -> Self {
        let mut registry = Self::default();
        for spec in core_commands() {
            registry
                .register(spec)
                .expect("built-in command names are valid and unique");
        }
        registry
    }

    pub fn register(&mut self, spec: CommandSpec) -> Result<()> {
        validate_name(&spec.id)?;
        if spec.title.trim().is_empty() || spec.description.trim().is_empty() {
            bail!("command {} requires a title and description", spec.id);
        }
        let mut names = Vec::with_capacity(spec.aliases.len() + 1);
        names.push(spec.id.clone());
        for alias in &spec.aliases {
            validate_name(alias)?;
            names.push(alias.clone());
        }
        let mut unique = HashSet::new();
        for name in &names {
            if !unique.insert(name) {
                bail!("command {} repeats name or alias {name:?}", spec.id);
            }
            if let Some(owner) = self.names.get(name) {
                bail!("command name or alias {name:?} is already registered by {owner}");
            }
        }
        let id = spec.id.clone();
        for name in names {
            self.names.insert(name, id.clone());
        }
        self.commands.insert(id, spec);
        Ok(())
    }

    pub fn resolve(&self, input: &str) -> Option<ResolvedCommand> {
        let input = input.trim().trim_start_matches([':', '：']).trim_start();
        let split = input.find(char::is_whitespace).unwrap_or(input.len());
        let name = &input[..split];
        let id = self.names.get(name)?;
        Some(ResolvedCommand {
            spec: self.commands.get(id)?.clone(),
            arguments: input[split..].trim().to_string(),
        })
    }

    pub fn target_for_action(&self, action: &str) -> Option<CommandTarget> {
        self.commands.get(action).map(|spec| spec.target.clone())
    }

    pub fn list(&self) -> Vec<CommandSpec> {
        self.commands.values().cloned().collect()
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid command name or alias: {name:?}");
    }
    Ok(())
}

fn core_commands() -> Vec<CommandSpec> {
    use CoreCommand::*;
    vec![
        CommandSpec::new(
            "insert",
            "Compose message",
            "open the floating composer",
            ["compose"],
            CommandTarget::Core(Compose),
        ),
        CommandSpec::new(
            "copy",
            "Copy panel",
            "copy the selection or visible panel with OSC52",
            ["yank"],
            CommandTarget::Core(Copy),
        ),
        CommandSpec::new(
            "tasks",
            "Managed tasks",
            "inspect asynchronous protocol work",
            std::iter::empty::<&str>(),
            CommandTarget::Core(Tasks),
        ),
        CommandSpec::new(
            "protocols",
            "Protocols",
            "show registered read and exec routes",
            std::iter::empty::<&str>(),
            CommandTarget::Core(Protocols),
        ),
        CommandSpec::new(
            "status",
            "Session status",
            "expand project, model, usage, and plugin status",
            std::iter::empty::<&str>(),
            CommandTarget::Core(Status),
        ),
        CommandSpec::new(
            "model",
            "Select model",
            "search all runnable models from the Pi catalog",
            ["models"],
            CommandTarget::Core(Models),
        ),
        CommandSpec::new(
            "refresh-catalog",
            "Refresh model catalog",
            "force-refresh and apply model configurations from Pi",
            std::iter::empty::<&str>(),
            CommandTarget::Core(RefreshCatalog),
        ),
        CommandSpec::new(
            "effort",
            "Thinking effort",
            "select thinking effort for the active model",
            ["thinking"],
            CommandTarget::Core(Effort),
        ),
        CommandSpec::new(
            "settings",
            "Settings",
            "model, credentials, thinking, output limit, and Agent environment",
            std::iter::empty::<&str>(),
            CommandTarget::Core(Settings),
        ),
        CommandSpec::new(
            "login",
            "Log in",
            "save an API key or complete provider OAuth",
            std::iter::empty::<&str>(),
            CommandTarget::Core(Login),
        ),
        CommandSpec::new(
            "logout",
            "Log out",
            "remove a stored provider credential",
            std::iter::empty::<&str>(),
            CommandTarget::Core(Logout),
        ),
        CommandSpec::new(
            "resume",
            "Resume session",
            "switch to another session in this project",
            ["sessions"],
            CommandTarget::Core(Resume),
        ),
        CommandSpec::new(
            "search",
            "Search conversation",
            "find text already shown in this conversation",
            ["find"],
            CommandTarget::Core(Search),
        ),
        CommandSpec::new(
            "new",
            "New session",
            "start a new session in this project",
            std::iter::empty::<&str>(),
            CommandTarget::Core(NewSession),
        ),
        CommandSpec::new(
            "compact",
            "Compact context",
            "summarize older model context and keep raw history",
            std::iter::empty::<&str>(),
            CommandTarget::Core(Compact),
        ),
        CommandSpec::new(
            "help",
            "Help",
            "active keymap and command reference",
            std::iter::empty::<&str>(),
            CommandTarget::Core(Help),
        ),
        CommandSpec::new(
            "quit",
            "Quit",
            "close URI Agent",
            ["q"],
            CommandTarget::Core(Quit),
        ),
        CommandSpec::new(
            "set-env",
            "Add environment variable",
            "add or replace an Agent environment variable with a masked value prompt",
            ["environment-add"],
            CommandTarget::Core(SetEnvironment),
        ),
        CommandSpec::new(
            "set-terminal",
            "Set default terminal",
            "save the command used by :terminal",
            ["terminal-set"],
            CommandTarget::Core(SetTerminal),
        ),
        CommandSpec::new(
            "terminal",
            "Open terminal",
            "open the default terminal in a float",
            ["term"],
            CommandTarget::Core(Terminal),
        ),
    ]
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiDocument {
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct TuiPanelContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub arguments: String,
}

#[async_trait]
pub trait TuiPanelProvider: Send + Sync {
    async fn open(&self, context: TuiPanelContext) -> Result<TuiDocument>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuiTextPosition {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuiTextRange {
    pub start: TuiTextPosition,
    pub end: TuiTextPosition,
}

#[derive(Clone, Debug)]
pub struct TuiCompletionContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub lines: Vec<String>,
    pub cursor: TuiTextPosition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiCompletionItem {
    pub insert_text: String,
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiCompletions {
    pub replacement: TuiTextRange,
    pub items: Vec<TuiCompletionItem>,
}

#[async_trait]
pub trait TuiCompletionProvider: Send + Sync {
    async fn complete(&self, context: &TuiCompletionContext) -> Result<Option<TuiCompletions>>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TuiStatusTone {
    #[default]
    Default,
    Accent,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiStatusItem {
    pub label: String,
    pub value: String,
    pub tone: TuiStatusTone,
}

impl TuiStatusItem {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            tone: TuiStatusTone::Default,
        }
    }

    pub fn with_tone(mut self, tone: TuiStatusTone) -> Self {
        self.tone = tone;
        self
    }
}

#[derive(Clone, Debug)]
pub struct TuiStatusContext {
    pub cwd: PathBuf,
    pub session_id: String,
    /// Providers may return a richer value when the expanded status panel
    /// requests one.
    pub expanded: bool,
}

/// Keep status providers fast and non-blocking: the TUI evaluates them while
/// drawing each frame. Providers can own shared state when their value changes.
pub trait TuiStatusProvider: Send + Sync {
    fn status(&self, context: &TuiStatusContext) -> Option<TuiStatusItem>;
}

impl<F> TuiStatusProvider for F
where
    F: Fn(&TuiStatusContext) -> Option<TuiStatusItem> + Send + Sync,
{
    fn status(&self, context: &TuiStatusContext) -> Option<TuiStatusItem> {
        self(context)
    }
}

#[derive(Clone)]
pub struct TuiPanelSpec {
    pub id: String,
    pub provider: Arc<dyn TuiPanelProvider>,
}

#[derive(Default)]
pub struct TuiRegistry {
    panels: BTreeMap<String, TuiPanelSpec>,
    status: BTreeMap<String, Arc<dyn TuiStatusProvider>>,
    completions: BTreeMap<String, Arc<dyn TuiCompletionProvider>>,
}

impl TuiRegistry {
    pub fn register_panel(
        &mut self,
        id: impl Into<String>,
        provider: impl TuiPanelProvider + 'static,
    ) -> Result<()> {
        let id = id.into();
        validate_name(&id)?;
        if self.panels.contains_key(&id) {
            bail!("TUI panel is already registered: {id}");
        }
        self.panels.insert(
            id.clone(),
            TuiPanelSpec {
                id,
                provider: Arc::new(provider),
            },
        );
        Ok(())
    }

    pub async fn open_panel(&self, id: &str, context: TuiPanelContext) -> Result<TuiDocument> {
        let Some(panel) = self.panels.get(id) else {
            bail!("unknown TUI panel: {id}");
        };
        panel.provider.open(context).await
    }

    pub fn register_status(
        &mut self,
        id: impl Into<String>,
        provider: impl TuiStatusProvider + 'static,
    ) -> Result<()> {
        let id = id.into();
        validate_name(&id)?;
        if self.status.contains_key(&id) {
            bail!("TUI status provider is already registered: {id}");
        }
        self.status.insert(id, Arc::new(provider));
        Ok(())
    }

    pub fn status_items(&self, context: &TuiStatusContext) -> Vec<TuiStatusItem> {
        self.status
            .values()
            .filter_map(|provider| provider.status(context))
            .collect()
    }

    pub fn register_completion(
        &mut self,
        id: impl Into<String>,
        provider: impl TuiCompletionProvider + 'static,
    ) -> Result<()> {
        let id = id.into();
        validate_name(&id)?;
        if self.completions.contains_key(&id) {
            bail!("TUI completion provider is already registered: {id}");
        }
        self.completions.insert(id, Arc::new(provider));
        Ok(())
    }

    pub async fn completions(
        &self,
        context: &TuiCompletionContext,
    ) -> Result<Option<TuiCompletions>> {
        for provider in self.completions.values() {
            if let Some(completions) = provider.complete(context).await?
                && !completions.items.is_empty()
            {
                return Ok(Some(completions));
            }
        }
        Ok(None)
    }
}

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

#[async_trait]
pub trait ModelTool: Send + Sync {
    fn descriptor(&self) -> ModelToolDescriptor;

    async fn execute(&self, arguments: &Value, protocols: &ProtocolRegistry) -> Result<String>;
}

pub trait DynamicModelToolSource: Send + Sync {
    fn descriptors(&self) -> Vec<ModelToolDescriptor>;
    fn tool(&self, name: &str) -> Option<Arc<dyn ModelTool>>;
}

#[derive(Default)]
pub struct ModelToolRegistry {
    tools: BTreeMap<String, Arc<dyn ModelTool>>,
    dynamic: Option<Arc<dyn DynamicModelToolSource>>,
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

    pub fn set_dynamic_source(&mut self, source: Arc<dyn DynamicModelToolSource>) -> Result<()> {
        if self.dynamic.is_some() {
            bail!("dynamic model tool source is already registered");
        }
        let mut names = HashSet::new();
        for descriptor in source.descriptors() {
            validate_model_tool_descriptor(&descriptor)?;
            if self.tools.contains_key(&descriptor.name) || !names.insert(descriptor.name.clone()) {
                bail!(
                    "dynamic model tool name is already registered: {}",
                    descriptor.name
                );
            }
        }
        self.dynamic = Some(source);
        Ok(())
    }

    pub fn descriptors(&self) -> Vec<ModelToolDescriptor> {
        let mut descriptors = self
            .tools
            .values()
            .map(|tool| tool.descriptor())
            .collect::<Vec<_>>();
        if let Some(dynamic) = &self.dynamic {
            descriptors.extend(dynamic.descriptors());
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

    pub async fn dispatch(
        &self,
        name: &str,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .cloned()
            .or_else(|| self.dynamic.as_ref().and_then(|source| source.tool(name)));
        let tool = tool.ok_or_else(|| anyhow::anyhow!("unknown model tool: {name}"))?;
        tool.execute(arguments, protocols).await
    }
}

pub fn validate_model_tool_descriptor(descriptor: &ModelToolDescriptor) -> Result<()> {
    validate_name(&descriptor.name)?;
    if descriptor.description.trim().is_empty() {
        bail!("model tool {} requires a description", descriptor.name);
    }
    let schema = descriptor.parameters.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "model tool {} parameters must be a JSON Schema object",
            descriptor.name
        )
    })?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        bail!(
            "model tool {} parameters must declare type object",
            descriptor.name
        );
    }
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "model tool {} parameters must declare an object properties map",
                descriptor.name
            )
        })?;
    if schema.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        bail!(
            "model tool {} parameters must set additionalProperties to false",
            descriptor.name
        );
    }
    if let Some(required) = schema.get("required") {
        let required = required.as_array().ok_or_else(|| {
            anyhow::anyhow!("model tool {} required must be an array", descriptor.name)
        })?;
        let mut names = HashSet::new();
        for name in required {
            let name = name.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "model tool {} required names must be strings",
                    descriptor.name
                )
            })?;
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
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PluginPermission {
    /// Read the user-managed Agent environment. This declaration is an audit
    /// marker for trusted plugin code, not an interactive approval boundary.
    Environment,
    /// Resolve saved or provider-environment API keys. This declaration is an
    /// audit marker for trusted plugin code, not an interactive approval boundary.
    Credentials,
    /// Download and cache a pinned external executable. This declaration is an
    /// audit marker for trusted plugin code, not an interactive approval boundary.
    Downloads,
}

#[derive(Clone)]
pub struct PluginEnvironment {
    environment: Arc<AgentEnvironment>,
}

impl PluginEnvironment {
    pub(crate) fn new(environment: Arc<AgentEnvironment>) -> Self {
        Self { environment }
    }

    pub async fn get(&self, name: &str) -> Result<Option<String>> {
        self.environment.get(name).await
    }

    pub async fn snapshot(&self) -> BTreeMap<String, String> {
        self.environment.snapshot().await
    }
}

#[derive(Clone)]
pub struct PluginCredentials {
    manager: Arc<ConfigManager>,
}

impl PluginCredentials {
    pub(crate) fn new(manager: Arc<ConfigManager>) -> Self {
        Self { manager }
    }

    pub async fn api_key(&self, provider: &str) -> Result<Option<String>> {
        self.manager.provider_api_key(provider).await
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

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new()
    }

    pub async fn ensure(&self, spec: &BinaryDownload) -> Result<PathBuf> {
        self.downloader.ensure(spec).await
    }
}

pub struct PluginHost<'a> {
    pub protocols: &'a mut ProtocolRegistry,
    pub model_tools: &'a mut ModelToolRegistry,
    pub commands: &'a mut CommandRegistry,
    pub tui: &'a mut TuiRegistry,
    environment: Arc<AgentEnvironment>,
    credentials: Option<Arc<ConfigManager>>,
    downloads: PluginDownloads,
    permissions: HashSet<PluginPermission>,
}

impl<'a> PluginHost<'a> {
    pub fn new(
        protocols: &'a mut ProtocolRegistry,
        model_tools: &'a mut ModelToolRegistry,
        commands: &'a mut CommandRegistry,
        tui: &'a mut TuiRegistry,
        environment: Arc<AgentEnvironment>,
    ) -> Self {
        Self {
            protocols,
            model_tools,
            commands,
            tui,
            environment,
            credentials: None,
            downloads: PluginDownloads::new(),
            permissions: HashSet::new(),
        }
    }

    pub fn with_credentials(mut self, manager: Arc<ConfigManager>) -> Self {
        self.credentials = Some(manager);
        self
    }

    pub fn environment(&self) -> Result<PluginEnvironment> {
        if !self.permissions.contains(&PluginPermission::Environment) {
            bail!("plugin did not request Agent environment access");
        }
        Ok(PluginEnvironment::new(self.environment.clone()))
    }

    pub fn credentials(&self) -> Result<PluginCredentials> {
        if !self.permissions.contains(&PluginPermission::Credentials) {
            bail!("plugin did not request credential access");
        }
        let manager = self
            .credentials
            .clone()
            .ok_or_else(|| anyhow::anyhow!("plugin credential access is not attached"))?;
        Ok(PluginCredentials::new(manager))
    }

    pub fn downloads(&self) -> Result<PluginDownloads> {
        if !self.permissions.contains(&PluginPermission::Downloads) {
            bail!("plugin did not request binary download access");
        }
        Ok(self.downloads.clone())
    }
}

pub trait Plugin: Send + Sync {
    /// Protocols contributed by this plugin. These declarations are used to
    /// freeze a new session's system prompt before the runtime registries exist.
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        Vec::new()
    }

    /// Direct model tools contributed by this plugin. Prefer a direct tool
    /// when structured arguments would otherwise require nested serialization.
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        Vec::new()
    }

    /// Notices contributed by this plugin for the current application startup.
    fn startup_notices(&self) -> Vec<String> {
        Vec::new()
    }

    /// Optional content appended to a new session's system prompt before it is
    /// frozen. A plugin may contribute prompt content without adding a protocol.
    fn system_prompt_fragment(&self) -> Result<Option<String>> {
        Ok(None)
    }

    /// Permissions requested by this trusted plugin. Requests are explicit so
    /// source review can find sensitive host access without an approval flow.
    fn permissions(&self) -> Vec<PluginPermission> {
        Vec::new()
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()>;
}

#[derive(Default)]
pub struct PluginRegistry {
    plugins: Vec<Box<dyn Plugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, plugin: impl Plugin + 'static) {
        self.plugins.push(Box::new(plugin));
    }

    pub fn protocol_descriptors(&self) -> Result<Vec<ProtocolDescriptor>> {
        let mut descriptors = Vec::new();
        let mut names = HashSet::new();
        for plugin in &self.plugins {
            for descriptor in plugin.protocol_descriptors() {
                validate_descriptor(&descriptor)?;
                if !names.insert(descriptor.name.clone()) {
                    bail!(
                        "plugin protocol name is declared more than once: {}",
                        descriptor.name
                    );
                }
                descriptors.push(descriptor);
            }
        }
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(descriptors)
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

    pub fn startup_notices(&self) -> Vec<String> {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.startup_notices())
            .collect()
    }

    pub fn system_prompt_fragments(&self) -> Result<Vec<String>> {
        let mut fragments = Vec::new();
        for plugin in &self.plugins {
            if let Some(fragment) = plugin.system_prompt_fragment()? {
                fragments.push(fragment);
            }
        }
        Ok(fragments)
    }

    pub fn install(&self, host: &mut PluginHost<'_>) -> Result<()> {
        let expected_protocols = self
            .protocol_descriptors()?
            .into_iter()
            .map(|descriptor| (descriptor.name.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        let before = host
            .protocols
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<HashSet<_>>();
        if let Some(name) = expected_protocols
            .keys()
            .find(|name| before.contains(*name))
        {
            bail!("plugin protocol name is already registered: {name}");
        }
        let expected_tools = self
            .model_tool_descriptors()?
            .into_iter()
            .map(|descriptor| (descriptor.name.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        let tools_before = host
            .model_tools
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<HashSet<_>>();
        if let Some(name) = expected_tools
            .keys()
            .find(|name| tools_before.contains(*name))
        {
            bail!("plugin model tool name is already registered: {name}");
        }

        for plugin in &self.plugins {
            let permissions = plugin.permissions();
            let unique = permissions.iter().copied().collect::<HashSet<_>>();
            if unique.len() != permissions.len() {
                bail!("plugin declares the same permission more than once");
            }
            host.permissions = unique;
            let result = plugin.register(host).context("failed to register plugin");
            host.permissions.clear();
            result?;
        }

        let installed = host
            .protocols
            .descriptors()
            .into_iter()
            .filter(|descriptor| !before.contains(&descriptor.name))
            .map(|descriptor| (descriptor.name.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        if installed != expected_protocols {
            bail!("plugin protocol declarations do not match installed protocols");
        }
        let installed_tools = host
            .model_tools
            .descriptors()
            .into_iter()
            .filter(|descriptor| !tools_before.contains(&descriptor.name))
            .map(|descriptor| (descriptor.name.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        if installed_tools != expected_tools {
            bail!("plugin model tool declarations do not match installed model tools");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputStore;
    use crate::protocol::{Protocol, ProtocolDescriptor};
    use crate::task::TaskManager;

    struct StaticPanel;

    #[async_trait]
    impl TuiPanelProvider for StaticPanel {
        async fn open(&self, context: TuiPanelContext) -> Result<TuiDocument> {
            Ok(TuiDocument {
                title: "Plugin panel".to_string(),
                body: format!("{} {}", context.session_id, context.arguments),
            })
        }
    }

    #[test]
    fn command_ids_and_aliases_resolve_through_one_registry() {
        let registry = CommandRegistry::with_core_commands();
        assert_eq!(
            registry.resolve(":model").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Models)
        );
        assert_eq!(
            registry.resolve("：login").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Login)
        );
        assert_eq!(
            registry.resolve(":status").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Status)
        );
        assert_eq!(
            registry.resolve(":quit").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Quit)
        );
        assert_eq!(
            registry.resolve(":q").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Quit)
        );
        assert_eq!(
            registry.resolve(":insert").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Compose)
        );
        assert_eq!(
            registry.resolve(":compose").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Compose)
        );
        assert_eq!(
            registry.resolve(":yank").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Copy)
        );
        assert_eq!(
            registry.resolve(":models").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Models)
        );
        assert_eq!(
            registry.resolve(":refresh-catalog").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::RefreshCatalog)
        );
        assert_eq!(
            registry.resolve(":term").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Terminal)
        );
        assert_eq!(registry.resolve("compact now").unwrap().arguments, "now");
        assert_eq!(
            registry.resolve(":thinking high").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Effort)
        );
        assert_eq!(
            registry.resolve(":sessions").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Resume)
        );
        assert_eq!(
            registry.resolve(":search").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Search)
        );
        assert_eq!(
            registry.resolve(":find").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Search)
        );
        assert_eq!(
            registry.resolve(":terminal-set").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::SetTerminal)
        );
        assert_eq!(registry.resolve(":effort high").unwrap().arguments, "high");
    }

    #[test]
    fn command_conflicts_are_rejected_before_the_tui_starts() {
        let mut registry = CommandRegistry::with_core_commands();
        let error = registry
            .register(CommandSpec::new(
                "other",
                "Other",
                "Other command",
                ["settings"],
                CommandTarget::Panel("other".to_string()),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("already registered"));

        let error = CommandRegistry::default()
            .register(CommandSpec::new(
                "duplicate",
                "Duplicate",
                "Duplicate command",
                ["duplicate"],
                CommandTarget::Panel("duplicate".to_string()),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("repeats"));
    }

    #[tokio::test]
    async fn registered_tui_panels_receive_session_context_and_arguments() {
        let mut registry = TuiRegistry::default();
        registry.register_panel("demo", StaticPanel).unwrap();
        let document = registry
            .open_panel(
                "demo",
                TuiPanelContext {
                    cwd: PathBuf::from("/work"),
                    session_id: "session".to_string(),
                    arguments: "argument".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(document.title, "Plugin panel");
        assert_eq!(document.body, "session argument");
    }

    #[test]
    fn registered_status_providers_receive_context_and_reject_collisions() {
        let mut registry = TuiRegistry::default();
        registry
            .register_status("build", |context: &TuiStatusContext| {
                Some(
                    TuiStatusItem::new(
                        "build",
                        if context.expanded {
                            format!("clean · {}", context.session_id)
                        } else {
                            "clean".to_string()
                        },
                    )
                    .with_tone(TuiStatusTone::Accent),
                )
            })
            .unwrap();
        let context = TuiStatusContext {
            cwd: PathBuf::from("/work"),
            session_id: "session".to_string(),
            expanded: true,
        };
        assert_eq!(
            registry.status_items(&context),
            vec![TuiStatusItem::new("build", "clean · session").with_tone(TuiStatusTone::Accent)]
        );

        let error = registry
            .register_status("build", |_context: &TuiStatusContext| None)
            .unwrap_err();
        assert!(error.to_string().contains("already registered"));
    }

    #[derive(Clone)]
    struct DeclaredProtocolPlugin {
        declares_protocol: bool,
    }

    struct PromptOnlyPlugin;

    struct EnvironmentPlugin {
        requests_environment: bool,
        environment: Arc<std::sync::OnceLock<PluginEnvironment>>,
    }

    struct CredentialPlugin {
        requests_credentials: bool,
        credentials: Arc<std::sync::OnceLock<PluginCredentials>>,
    }

    #[derive(Clone)]
    struct DeclaredModelToolPlugin {
        declares_tool: bool,
    }

    struct DownloadPlugin {
        requests_downloads: bool,
        downloads: Arc<std::sync::OnceLock<PluginDownloads>>,
    }

    #[async_trait]
    impl ModelTool for DeclaredModelToolPlugin {
        fn descriptor(&self) -> ModelToolDescriptor {
            ModelToolDescriptor {
                name: "declared_tool".to_string(),
                description: "Declared test model tool".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            }
        }

        async fn execute(
            &self,
            _arguments: &Value,
            _protocols: &ProtocolRegistry,
        ) -> Result<String> {
            Ok("ok".to_string())
        }
    }

    impl Plugin for PromptOnlyPlugin {
        fn startup_notices(&self) -> Vec<String> {
            vec!["plugin startup notice".to_string()]
        }

        fn system_prompt_fragment(&self) -> Result<Option<String>> {
            Ok(Some("<prompt-only>content</prompt-only>".to_string()))
        }

        fn register(&self, _host: &mut PluginHost<'_>) -> Result<()> {
            Ok(())
        }
    }

    impl Plugin for EnvironmentPlugin {
        fn permissions(&self) -> Vec<PluginPermission> {
            self.requests_environment
                .then_some(PluginPermission::Environment)
                .into_iter()
                .collect()
        }

        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            self.environment
                .set(host.environment()?)
                .map_err(|_| anyhow::anyhow!("environment was already captured"))
        }
    }

    impl Plugin for CredentialPlugin {
        fn permissions(&self) -> Vec<PluginPermission> {
            self.requests_credentials
                .then_some(PluginPermission::Credentials)
                .into_iter()
                .collect()
        }

        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            self.credentials
                .set(host.credentials()?)
                .map_err(|_| anyhow::anyhow!("credentials were already captured"))
        }
    }

    impl Plugin for DeclaredModelToolPlugin {
        fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
            self.declares_tool
                .then(|| <Self as ModelTool>::descriptor(self))
                .into_iter()
                .collect()
        }

        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            host.model_tools.register(self.clone())
        }
    }

    impl Plugin for DownloadPlugin {
        fn permissions(&self) -> Vec<PluginPermission> {
            self.requests_downloads
                .then_some(PluginPermission::Downloads)
                .into_iter()
                .collect()
        }

        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            self.downloads
                .set(host.downloads()?)
                .map_err(|_| anyhow::anyhow!("download handle was already captured"))
        }
    }

    #[async_trait]
    impl Protocol for DeclaredProtocolPlugin {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: "declared".to_string(),
                description: "Declared test protocol".to_string(),
                can_read: true,
                can_exec: false,
            }
        }
    }

    impl Plugin for DeclaredProtocolPlugin {
        fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
            self.declares_protocol
                .then(|| self.descriptor())
                .into_iter()
                .collect()
        }

        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            host.protocols.register(self.clone())
        }
    }

    async fn empty_host() -> (
        ProtocolRegistry,
        ModelToolRegistry,
        CommandRegistry,
        TuiRegistry,
        PathBuf,
    ) {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let directory = output.directory().to_path_buf();
        (
            ProtocolRegistry::new(output, TaskManager::new()),
            ModelToolRegistry::new(),
            CommandRegistry::with_core_commands(),
            TuiRegistry::default(),
            directory,
        )
    }

    #[test]
    fn plugins_can_contribute_startup_notices_and_prompt_content_without_protocols() {
        let mut plugins = PluginRegistry::new();
        plugins.add(PromptOnlyPlugin);

        assert!(plugins.protocol_descriptors().unwrap().is_empty());
        assert_eq!(
            plugins.system_prompt_fragments().unwrap(),
            vec!["<prompt-only>content</prompt-only>".to_string()]
        );
        assert_eq!(
            plugins.startup_notices(),
            vec!["plugin startup notice".to_string()]
        );
    }

    #[tokio::test]
    async fn plugins_declare_protocols_before_installing_the_same_runtime_contract() {
        let mut plugins = PluginRegistry::new();
        plugins.add(DeclaredProtocolPlugin {
            declares_protocol: true,
        });
        assert_eq!(
            plugins.protocol_descriptors().unwrap(),
            vec![
                DeclaredProtocolPlugin {
                    declares_protocol: true,
                }
                .descriptor()
            ]
        );
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());

        plugins
            .install(&mut PluginHost::new(
                &mut protocols,
                &mut model_tools,
                &mut commands,
                &mut tui,
                environment,
            ))
            .unwrap();

        assert_eq!(
            protocols.descriptors(),
            plugins.protocol_descriptors().unwrap()
        );
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn plugin_install_rejects_an_undeclared_protocol() {
        let mut plugins = PluginRegistry::new();
        plugins.add(DeclaredProtocolPlugin {
            declares_protocol: false,
        });
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());

        let error = plugins
            .install(&mut PluginHost::new(
                &mut protocols,
                &mut model_tools,
                &mut commands,
                &mut tui,
                environment,
            ))
            .unwrap_err();

        assert!(error.to_string().contains("declarations do not match"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn plugins_declare_and_install_the_same_model_tool_contract() {
        let mut plugins = PluginRegistry::new();
        let plugin = DeclaredModelToolPlugin {
            declares_tool: true,
        };
        plugins.add(plugin.clone());
        assert_eq!(
            plugins.model_tool_descriptors().unwrap(),
            vec![<DeclaredModelToolPlugin as ModelTool>::descriptor(&plugin)]
        );
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());

        plugins
            .install(&mut PluginHost::new(
                &mut protocols,
                &mut model_tools,
                &mut commands,
                &mut tui,
                environment,
            ))
            .unwrap();

        assert_eq!(
            model_tools.descriptors(),
            plugins.model_tool_descriptors().unwrap()
        );
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[test]
    fn model_tool_descriptors_require_a_strict_object_schema() {
        let descriptor = |parameters| ModelToolDescriptor {
            name: "strict_tool".to_string(),
            description: "Strict test tool".to_string(),
            parameters,
        };

        assert!(
            validate_model_tool_descriptor(&descriptor(serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            })))
            .is_ok()
        );
        assert!(
            validate_model_tool_descriptor(&descriptor(serde_json::json!({
                "type": "string",
                "properties": {},
                "additionalProperties": false
            })))
            .unwrap_err()
            .to_string()
            .contains("type object")
        );
        assert!(
            validate_model_tool_descriptor(&descriptor(serde_json::json!({
                "type": "object",
                "properties": {},
                "required": ["missing"],
                "additionalProperties": false
            })))
            .unwrap_err()
            .to_string()
            .contains("undeclared property")
        );
        assert!(
            validate_model_tool_descriptor(&descriptor(serde_json::json!({
                "type": "object",
                "properties": {}
            })))
            .unwrap_err()
            .to_string()
            .contains("additionalProperties")
        );
    }

    #[tokio::test]
    async fn model_tool_declarations_reject_undeclared_installs_and_collisions() {
        let mut undeclared = PluginRegistry::new();
        undeclared.add(DeclaredModelToolPlugin {
            declares_tool: false,
        });
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());
        let error = undeclared
            .install(&mut PluginHost::new(
                &mut protocols,
                &mut model_tools,
                &mut commands,
                &mut tui,
                environment,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("declarations do not match"));

        let mut duplicate = PluginRegistry::new();
        duplicate.add(DeclaredModelToolPlugin {
            declares_tool: true,
        });
        duplicate.add(DeclaredModelToolPlugin {
            declares_tool: true,
        });
        let error = duplicate.model_tool_descriptors().unwrap_err();
        assert!(error.to_string().contains("declared more than once"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn plugins_must_request_binary_download_access() {
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());
        let denied_capture = Arc::new(std::sync::OnceLock::new());
        let mut denied = PluginRegistry::new();
        denied.add(DownloadPlugin {
            requests_downloads: false,
            downloads: denied_capture,
        });
        let mut host = PluginHost::new(
            &mut protocols,
            &mut model_tools,
            &mut commands,
            &mut tui,
            environment,
        );

        let error = denied.install(&mut host).unwrap_err();
        assert!(format!("{error:#}").contains("did not request binary download access"));
        assert!(host.downloads().is_err());

        let allowed_capture = Arc::new(std::sync::OnceLock::new());
        let mut allowed = PluginRegistry::new();
        allowed.add(DownloadPlugin {
            requests_downloads: true,
            downloads: allowed_capture.clone(),
        });
        allowed.install(&mut host).unwrap();
        assert!(allowed_capture.get().is_some());
        assert!(host.downloads().is_err());
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn plugins_must_request_environment_access_once_for_dynamic_reads() {
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());
        environment
            .set("NPM_TOKEN", "first".to_string())
            .await
            .unwrap();

        let denied_capture = Arc::new(std::sync::OnceLock::new());
        let mut denied = PluginRegistry::new();
        denied.add(EnvironmentPlugin {
            requests_environment: false,
            environment: denied_capture,
        });
        let mut host = PluginHost::new(
            &mut protocols,
            &mut model_tools,
            &mut commands,
            &mut tui,
            environment.clone(),
        );
        let error = denied.install(&mut host).unwrap_err();
        assert!(format!("{error:#}").contains("did not request Agent environment access"));
        assert!(host.environment().is_err());

        let allowed_capture = Arc::new(std::sync::OnceLock::new());
        let mut allowed = PluginRegistry::new();
        allowed.add(EnvironmentPlugin {
            requests_environment: true,
            environment: allowed_capture.clone(),
        });
        allowed.install(&mut host).unwrap();
        let reader = allowed_capture.get().unwrap();
        assert_eq!(
            reader.get("NPM_TOKEN").await.unwrap().as_deref(),
            Some("first")
        );

        environment
            .set("DYNAMIC_TOKEN", "added later".to_string())
            .await
            .unwrap();
        assert_eq!(
            reader.get("DYNAMIC_TOKEN").await.unwrap().as_deref(),
            Some("added later")
        );
        assert_eq!(reader.snapshot().await.len(), 2);
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn plugins_must_request_credential_access_once_for_dynamic_reads() {
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());
        let manager = ConfigManager::load_for_test(&output, &output)
            .await
            .unwrap();
        manager
            .set_api_key("parallel", "first".to_string())
            .await
            .unwrap();

        let denied_capture = Arc::new(std::sync::OnceLock::new());
        let mut denied = PluginRegistry::new();
        denied.add(CredentialPlugin {
            requests_credentials: false,
            credentials: denied_capture,
        });
        let mut host = PluginHost::new(
            &mut protocols,
            &mut model_tools,
            &mut commands,
            &mut tui,
            environment,
        )
        .with_credentials(manager.clone());
        let error = denied.install(&mut host).unwrap_err();
        assert!(format!("{error:#}").contains("did not request credential access"));
        assert!(host.credentials().is_err());

        let allowed_capture = Arc::new(std::sync::OnceLock::new());
        let mut allowed = PluginRegistry::new();
        allowed.add(CredentialPlugin {
            requests_credentials: true,
            credentials: allowed_capture.clone(),
        });
        allowed.install(&mut host).unwrap();
        let reader = allowed_capture.get().unwrap();
        assert_eq!(
            reader.api_key("parallel").await.unwrap().as_deref(),
            Some("first")
        );

        manager
            .set_api_key("exa", "added later".to_string())
            .await
            .unwrap();
        assert_eq!(
            reader.api_key("exa").await.unwrap().as_deref(),
            Some("added later")
        );
        let _ = tokio::fs::remove_dir_all(output).await;
    }
}
