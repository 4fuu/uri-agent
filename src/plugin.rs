use crate::protocol::{ProtocolDescriptor, ProtocolRegistry, validate_descriptor};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
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
    Effort,
    Settings,
    Login,
    Logout,
    Resume,
    NewSession,
    Compact,
    Help,
    Quit,
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
            "compose",
            "Compose message",
            "open the floating composer",
            ["insert"],
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
            "effort",
            "Thinking effort",
            "select thinking effort for the active model",
            ["thinking"],
            CommandTarget::Core(Effort),
        ),
        CommandSpec::new(
            "settings",
            "Settings",
            "model, credential status, thinking, and output limit",
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
}

pub struct PluginHost<'a> {
    pub protocols: &'a mut ProtocolRegistry,
    pub commands: &'a mut CommandRegistry,
    pub tui: &'a mut TuiRegistry,
}

pub trait Plugin: Send + Sync {
    /// Protocols contributed by this plugin. These declarations are used to
    /// freeze a new session's system prompt before the runtime registries exist.
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
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

    pub fn install(&self, host: &mut PluginHost<'_>) -> Result<()> {
        let expected = self
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
        if let Some(name) = expected.keys().find(|name| before.contains(*name)) {
            bail!("plugin protocol name is already registered: {name}");
        }

        for plugin in &self.plugins {
            plugin.register(host).context("failed to register plugin")?;
        }

        let installed = host
            .protocols
            .descriptors()
            .into_iter()
            .filter(|descriptor| !before.contains(&descriptor.name))
            .map(|descriptor| (descriptor.name.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        if installed != expected {
            bail!("plugin protocol declarations do not match installed protocols");
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
        assert_eq!(registry.resolve("compact now").unwrap().arguments, "now");
        assert_eq!(
            registry.resolve(":thinking high").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Effort)
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

    async fn empty_host() -> (ProtocolRegistry, CommandRegistry, TuiRegistry, PathBuf) {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let directory = output.directory().to_path_buf();
        (
            ProtocolRegistry::new(output, TaskManager::new()),
            CommandRegistry::with_core_commands(),
            TuiRegistry::default(),
            directory,
        )
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
        let (mut protocols, mut commands, mut tui, output) = empty_host().await;

        plugins
            .install(&mut PluginHost {
                protocols: &mut protocols,
                commands: &mut commands,
                tui: &mut tui,
            })
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
        let (mut protocols, mut commands, mut tui, output) = empty_host().await;

        let error = plugins
            .install(&mut PluginHost {
                protocols: &mut protocols,
                commands: &mut commands,
                tui: &mut tui,
            })
            .unwrap_err();

        assert!(error.to_string().contains("declarations do not match"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }
}
