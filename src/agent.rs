use crate::builtins::context::{ContextPlugin, ContextState};
use crate::catalog::{ModelCatalog, ModelLimits, ThinkingLevel};
use crate::config::{AgentEnvironment, ConfigManager, display_path};
use crate::model::configured_backend;
use crate::output::OutputStore;
use crate::plugin::{
    CommandRegistry, ModelToolRegistry, PluginAgents, PluginHost, PluginRegistry, TuiRegistry,
};
use crate::plugin_state::{PLUGIN_STATE_DATABASE, PluginStateStore};
use crate::prompts::PromptEntry;
use crate::protocol::{ProtocolImage, ProtocolRegistry};
use crate::runtime::{AgentRuntime, RuntimeInitializer, forward_task_notices};
use crate::session::{EventKind, Session, SessionContext};
use crate::skill::{SkillProtocol, SkillProtocolSource};
use crate::task::TaskManager;
use crate::wasm_plugin::WasmPluginManager;
use anyhow::Result;
use async_trait::async_trait;
use rig::message::{AssistantContent, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;

pub const ROOT_AGENT_DEPTH: u8 = 1;
pub const MAX_AGENT_DEPTH: u8 = 2;

#[derive(Clone, Debug, Default)]
pub struct AgentOpenOptions {
    pub private_records: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitKind {
    Prompt,
    Steer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPrompt {
    pub text: String,
    pub images: Vec<ProtocolImage>,
}

impl AgentPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "content", rename_all = "snake_case")]
pub enum SystemPromptSelection {
    #[default]
    Inherit,
    Append(String),
    Replace(String),
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "names", rename_all = "snake_case")]
pub enum CapabilitySelection {
    #[default]
    All,
    Only(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    pub provider: String,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub working_directory: PathBuf,
    pub parent_session_id: Option<String>,
    pub system_prompt: SystemPromptSelection,
    pub tools: CapabilitySelection,
    pub protocols: CapabilitySelection,
    pub max_output_tokens: Option<usize>,
    depth: u8,
}

impl AgentSpec {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        thinking: ThinkingLevel,
        working_directory: impl Into<PathBuf>,
        parent_session_id: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            thinking,
            working_directory: working_directory.into(),
            parent_session_id: Some(parent_session_id.into()),
            system_prompt: SystemPromptSelection::Inherit,
            tools: CapabilitySelection::All,
            protocols: CapabilitySelection::All,
            max_output_tokens: None,
            depth: 0,
        }
    }

    #[doc(hidden)]
    pub fn root(
        provider: impl Into<String>,
        model: impl Into<String>,
        thinking: ThinkingLevel,
        working_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            thinking,
            working_directory: working_directory.into(),
            parent_session_id: None,
            system_prompt: SystemPromptSelection::Inherit,
            tools: CapabilitySelection::All,
            protocols: CapabilitySelection::All,
            max_output_tokens: None,
            depth: ROOT_AGENT_DEPTH,
        }
    }

    pub fn append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = SystemPromptSelection::Append(prompt.into());
        self
    }

    pub fn replace_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = SystemPromptSelection::Replace(prompt.into());
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = CapabilitySelection::Only(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_protocols(
        mut self,
        protocols: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.protocols = CapabilitySelection::Only(protocols.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: usize) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    pub fn depth(&self) -> u8 {
        self.depth
    }

    pub(crate) fn assign_depth(&mut self, depth: u8) {
        self.depth = depth;
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "content", rename_all = "snake_case")]
pub enum SystemPromptUpdate {
    Append(String),
    Replace(String),
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpecPatch {
    pub system_prompt: Option<SystemPromptUpdate>,
    pub tools: Option<CapabilitySelection>,
    pub protocols: Option<CapabilitySelection>,
}

#[derive(Clone, Debug)]
pub struct CompactionContext {
    pub session_id: String,
    pub summary: String,
    pub manual: bool,
    pub spec: AgentSpec,
}

#[async_trait]
pub trait CompactionCallback: Send + Sync {
    async fn compacted(&self, context: CompactionContext) -> Result<Option<AgentSpecPatch>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Running,
    Closed,
}

pub struct AgentServices {
    pub runtime: Arc<AgentRuntime>,
    pub protocols: Arc<ProtocolRegistry>,
    pub commands: Arc<CommandRegistry>,
    pub tui: Arc<TuiRegistry>,
    pub tasks: TaskManager,
    pub output: Arc<OutputStore>,
    pub context_window: usize,
    pub model_ready: bool,
    plugins: Arc<PluginRegistry>,
    wasm_plugins: WasmPluginManager,
}

struct AgentInstance {
    services: AgentServices,
    closed: AtomicBool,
}

#[derive(Clone)]
pub struct AgentHandle {
    host: AgentHost,
    instance: Arc<AgentInstance>,
}

impl AgentHandle {
    pub fn session_id(&self) -> &str {
        self.instance.services.runtime.session().id()
    }

    pub fn services(&self) -> &AgentServices {
        &self.instance.services
    }

    pub async fn spec(&self) -> AgentSpec {
        self.instance.services.runtime.session().spec().await
    }

    pub async fn submit(&self, text: impl Into<String>, kind: SubmitKind) -> Result<u64> {
        self.submit_prompt(AgentPrompt::text(text), kind).await
    }

    pub async fn submit_prompt(&self, prompt: AgentPrompt, kind: SubmitKind) -> Result<u64> {
        if self.instance.closed.load(Ordering::Acquire) {
            anyhow::bail!("Agent handle is closed");
        }
        Ok(self
            .instance
            .services
            .runtime
            .submit_with_images(prompt.text, prompt.images, kind)
            .await?
            .id)
    }

    pub async fn submit_prompt_exclusive(&self, prompt: AgentPrompt) -> Result<u64> {
        if self.instance.closed.load(Ordering::Acquire) {
            anyhow::bail!("Agent handle is closed");
        }
        Ok(self
            .instance
            .services
            .runtime
            .submit_exclusive_with_images(prompt.text, prompt.images)
            .await?
            .id)
    }

    pub async fn status(&self) -> AgentStatus {
        if self.instance.closed.load(Ordering::Acquire) {
            AgentStatus::Closed
        } else if self.instance.services.runtime.turn_running().await {
            AgentStatus::Running
        } else {
            AgentStatus::Idle
        }
    }

    pub async fn result(&self) -> Option<String> {
        if self.status().await == AgentStatus::Running {
            return None;
        }
        self.instance
            .services
            .runtime
            .session()
            .model_history()
            .await
            .into_iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant { content, .. } => {
                    let text = content
                        .into_iter()
                        .filter_map(|content| match content {
                            AssistantContent::Text(text) => Some(text.text),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            })
    }

    pub async fn cancel(&self) -> bool {
        self.instance.services.runtime.interrupt_turn().await
    }

    pub(crate) async fn cancel_submission(&self, submission_id: u64) -> bool {
        self.instance
            .services
            .runtime
            .interrupt_submission(submission_id)
            .await
    }

    pub async fn close(&self) {
        if self.instance.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.instance.services.wasm_plugins.shutdown().await;
        self.instance.services.runtime.shutdown().await;
        let _ = self.instance.services.plugins.shutdown().await;
        self.host
            .remove(self.session_id(), Arc::as_ptr(&self.instance) as usize)
            .await;
    }
}

struct AgentHostInner {
    manager: Arc<ConfigManager>,
    environment: Arc<AgentEnvironment>,
    catalog: Arc<ModelCatalog>,
    cwd: PathBuf,
    plugin_state: PluginStateStore,
    active: Mutex<HashMap<String, Weak<AgentInstance>>>,
    open_lock: Mutex<()>,
}

#[derive(Clone)]
pub struct AgentHost {
    inner: Arc<AgentHostInner>,
}

impl AgentHost {
    pub async fn new(
        manager: Arc<ConfigManager>,
        environment: Arc<AgentEnvironment>,
        catalog: Arc<ModelCatalog>,
        cwd: PathBuf,
    ) -> Result<Self> {
        let cwd = cwd.canonicalize()?;
        let plugin_state =
            PluginStateStore::open(manager.directory().join(PLUGIN_STATE_DATABASE), &cwd).await?;
        Ok(Self {
            inner: Arc::new(AgentHostInner {
                manager,
                environment,
                catalog,
                cwd,
                plugin_state,
                active: Mutex::new(HashMap::new()),
                open_lock: Mutex::new(()),
            }),
        })
    }

    pub fn working_directory(&self) -> &PathBuf {
        &self.inner.cwd
    }

    pub async fn create(
        &self,
        mut spec: AgentSpec,
        callback: Option<Arc<dyn CompactionCallback>>,
    ) -> Result<AgentHandle> {
        validate_spec(&spec)?;
        let parent_id = spec
            .parent_session_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("plugin-created Agents require a parent session"))?;
        let parent = Session::persisted_spec(&self.inner.cwd, &parent_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("parent session {parent_id} does not exist"))?;
        if parent.depth() != ROOT_AGENT_DEPTH {
            anyhow::bail!("maximum Agent nesting depth is {MAX_AGENT_DEPTH}");
        }
        let requested_cwd = spec
            .working_directory
            .canonicalize()
            .unwrap_or_else(|_| spec.working_directory.clone());
        if requested_cwd != self.inner.cwd || parent.working_directory != self.inner.cwd {
            anyhow::bail!("parent and child Agent must belong to the current project");
        }
        spec.working_directory.clone_from(&self.inner.cwd);
        spec.assign_depth(MAX_AGENT_DEPTH);
        self.open_inner(
            None,
            spec,
            callback,
            false,
            AgentOpenOptions::default(),
            true,
        )
        .await
    }

    pub async fn open_plugin(
        &self,
        session_id: &str,
        bound_parent: Option<&str>,
        callback: Option<Arc<dyn CompactionCallback>>,
    ) -> Result<AgentHandle> {
        if let Some(handle) = self.active(session_id).await {
            let spec = handle.spec().await;
            validate_spec(&spec)?;
            validate_plugin_open(&spec, bound_parent)?;
            handle
                .services()
                .runtime
                .set_compaction_callback(callback)
                .await;
            return Ok(handle);
        }
        let spec = Session::persisted_spec(&self.inner.cwd, session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Agent session {session_id} does not exist"))?;
        validate_spec(&spec)?;
        validate_plugin_open(&spec, bound_parent)?;
        self.open_inner(
            Some(session_id),
            spec,
            callback,
            false,
            AgentOpenOptions::default(),
            true,
        )
        .await
    }

    #[doc(hidden)]
    pub async fn open_root(&self, requested: Option<&str>, spec: AgentSpec) -> Result<AgentHandle> {
        self.open_root_with_options(requested, spec, AgentOpenOptions::default())
            .await
    }

    #[doc(hidden)]
    pub async fn open_root_with_options(
        &self,
        requested: Option<&str>,
        spec: AgentSpec,
        options: AgentOpenOptions,
    ) -> Result<AgentHandle> {
        self.open_root_with_resume(requested, spec, options, true)
            .await
    }

    pub(crate) async fn open_root_with_deferred_resume(
        &self,
        requested: Option<&str>,
        spec: AgentSpec,
        options: AgentOpenOptions,
    ) -> Result<AgentHandle> {
        self.open_root_with_resume(requested, spec, options, false)
            .await
    }

    async fn open_root_with_resume(
        &self,
        requested: Option<&str>,
        spec: AgentSpec,
        options: AgentOpenOptions,
        resume_pending: bool,
    ) -> Result<AgentHandle> {
        validate_spec(&spec)?;
        if spec.parent_session_id.is_some() || spec.depth() != ROOT_AGENT_DEPTH {
            anyhow::bail!("root Agent spec must have depth 1 and no parent");
        }
        self.open_inner(requested, spec, None, true, options, resume_pending)
            .await
    }

    async fn open_inner(
        &self,
        requested: Option<&str>,
        spec: AgentSpec,
        callback: Option<Arc<dyn CompactionCallback>>,
        root: bool,
        options: AgentOpenOptions,
        resume_pending: bool,
    ) -> Result<AgentHandle> {
        let _open = self.inner.open_lock.lock().await;
        let session = Session::open_deferred(requested, &self.inner.cwd, spec).await?;
        let restored = session.spec().await;
        validate_spec(&restored)?;
        if root {
            if restored.parent_session_id.is_some() || restored.depth() != ROOT_AGENT_DEPTH {
                anyhow::bail!("the terminal interface may only open depth-1 Agent sessions");
            }
        } else if restored.parent_session_id.is_none() || restored.depth() != MAX_AGENT_DEPTH {
            anyhow::bail!("plugins may only create or open depth-2 Agent sessions");
        }
        if let Some(handle) = self.active(session.id()).await {
            if !options.private_records.is_empty() {
                anyhow::bail!("cannot replace private records of an active Agent session");
            }
            handle
                .services()
                .runtime
                .set_compaction_callback(callback)
                .await;
            return Ok(handle);
        }
        let private_record_owners = options.private_records.keys().cloned().collect::<Vec<_>>();
        for (owner, payload) in options.private_records {
            session.stage_private_record(&owner, payload).await?;
        }
        let resume_pending = resume_pending && !session.pending_inputs().await?.is_empty();
        let services = self.build_services(session.clone(), callback).await?;
        if !private_record_owners.is_empty()
            && let Err(error) = services.runtime.prepare_context().await
        {
            let _ = services.wasm_plugins.shutdown().await;
            services.runtime.shutdown().await;
            let _ = services.plugins.shutdown().await;
            return Err(error);
        }
        if let Err(error) = session
            .persist_private_records(&private_record_owners)
            .await
        {
            let _ = services.wasm_plugins.shutdown().await;
            services.runtime.shutdown().await;
            let _ = services.plugins.shutdown().await;
            return Err(error);
        }
        let instance = Arc::new(AgentInstance {
            services,
            closed: AtomicBool::new(false),
        });
        self.inner.active.lock().await.insert(
            instance.services.runtime.session().id().to_string(),
            Arc::downgrade(&instance),
        );
        let handle = AgentHandle {
            host: self.clone(),
            instance,
        };
        if resume_pending
            && let Err(error) = handle.instance.services.runtime.resume_pending().await
        {
            handle.close().await;
            return Err(error);
        }
        Ok(handle)
    }

    async fn active(&self, session_id: &str) -> Option<AgentHandle> {
        let mut active = self.inner.active.lock().await;
        let instance = active.get(session_id).and_then(Weak::upgrade);
        if instance.is_none() {
            active.remove(session_id);
        }
        instance.map(|instance| AgentHandle {
            host: self.clone(),
            instance,
        })
    }

    async fn remove(&self, session_id: &str, instance: usize) {
        let mut active = self.inner.active.lock().await;
        if active
            .get(session_id)
            .and_then(Weak::upgrade)
            .is_some_and(|active| Arc::as_ptr(&active) as usize == instance)
        {
            active.remove(session_id);
        }
    }

    pub async fn run_background(&self) -> Result<()> {
        let mut plugins = crate::builtins::plugins(&self.inner.cwd, self.inner.manager.directory());
        let wasm_plugins =
            WasmPluginManager::new(self.inner.manager.directory(), &self.inner.cwd).await?;
        plugins.add(wasm_plugins.clone());

        let active = self.inner.manager.current().await;
        let output = Arc::new(OutputStore::new("background", active.output_limit).await?);
        wasm_plugins.bind_output(output.clone())?;
        let tasks = TaskManager::new();
        let mut protocols = ProtocolRegistry::new(output, tasks);
        let mut model_tools = ModelToolRegistry::new();
        let mut commands = CommandRegistry::with_core_commands();
        let mut tui = TuiRegistry::default();
        plugins.install(
            &mut PluginHost::new(
                &mut protocols,
                &mut model_tools,
                &mut commands,
                &mut tui,
                self.inner.environment.clone(),
            )
            .with_credentials(self.inner.manager.clone())
            .with_agents(PluginAgents::new(self.clone(), None))
            .with_state(self.inner.plugin_state.clone()),
        )?;
        wasm_plugins.set_reserved_protocols(
            protocols
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name),
        )?;
        wasm_plugins.set_reserved_model_tools(
            model_tools
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name),
        )?;
        let protocols = Arc::new(protocols);
        wasm_plugins.bind_host(Arc::downgrade(&protocols))?;

        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let result = plugins
            .run_residents(async move {
                #[cfg(unix)]
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
                #[cfg(not(unix))]
                let _ = tokio::signal::ctrl_c().await;
            })
            .await;
        self.shutdown_all().await;
        result
    }

    pub async fn shutdown_all(&self) {
        let instances = {
            let mut active = self.inner.active.lock().await;
            let instances = active
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            active.clear();
            instances
        };
        for instance in instances {
            AgentHandle {
                host: self.clone(),
                instance,
            }
            .close()
            .await;
        }
    }
}

fn validate_plugin_open(spec: &AgentSpec, bound_parent: Option<&str>) -> Result<()> {
    if spec.depth() != MAX_AGENT_DEPTH || spec.parent_session_id.is_none() {
        anyhow::bail!("plugins may only open depth-2 Agent sessions");
    }
    if let Some(parent) = bound_parent
        && spec.parent_session_id.as_deref() != Some(parent)
    {
        anyhow::bail!("Agent session does not belong to the calling parent session");
    }
    Ok(())
}

fn validate_spec(spec: &AgentSpec) -> Result<()> {
    if spec.parent_session_id.is_some() {
        if spec.provider.trim().is_empty() {
            anyhow::bail!("Agent provider cannot be empty");
        }
        if spec.model.trim().is_empty() {
            anyhow::bail!("Agent model cannot be empty");
        }
    }
    if spec.max_output_tokens == Some(0) {
        anyhow::bail!("Agent output-token limit must be greater than zero");
    }
    match &spec.system_prompt {
        SystemPromptSelection::Append(prompt) if prompt.trim().is_empty() => {
            anyhow::bail!("system prompt append cannot be empty");
        }
        SystemPromptSelection::Replace(prompt) if prompt.trim().is_empty() => {
            anyhow::bail!("replacement system prompt cannot be empty");
        }
        _ => {}
    }
    for (label, selection) in [("model tool", &spec.tools), ("protocol", &spec.protocols)] {
        let CapabilitySelection::Only(names) = selection else {
            continue;
        };
        let unique = names.iter().collect::<HashSet<_>>();
        if unique.len() != names.len() {
            anyhow::bail!("Agent {label} is selected more than once");
        }
        if names.iter().any(|name| name.trim().is_empty()) {
            anyhow::bail!("Agent {label} name cannot be empty");
        }
    }
    Ok(())
}

impl AgentHost {
    async fn build_services(
        &self,
        session: Session,
        callback: Option<Arc<dyn CompactionCallback>>,
    ) -> Result<AgentServices> {
        let context_state = ContextState::new(session.clone());
        let mcp_profile = session
            .private_record(crate::builtins::MCP_SESSION_PROFILE_OWNER)
            .await;
        let mut plugins = crate::builtins::plugins_with_session_profile(
            &self.inner.cwd,
            self.inner.manager.directory(),
            mcp_profile,
        );
        plugins.add(ContextPlugin::new(context_state.clone()));
        let wasm_plugins =
            WasmPluginManager::new(self.inner.manager.directory(), &self.inner.cwd).await?;
        plugins.add(wasm_plugins.clone());
        if !session.is_new() {
            plugins.restore_session_protocol_records(&session.session_protocol_records().await)?;
        }
        let startup_notices = plugins.startup_notices();
        let reserved_protocols = plugins
            .protocol_descriptors()?
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<HashSet<_>>();
        let tasks = TaskManager::from_reports(session.task_reports().await?);
        let spec = session.spec().await;
        let active = self
            .inner
            .manager
            .for_session(&spec.provider, &spec.model, spec.thinking)
            .await?;
        let output = Arc::new(OutputStore::new(session.id(), active.output_limit).await?);
        wasm_plugins.bind_output(output.clone())?;
        let mut protocols = ProtocolRegistry::new(output.clone(), tasks.clone());
        let mut model_tools = ModelToolRegistry::new();
        let mut commands = CommandRegistry::with_core_commands();
        let mut tui = TuiRegistry::default();
        plugins.install(
            &mut PluginHost::new(
                &mut protocols,
                &mut model_tools,
                &mut commands,
                &mut tui,
                self.inner.environment.clone(),
            )
            .with_credentials(self.inner.manager.clone())
            .with_agents(PluginAgents::new(
                self.clone(),
                Some(session.id().to_string()),
            ))
            .with_state(self.inner.plugin_state.clone()),
        )?;
        wasm_plugins.set_reserved_model_tools(
            model_tools
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name),
        )?;
        model_tools.set_dynamic_source(Arc::new(wasm_plugins.clone()))?;
        let skill_source = SkillProtocolSource::default();
        protocols.set_dynamic_source(Arc::new(skill_source.clone()))?;
        protocols.set_dynamic_source(Arc::new(wasm_plugins.clone()))?;
        protocols
            .restore_help_read_names(session.successful_protocol_help_reads().await)
            .await;
        let protocols = Arc::new(protocols);
        let model_tools = Arc::new(model_tools);
        let plugins = Arc::new(plugins);
        wasm_plugins.bind_host(Arc::downgrade(&protocols))?;

        let mut startup_notices = startup_notices;
        startup_notices.extend(self.inner.catalog.warnings().await);
        let configured = match configured_backend(
            &active,
            &self.inner.catalog,
            Some(session.id()),
            self.inner.manager.clone(),
        )
        .await
        {
            Ok(configured) => configured,
            Err(error) => {
                startup_notices.push(format!("model configuration is not usable: {error:#}"));
                None
            }
        };
        let model_ready = configured.is_some();
        let (backend, limits) = match configured {
            Some((backend, limits)) => (Some(backend), limits),
            None => (
                None,
                active
                    .catalog_model(&self.inner.catalog)
                    .await
                    .map_or_else(ModelLimits::default, |model| model.limits()),
            ),
        };
        let context_window = limits.context_window;
        let initializer = Arc::new(AgentInitializer {
            session: session.clone(),
            plugins: plugins.clone(),
            cwd: self.inner.cwd.clone(),
            reserved_protocols,
            protocols: protocols.clone(),
            model_tools: model_tools.clone(),
            skill_source,
            wasm_plugins: wasm_plugins.clone(),
            startup_notices,
        });
        let runtime = Arc::new(AgentRuntime::new_deferred_with_context(
            backend,
            protocols.clone(),
            model_tools,
            session,
            initializer,
            limits,
            context_state,
        ));
        forward_task_notices(
            runtime.session().clone(),
            tasks.clone(),
            Arc::downgrade(&runtime),
        );
        runtime.set_compaction_settings(active.compaction).await;
        runtime.set_compaction_callback(callback).await;
        runtime.set_max_output_tokens(spec.max_output_tokens).await;
        Ok(AgentServices {
            runtime,
            protocols,
            commands: Arc::new(commands),
            tui: Arc::new(tui),
            tasks,
            output,
            context_window,
            model_ready,
            plugins,
            wasm_plugins,
        })
    }
}

