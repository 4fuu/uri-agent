use crate::agent::{AgentHandle, AgentHost, AgentSpec, CompactionCallback};
use crate::config::{AgentEnvironment, ConfigManager, ModelRole};
use crate::plugin_state::{PluginState, PluginStateScope, PluginStateStore};
use crate::protocol::{ProtocolDescriptor, ProtocolImage, ProtocolRegistry, validate_descriptor};
use crate::tool_download::BinaryDownloader;
pub use crate::tool_download::{BinaryDownload, DownloadArchive};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rig::completion::ToolDefinition;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tokio::time::Instant;

const RESIDENT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(60);
const MIN_RESIDENT_WAKE_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreCommand {
    Compose,
    Copy,
    Tasks,
    Protocols,
    Status,
    Models,
    ModelRoles,
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
    ModelRole {
        plugin: String,
        key: String,
        default_role: String,
    },
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
        if let CommandTarget::ModelRole {
            plugin,
            key,
            default_role,
        } = &spec.target
        {
            validate_name(plugin)?;
            validate_name(key)?;
            validate_name(default_role)?;
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
            "model-roles",
            "Model roles",
            "assign models to the small and custom plugin roles",
            ["roles"],
            CommandTarget::Core(ModelRoles),
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

#[derive(Clone, Debug)]
pub struct TuiSubmissionContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub prompt: String,
    pub first_user_message: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiEffect {
    TerminalTitle(String),
}

#[async_trait]
pub trait TuiSubmissionProvider: Send + Sync {
    async fn submitted(&self, context: &TuiSubmissionContext) -> Result<Option<TuiEffect>>;
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
    submissions: BTreeMap<String, Arc<dyn TuiSubmissionProvider>>,
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

    pub fn register_submission(
        &mut self,
        id: impl Into<String>,
        provider: impl TuiSubmissionProvider + 'static,
    ) -> Result<()> {
        let id = id.into();
        validate_name(&id)?;
        if self.submissions.contains_key(&id) {
            bail!("TUI submission provider is already registered: {id}");
        }
        self.submissions.insert(id, Arc::new(provider));
        Ok(())
    }

    pub async fn submission_effects(&self, context: &TuiSubmissionContext) -> Vec<TuiEffect> {
        let mut effects = Vec::new();
        for provider in self.submissions.values() {
            if let Ok(Some(effect)) = provider.submitted(context).await {
                effects.push(effect);
            }
        }
        effects
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelToolOutput {
    output: String,
    images: Vec<ProtocolImage>,
}

impl ModelToolOutput {
    pub fn new(output: String, images: Vec<ProtocolImage>) -> Self {
        Self { output, images }
    }

    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn images(&self) -> &[ProtocolImage] {
        &self.images
    }

    pub(crate) fn into_parts(self) -> (String, Vec<ProtocolImage>) {
        (self.output, self.images)
    }
}

impl From<String> for ModelToolOutput {
    fn from(output: String) -> Self {
        Self::new(output, Vec::new())
    }
}

impl From<&str> for ModelToolOutput {
    fn from(output: &str) -> Self {
        output.to_string().into()
    }
}

#[async_trait]
pub trait ModelTool: Send + Sync {
    fn descriptor(&self) -> ModelToolDescriptor;

    async fn execute(
        &self,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput>;
}

pub trait DynamicModelToolSource: Send + Sync {
    fn descriptors(&self) -> Vec<ModelToolDescriptor>;
    fn tool(&self, name: &str) -> Option<Arc<dyn ModelTool>>;
}

#[derive(Default)]
pub struct ModelToolRegistry {
    tools: BTreeMap<String, Arc<dyn ModelTool>>,
    dynamic: Option<Arc<dyn DynamicModelToolSource>>,
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

    pub fn select(&self, names: Option<&[String]>) -> Result<()> {
        let selected = names
            .map(|names| self.validate_selection(names))
            .transpose()?;
        *self
            .allowed
            .write()
            .expect("model tool selection lock poisoned") = selected;
        Ok(())
    }

    pub(crate) fn validate_selection(&self, names: &[String]) -> Result<HashSet<String>> {
        let available = self
            .all_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<HashSet<_>>();
        let selected = names.iter().cloned().collect::<HashSet<_>>();
        if selected.len() != names.len() {
            bail!("Agent model tool is selected more than once");
        }
        if let Some(name) = selected.iter().find(|name| !available.contains(*name)) {
            bail!("unknown Agent model tool: {name}");
        }
        Ok(selected)
    }

    fn all_descriptors(&self) -> Vec<ModelToolDescriptor> {
        let mut descriptors = self
            .tools
            .values()
            .map(|tool| tool.descriptor())
            .collect::<Vec<_>>();
        if let Some(dynamic) = &self.dynamic {
            descriptors.extend(dynamic.descriptors());
        }
        descriptors
    }

    pub(crate) fn descriptors_for(
        &self,
        names: Option<&[String]>,
    ) -> Result<Vec<ModelToolDescriptor>> {
        let selected = names
            .map(|names| self.validate_selection(names))
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
        if let Some(allowed) = self
            .allowed
            .read()
            .expect("model tool selection lock poisoned")
            .as_ref()
        {
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

    pub async fn dispatch(
        &self,
        name: &str,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        if self
            .allowed
            .read()
            .expect("model tool selection lock poisoned")
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(name))
        {
            bail!("unknown model tool: {name}");
        }
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
    /// Create, open, and submit work to persistent Agents.
    /// This declaration is an audit marker for trusted plugin code, not an
    /// interactive approval boundary.
    Agents,
    /// Read and write the plugin's separate persistent state namespace.
    State,
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
pub struct PluginModelRoleResolver {
    manager: Arc<ConfigManager>,
}

impl PluginModelRoleResolver {
    pub(crate) fn new(manager: Arc<ConfigManager>) -> Self {
        Self { manager }
    }

    pub async fn resolve(&self, name: &str) -> Result<Option<ModelRole>> {
        self.manager.model_role(name).await
    }
}

#[derive(Clone)]
pub struct PluginSettings {
    manager: Arc<ConfigManager>,
    plugin: String,
}

impl PluginSettings {
    pub(crate) fn new(manager: Arc<ConfigManager>, plugin: impl Into<String>) -> Self {
        Self {
            manager,
            plugin: plugin.into(),
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Value>> {
        self.manager.plugin_setting(&self.plugin, key).await
    }

    pub async fn set(&self, key: &str, value: Value) -> Result<()> {
        self.manager
            .set_plugin_setting(&self.plugin, key, value)
            .await
    }

    pub async fn remove(&self, key: &str) -> Result<bool> {
        self.manager.remove_plugin_setting(&self.plugin, key).await
    }

    pub(crate) fn scoped(&self, plugin: impl Into<String>) -> Self {
        Self::new(self.manager.clone(), plugin)
    }
}

#[derive(Clone)]
pub struct PluginAgents {
    host: AgentHost,
    parent_session_id: Option<String>,
}

impl PluginAgents {
    pub(crate) fn new(host: AgentHost, parent_session_id: Option<String>) -> Self {
        Self {
            host,
            parent_session_id,
        }
    }

    pub async fn create(
        &self,
        mut spec: AgentSpec,
        callback: Option<Arc<dyn CompactionCallback>>,
    ) -> Result<AgentHandle> {
        if let Some(parent) = &self.parent_session_id {
            if spec
                .parent_session_id
                .as_deref()
                .is_some_and(|id| id != parent)
            {
                bail!("plugin Agent parent does not match the calling Agent");
            }
            spec.parent_session_id = Some(parent.clone());
        }
        self.host.create(spec, callback).await
    }

    pub async fn open(
        &self,
        session_id: &str,
        callback: Option<Arc<dyn CompactionCallback>>,
    ) -> Result<AgentHandle> {
        self.host
            .open_plugin(session_id, self.parent_session_id.as_deref(), callback)
            .await
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
    agents: Option<PluginAgents>,
    state: Option<PluginStateStore>,
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
            agents: None,
            state: None,
            downloads: PluginDownloads::new(),
            permissions: HashSet::new(),
        }
    }

    pub fn with_credentials(mut self, manager: Arc<ConfigManager>) -> Self {
        self.credentials = Some(manager);
        self
    }

    #[doc(hidden)]
    pub fn with_agents(mut self, agents: PluginAgents) -> Self {
        self.agents = Some(agents);
        self
    }

    #[doc(hidden)]
    pub fn with_state(mut self, state: PluginStateStore) -> Self {
        self.state = Some(state);
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

    pub fn model_roles(&self) -> Result<PluginModelRoleResolver> {
        let manager = self
            .credentials
            .clone()
            .ok_or_else(|| anyhow::anyhow!("plugin model roles are not attached"))?;
        Ok(PluginModelRoleResolver::new(manager))
    }

    pub fn settings(&self, plugin: impl Into<String>) -> Result<PluginSettings> {
        let manager = self
            .credentials
            .clone()
            .ok_or_else(|| anyhow::anyhow!("plugin settings are not attached"))?;
        Ok(PluginSettings::new(manager, plugin))
    }

    pub fn agents(&self) -> Result<PluginAgents> {
        if !self.permissions.contains(&PluginPermission::Agents) {
            bail!("plugin did not request Agent access");
        }
        self.agents
            .clone()
            .ok_or_else(|| anyhow::anyhow!("plugin Agent access is not attached"))
    }

    pub fn state(&self, plugin: impl Into<String>, scope: PluginStateScope) -> Result<PluginState> {
        if !self.permissions.contains(&PluginPermission::State) {
            bail!("plugin did not request persistent state access");
        }
        self.state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("plugin state access is not attached"))?
            .state(plugin, scope)
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

    /// Opt into the process-resident lifecycle used by `uri-agent --background`.
    /// Plugins that return `None` remain ordinary request-driven plugins.
    fn resident(&self) -> Option<Arc<dyn ResidentPlugin>> {
        None
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResidentEvent {
    Start,
    Wake,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResidentResponse {
    pub wake_after: Option<Duration>,
}

impl ResidentResponse {
    pub fn wake_after(delay: Duration) -> Self {
        Self {
            wake_after: Some(delay),
        }
    }
}

#[async_trait]
pub trait ResidentPlugin: Send + Sync {
    fn name(&self) -> &str;

    async fn handle(&self, event: ResidentEvent) -> Result<ResidentResponse>;
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

    pub async fn run_residents(&self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let residents = self.residents()?;
        let mut started = Vec::with_capacity(residents.len());
        let mut schedule = Vec::with_capacity(residents.len());
        let mut failure = None;
        for resident in residents {
            match resident_call(&resident, ResidentEvent::Start).await {
                Ok(response) => {
                    schedule.push((resident.clone(), next_wake(response)));
                    started.push(resident);
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }

        if failure.is_none() {
            tokio::pin!(shutdown);
            loop {
                let next = schedule.iter().filter_map(|(_, wake)| *wake).min();
                match next {
                    Some(next) => tokio::select! {
                        () = &mut shutdown => break,
                        () = tokio::time::sleep_until(next) => {
                            let now = Instant::now();
                            for (resident, wake) in &mut schedule {
                                if wake.is_none_or(|wake| wake > now) {
                                    continue;
                                }
                                match resident_call(resident, ResidentEvent::Wake).await {
                                    Ok(response) => *wake = next_wake(response),
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                            if failure.is_some() {
                                break;
                            }
                        }
                    },
                    None => {
                        shutdown.await;
                        break;
                    }
                }
            }
        }

        let mut shutdown_failure = None;
        for resident in started.into_iter().rev() {
            if let Err(error) = resident_call(&resident, ResidentEvent::Shutdown).await
                && shutdown_failure.is_none()
            {
                shutdown_failure = Some(error);
            }
        }
        match (failure, shutdown_failure) {
            (Some(error), _) | (None, Some(error)) => Err(error),
            (None, None) => Ok(()),
        }
    }

    fn residents(&self) -> Result<Vec<Arc<dyn ResidentPlugin>>> {
        let mut names = HashSet::new();
        let mut residents = Vec::new();
        for plugin in &self.plugins {
            let Some(resident) = plugin.resident() else {
                continue;
            };
            validate_name(resident.name())?;
            if !names.insert(resident.name().to_string()) {
                bail!(
                    "resident plugin name is already registered: {}",
                    resident.name()
                );
            }
            residents.push(resident);
        }
        Ok(residents)
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

async fn resident_call(
    resident: &Arc<dyn ResidentPlugin>,
    event: ResidentEvent,
) -> Result<ResidentResponse> {
    tokio::time::timeout(RESIDENT_CALLBACK_TIMEOUT, resident.handle(event))
        .await
        .with_context(|| {
            format!(
                "resident plugin {} {event:?} callback exceeded {} seconds",
                resident.name(),
                RESIDENT_CALLBACK_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| {
            format!(
                "resident plugin {} {event:?} callback failed",
                resident.name()
            )
        })
}

fn next_wake(response: ResidentResponse) -> Option<Instant> {
    response
        .wake_after
        .map(|delay| Instant::now() + delay.max(MIN_RESIDENT_WAKE_DELAY))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ThinkingLevel;
    use crate::output::OutputStore;
    use crate::protocol::{Protocol, ProtocolDescriptor};
    use crate::task::TaskManager;

    struct ResidentFixture {
        name: &'static str,
        events: Arc<std::sync::Mutex<Vec<(&'static str, ResidentEvent)>>>,
        wake_after_start: Option<Duration>,
        fail_on: Option<ResidentEvent>,
    }

    #[async_trait]
    impl ResidentPlugin for ResidentFixture {
        fn name(&self) -> &str {
            self.name
        }

        async fn handle(&self, event: ResidentEvent) -> Result<ResidentResponse> {
            self.events.lock().unwrap().push((self.name, event));
            if self.fail_on == Some(event) {
                bail!("{} failed on {event:?}", self.name);
            }
            Ok(if event == ResidentEvent::Start {
                ResidentResponse {
                    wake_after: self.wake_after_start,
                }
            } else {
                ResidentResponse::default()
            })
        }
    }

    struct ResidentFixturePlugin(Arc<ResidentFixture>);

    impl Plugin for ResidentFixturePlugin {
        fn resident(&self) -> Option<Arc<dyn ResidentPlugin>> {
            Some(self.0.clone())
        }

        fn register(&self, _host: &mut PluginHost<'_>) -> Result<()> {
            Ok(())
        }
    }

    fn resident_fixture(
        name: &'static str,
        events: Arc<std::sync::Mutex<Vec<(&'static str, ResidentEvent)>>>,
        wake_after_start: Option<Duration>,
        fail_on: Option<ResidentEvent>,
    ) -> ResidentFixturePlugin {
        ResidentFixturePlugin(Arc::new(ResidentFixture {
            name,
            events,
            wake_after_start,
            fail_on,
        }))
    }

    #[tokio::test]
    async fn resident_plugins_start_wake_and_shutdown_in_lifecycle_order() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = PluginRegistry::new();
        registry.add(resident_fixture(
            "first",
            events.clone(),
            Some(Duration::from_millis(1)),
            None,
        ));
        registry.add(resident_fixture("second", events.clone(), None, None));

        registry
            .run_residents(tokio::time::sleep(Duration::from_millis(140)))
            .await
            .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            [
                ("first", ResidentEvent::Start),
                ("second", ResidentEvent::Start),
                ("first", ResidentEvent::Wake),
                ("second", ResidentEvent::Shutdown),
                ("first", ResidentEvent::Shutdown),
            ]
        );
    }

    #[tokio::test]
    async fn resident_wake_delays_are_clamped_and_start_failure_cleans_up_in_reverse() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut clamped = PluginRegistry::new();
        clamped.add(resident_fixture(
            "clamped",
            events.clone(),
            Some(Duration::ZERO),
            None,
        ));
        clamped
            .run_residents(tokio::time::sleep(Duration::from_millis(30)))
            .await
            .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            [
                ("clamped", ResidentEvent::Start),
                ("clamped", ResidentEvent::Shutdown),
            ]
        );

        events.lock().unwrap().clear();
        let mut failing = PluginRegistry::new();
        failing.add(resident_fixture("first", events.clone(), None, None));
        failing.add(resident_fixture("second", events.clone(), None, None));
        failing.add(resident_fixture(
            "failing",
            events.clone(),
            None,
            Some(ResidentEvent::Start),
        ));
        let error = failing
            .run_residents(std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("failing"));
        assert_eq!(
            *events.lock().unwrap(),
            [
                ("first", ResidentEvent::Start),
                ("second", ResidentEvent::Start),
                ("failing", ResidentEvent::Start),
                ("second", ResidentEvent::Shutdown),
                ("first", ResidentEvent::Shutdown),
            ]
        );
    }

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
            registry.resolve(":roles").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::ModelRoles)
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

    #[test]
    fn plugin_model_role_commands_validate_their_namespace_key_and_default_role() {
        let mut registry = CommandRegistry::with_core_commands();
        registry
            .register(CommandSpec::new(
                "terminal-title-model",
                "Terminal title model",
                "choose the role used for generated terminal titles",
                std::iter::empty::<&str>(),
                CommandTarget::ModelRole {
                    plugin: "terminal-title".to_string(),
                    key: "role".to_string(),
                    default_role: "small".to_string(),
                },
            ))
            .unwrap();
        assert!(matches!(
            registry
                .resolve(":terminal-title-model")
                .unwrap()
                .spec
                .target,
            CommandTarget::ModelRole { plugin, key, default_role }
                if plugin == "terminal-title" && key == "role" && default_role == "small"
        ));

        let error = registry
            .register(CommandSpec::new(
                "invalid-role-setting",
                "Invalid role setting",
                "test invalid role settings",
                std::iter::empty::<&str>(),
                CommandTarget::ModelRole {
                    plugin: "invalid setting".to_string(),
                    key: "role".to_string(),
                    default_role: "small".to_string(),
                },
            ))
            .unwrap_err();
        assert!(error.to_string().contains("invalid command name or alias"));
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

    struct ModelRolePlugin {
        model_roles: Arc<std::sync::OnceLock<PluginModelRoleResolver>>,
    }

    struct SettingsPlugin {
        settings: Arc<std::sync::OnceLock<PluginSettings>>,
    }

    struct AgentPlugin {
        requests_agents: bool,
        agents: Arc<std::sync::OnceLock<PluginAgents>>,
    }

    #[derive(Clone)]
    struct DeclaredModelToolPlugin {
        declares_tool: bool,
    }

    struct NamedModelTool(&'static str);

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
        ) -> Result<ModelToolOutput> {
            Ok("ok".into())
        }
    }

    #[async_trait]
    impl ModelTool for NamedModelTool {
        fn descriptor(&self) -> ModelToolDescriptor {
            ModelToolDescriptor {
                name: self.0.to_string(),
                description: "Named test model tool".to_string(),
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
        ) -> Result<ModelToolOutput> {
            Ok(self.0.into())
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

    impl Plugin for ModelRolePlugin {
        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            self.model_roles
                .set(host.model_roles()?)
                .map_err(|_| anyhow::anyhow!("model roles were already captured"))
        }
    }

    impl Plugin for SettingsPlugin {
        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            self.settings
                .set(host.settings("test-plugin")?)
                .map_err(|_| anyhow::anyhow!("settings were already captured"))
        }
    }

    impl Plugin for AgentPlugin {
        fn permissions(&self) -> Vec<PluginPermission> {
            self.requests_agents
                .then_some(PluginPermission::Agents)
                .into_iter()
                .collect()
        }

        fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
            self.agents
                .set(host.agents()?)
                .map_err(|_| anyhow::anyhow!("Agents were already captured"))
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

    #[tokio::test]
    async fn selection_limits_model_tool_descriptors_and_dispatch() {
        let (protocols, mut model_tools, _commands, _tui, output) = empty_host().await;
        model_tools.register(NamedModelTool("first")).unwrap();
        model_tools.register(NamedModelTool("second")).unwrap();

        model_tools.select(Some(&["second".to_string()])).unwrap();

        assert_eq!(
            model_tools
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name)
                .collect::<Vec<_>>(),
            vec!["second"]
        );
        assert_eq!(
            model_tools
                .dispatch("second", &serde_json::json!({}), &protocols)
                .await
                .unwrap(),
            ModelToolOutput::from("second")
        );
        assert_eq!(
            model_tools
                .dispatch("first", &serde_json::json!({}), &protocols)
                .await
                .unwrap_err()
                .to_string(),
            "unknown model tool: first"
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

    #[tokio::test]
    async fn plugins_resolve_model_roles_dynamically_without_a_permission() {
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        tokio::fs::create_dir_all(&output).await.unwrap();
        tokio::fs::write(
            output.join("models.json"),
            br#"{"providers":{"example":{"baseUrl":"https://example.invalid/v1","api":"openai-responses","models":[{"id":"first","name":"First"},{"id":"second","name":"Second"}]}}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            output.join("settings.json"),
            br#"{"modelRoles":{"review":{"provider":"example","model":"first","thinking":"low"}}}"#,
        )
        .await
        .unwrap();
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());
        let manager = ConfigManager::load_for_test(&output, &output)
            .await
            .unwrap();
        let capture = Arc::new(std::sync::OnceLock::new());
        let mut plugins = PluginRegistry::new();
        plugins.add(ModelRolePlugin {
            model_roles: capture.clone(),
        });
        let mut host = PluginHost::new(
            &mut protocols,
            &mut model_tools,
            &mut commands,
            &mut tui,
            environment,
        )
        .with_credentials(manager.clone());
        plugins.install(&mut host).unwrap();
        let roles = capture.get().unwrap();
        assert_eq!(
            roles.resolve("review").await.unwrap(),
            Some(ModelRole {
                provider: "example".to_string(),
                model: "first".to_string(),
                thinking: ThinkingLevel::Low,
            })
        );

        tokio::fs::write(
            output.join("settings.json"),
            br#"{"modelRoles":{"review":{"provider":"example","model":"second","thinking":"high"}}}"#,
        )
        .await
        .unwrap();
        manager.reload().await.unwrap();
        assert_eq!(
            roles.resolve("review").await.unwrap(),
            Some(ModelRole {
                provider: "example".to_string(),
                model: "second".to_string(),
                thinking: ThinkingLevel::High,
            })
        );
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn plugins_store_dynamic_json_values_in_their_own_namespace() {
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());
        let manager = ConfigManager::load_for_test(&output, &output)
            .await
            .unwrap();
        let capture = Arc::new(std::sync::OnceLock::new());
        let mut plugins = PluginRegistry::new();
        plugins.add(SettingsPlugin {
            settings: capture.clone(),
        });
        plugins
            .install(
                &mut PluginHost::new(
                    &mut protocols,
                    &mut model_tools,
                    &mut commands,
                    &mut tui,
                    environment,
                )
                .with_credentials(manager),
            )
            .unwrap();
        let settings = capture.get().unwrap();
        settings
            .set("options", serde_json::json!({"role": "small", "words": 5}))
            .await
            .unwrap();
        assert_eq!(
            settings.get("options").await.unwrap(),
            Some(serde_json::json!({"role": "small", "words": 5}))
        );
        assert!(settings.remove("options").await.unwrap());
        assert_eq!(settings.get("options").await.unwrap(), None);
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn plugins_must_request_agent_access() {
        let (mut protocols, mut model_tools, mut commands, mut tui, output) = empty_host().await;
        let environment = Arc::new(AgentEnvironment::load(&output).await.unwrap());
        let manager = ConfigManager::load_for_test(&output, &output)
            .await
            .unwrap();
        let agent_host = crate::agent::AgentHost::new(
            manager.clone(),
            environment.clone(),
            Arc::new(
                crate::catalog::ModelCatalog::load(&output, true)
                    .await
                    .unwrap(),
            ),
            output.clone(),
        )
        .await
        .unwrap();
        let agents = PluginAgents::new(agent_host, None);
        let denied_capture = Arc::new(std::sync::OnceLock::new());
        let mut denied = PluginRegistry::new();
        denied.add(AgentPlugin {
            requests_agents: false,
            agents: denied_capture,
        });
        let mut host = PluginHost::new(
            &mut protocols,
            &mut model_tools,
            &mut commands,
            &mut tui,
            environment,
        )
        .with_credentials(manager)
        .with_agents(agents);

        let error = denied.install(&mut host).unwrap_err();
        assert!(format!("{error:#}").contains("did not request Agent access"));
        assert!(host.agents().is_err());

        let allowed_capture = Arc::new(std::sync::OnceLock::new());
        let mut allowed = PluginRegistry::new();
        allowed.add(AgentPlugin {
            requests_agents: true,
            agents: allowed_capture.clone(),
        });
        allowed.install(&mut host).unwrap();
        assert!(allowed_capture.get().is_some());
        assert!(host.agents().is_err());
        let _ = tokio::fs::remove_dir_all(output).await;
    }
}