fn capability_names(selection: &CapabilitySelection) -> Option<&[String]> {
    match selection {
        CapabilitySelection::All => None,
        CapabilitySelection::Only(names) => Some(names),
    }
}

struct AgentInitializer {
    session: Session,
    plugins: Arc<PluginRegistry>,
    cwd: PathBuf,
    reserved_protocols: HashSet<String>,
    protocols: Arc<ProtocolRegistry>,
    model_tools: Arc<ModelToolRegistry>,
    skill_source: SkillProtocolSource,
    wasm_plugins: WasmPluginManager,
    startup_notices: Vec<String>,
}

impl AgentInitializer {
    fn render_system_prompt(&self, spec: &AgentSpec) -> Result<String> {
        let tools = self
            .model_tools
            .descriptors_for(capability_names(&spec.tools))?
            .into_iter()
            .map(|descriptor| PromptEntry {
                name: descriptor.name,
                description: descriptor.description,
            })
            .collect::<Vec<_>>();
        let protocols = self
            .protocols
            .prompt_protocols_for(capability_names(&spec.protocols))?;
        let fragments = self.plugins.system_prompt_fragments()?;
        let generated = crate::prompts::system_prompt(&tools, &protocols, &fragments);
        match &spec.system_prompt {
            SystemPromptSelection::Inherit => Ok(generated),
            SystemPromptSelection::Append(fragment) => {
                if fragment.trim().is_empty() {
                    anyhow::bail!("system prompt append cannot be empty");
                }
                Ok(format!("{generated}\n\n{fragment}"))
            }
            SystemPromptSelection::Replace(prompt) => {
                if prompt.trim().is_empty() {
                    anyhow::bail!("replacement system prompt cannot be empty");
                }
                Ok(prompt.clone())
            }
        }
    }
}

#[async_trait]
impl RuntimeInitializer for AgentInitializer {
    async fn initialize(&self) -> Result<String> {
        let new_session = self.session.is_new();
        let mut notices = Vec::new();
        let (skills, snapshots) = if new_session {
            let cwd = self.cwd.clone();
            let reserved = self.reserved_protocols.clone();
            let (skills, snapshots, discovered_notices) =
                tokio::task::spawn_blocking(move || -> Result<_> {
                    let (discovered, mut notices) = crate::skill::discover(&cwd);
                    let mut names = reserved;
                    let mut skills = Vec::new();
                    let mut snapshots = Vec::new();
                    for skill in discovered {
                        let snapshot = skill.snapshot();
                        let protocol = skill.protocol_name().to_string();
                        if !names.insert(protocol.clone()) {
                            notices.push(format!(
                                "skipped skill {} because protocol {}:// is already registered",
                                display_path(&snapshot.path),
                                protocol
                            ));
                            continue;
                        }
                        snapshots.push(snapshot);
                        skills.push(skill);
                    }
                    Ok((skills, snapshots, notices))
                })
                .await??;
            notices.extend(discovered_notices);
            (skills, snapshots)
        } else {
            let context = self.session.context().await;
            let mut skills = Vec::new();
            for snapshot in context.skills.clone() {
                let description = format!(
                    "skill {} at {}",
                    snapshot.name,
                    display_path(&snapshot.path)
                );
                match SkillProtocol::from_snapshot(snapshot) {
                    Ok(skill) => skills.push(skill),
                    Err(error) => notices.push(format!("skipped {description}: {error:#}")),
                }
            }
            (skills, context.skills)
        };
        let mut reserved = self.reserved_protocols.clone();
        reserved.extend(skills.iter().map(|skill| skill.protocol_name().to_string()));
        self.skill_source.replace(skills);
        self.wasm_plugins.set_reserved_protocols(reserved)?;

        let spec = self.session.spec().await;
        let needs_dynamic = matches!(spec.tools, CapabilitySelection::Only(_))
            || matches!(spec.protocols, CapabilitySelection::Only(_));
        if needs_dynamic {
            let report = self.wasm_plugins.initialize().await?;
            if !report.diagnostics.is_empty() {
                notices.push(format!(
                    "skipped {} WASM plugin(s); read wasm_plugin://help for diagnostics",
                    report.diagnostics.len()
                ));
            }
        }
        self.model_tools.select(capability_names(&spec.tools))?;
        self.protocols.select(capability_names(&spec.protocols))?;

        let prompt = if new_session {
            let prompt = self.render_system_prompt(&spec)?;
            self.session
                .initialize_context_with_protocols(
                    SessionContext {
                        system_prompt: prompt.clone(),
                        skills: snapshots,
                    },
                    self.plugins.session_protocol_records()?,
                )
                .await?;
            prompt
        } else {
            self.session.context().await.system_prompt
        };

        if !needs_dynamic {
            let wasm_plugins = self.wasm_plugins.clone();
            let session = self.session.clone();
            tokio::spawn(async move {
                let notice = match wasm_plugins.initialize().await {
                    Ok(report) if !report.diagnostics.is_empty() => Some(format!(
                        "skipped {} WASM plugin(s); read wasm_plugin://help for diagnostics",
                        report.diagnostics.len()
                    )),
                    Ok(_) => None,
                    Err(error) => Some(format!("WASM plugin initialization failed: {error:#}")),
                };
                if let Some(text) = notice {
                    let _ = session.append(EventKind::Notice { text }).await;
                }
            });
        }
        notices.extend(self.startup_notices.clone());
        for text in notices {
            self.session.append(EventKind::Notice { text }).await?;
        }
        Ok(prompt)
    }

    async fn render_system_prompt(&self, spec: &AgentSpec) -> Result<Option<String>> {
        Ok(Some(AgentInitializer::render_system_prompt(self, spec)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::{SessionMcpProfile, SessionMcpServer, SessionMcpTransport};
    use crate::catalog::ThinkingLevel;
    use std::path::Path;

    fn spec() -> AgentSpec {
        AgentSpec::new(
            "provider",
            "model",
            ThinkingLevel::Medium,
            Path::new("/work"),
            "parent",
        )
    }

    #[test]
    fn agent_specs_reject_invalid_static_boundaries() {
        let mut invalid = spec();
        invalid.provider.clear();
        assert!(validate_spec(&invalid).is_err());

        let mut invalid = spec();
        invalid.model = "  ".to_string();
        assert!(validate_spec(&invalid).is_err());

        let invalid = spec().with_max_output_tokens(0);
        assert!(validate_spec(&invalid).is_err());

        let invalid = spec().append_system_prompt("\n");
        assert!(validate_spec(&invalid).is_err());

        let invalid = spec().with_tools(["read", "read"]);
        assert!(validate_spec(&invalid).is_err());

        let invalid = spec().with_protocols([""]);
        assert!(validate_spec(&invalid).is_err());

        validate_spec(
            &spec()
                .replace_system_prompt("child prompt")
                .with_tools(std::iter::empty::<String>())
                .with_protocols(["file"])
                .with_max_output_tokens(128),
        )
        .unwrap();
    }

    #[test]
    fn root_agent_can_open_before_a_model_is_configured() {
        validate_spec(&AgentSpec::root(
            "",
            "",
            ThinkingLevel::Off,
            Path::new("/work"),
        ))
        .unwrap();
        validate_spec(&AgentSpec::root(
            "provider",
            "",
            ThinkingLevel::Off,
            Path::new("/work"),
        ))
        .unwrap();
    }

    #[test]
    fn plugin_open_requires_a_depth_two_session_owned_by_the_bound_parent() {
        let root = AgentSpec::root("provider", "model", ThinkingLevel::Off, Path::new("/work"));
        assert!(validate_plugin_open(&root, None).is_err());

        let mut child = spec();
        child.assign_depth(MAX_AGENT_DEPTH);
        validate_plugin_open(&child, None).unwrap();
        validate_plugin_open(&child, Some("parent")).unwrap();
        assert!(validate_plugin_open(&child, Some("other-parent")).is_err());
    }

    #[tokio::test]
    async fn frontend_private_mcp_profile_reopens_through_the_normal_root_path() {
        let workspace = tempfile::tempdir().unwrap();
        let config_directory = workspace.path().join("config");
        tokio::fs::create_dir_all(&config_directory).await.unwrap();
        let manager = ConfigManager::load_for_test(&config_directory, workspace.path())
            .await
            .unwrap();
        let environment = Arc::new(
            crate::config::AgentEnvironment::load(&config_directory)
                .await
                .unwrap(),
        );
        let catalog = Arc::new(ModelCatalog::load(&config_directory, true).await.unwrap());
        let host = AgentHost::new(
            manager.clone(),
            environment,
            catalog,
            workspace.path().to_path_buf(),
        )
        .await
        .unwrap();
        let profile = SessionMcpProfile::new(BTreeMap::from([(
            "private server".to_string(),
            SessionMcpServer {
                transport: SessionMcpTransport::Stdio {
                    command: "missing-test-server".to_string(),
                    args: Vec::new(),
                    environment: BTreeMap::from([(
                        "TOKEN".to_string(),
                        "private-test-value".to_string(),
                    )]),
                },
            },
        )]));
        let private_payload = serde_json::to_value(&profile).unwrap();
        let mut options = AgentOpenOptions::default();
        options.private_records.insert(
            crate::builtins::MCP_SESSION_PROFILE_OWNER.to_string(),
            private_payload.clone(),
        );
        let initial = manager.current().await;
        let spec = AgentSpec::root(
            &initial.provider,
            &initial.model,
            initial.thinking,
            workspace.path(),
        );
        let created = host
            .open_root_with_options(None, spec.clone(), options)
            .await
            .unwrap();
        created.services().runtime.prepare_context().await.unwrap();
        created
            .services()
            .runtime
            .session()
            .persist()
            .await
            .unwrap();
        let session_id = created.session_id().to_string();
        let frozen_context = created.services().runtime.session().context().await;
        let protocol_names = created
            .services()
            .protocols
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        assert!(
            protocol_names
                .iter()
                .any(|name| name == "private-server-mcp")
        );
        let transcript = serde_json::to_string(
            &created
                .services()
                .runtime
                .session()
                .snapshot()
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(!transcript.contains("private-test-value"));
        created.close().await;

        let reopened = host.open_root(Some(&session_id), spec).await.unwrap();
        reopened.services().runtime.prepare_context().await.unwrap();
        assert_eq!(
            reopened
                .services()
                .runtime
                .session()
                .private_record(crate::builtins::MCP_SESSION_PROFILE_OWNER)
                .await,
            Some(private_payload)
        );
        assert_eq!(
            reopened.services().runtime.session().context().await,
            frozen_context
        );
        assert_eq!(
            reopened
                .services()
                .protocols
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name)
                .collect::<Vec<_>>(),
            protocol_names
        );
        reopened.close().await;
    }
}
