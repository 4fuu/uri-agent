//! Session hosts, model resolution, and the minimal simple-mode service graph.

use crate::builtins::collaboration::CollaborationState;
use crate::builtins::context::ContextState;
use crate::catalog::ModelLimits;
use crate::catalog::ThinkingLevel;
use crate::config::AgentEnvironment;
use crate::config::ConfigManager;
use crate::model::configured_backend;
use crate::plugin::{ModelToolRegistry, PluginHost, PluginRegistry};
use crate::prompts::PromptEntry;
use crate::runtime::{AgentRuntime, RuntimeInitializer};
use crate::session::{EventKind, Session, SessionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;

pub const ROOT_AGENT_DEPTH: u8 = 1;

#[derive(Clone, Debug)]
pub struct AgentOpenOptions {
    pub collaboration_enabled: bool,
}

impl Default for AgentOpenOptions {
    fn default() -> Self {
        Self {
            collaboration_enabled: true,
        }
    }
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
}

impl AgentPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
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
    pub max_output_tokens: Option<usize>,
    #[serde(default)]
    depth: u8,
}

impl AgentSpec {
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
            max_output_tokens: None,
            depth: ROOT_AGENT_DEPTH,
        }
    }

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
            max_output_tokens: None,
            depth: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn assign_depth(&mut self, depth: u8) {
        self.depth = depth;
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

    pub fn with_max_output_tokens(mut self, max_output_tokens: usize) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    pub fn depth(&self) -> u8 {
        self.depth
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
}

#[async_trait]
pub trait CompactionCallback: Send + Sync {
    async fn compacted(&self) -> Result<Option<AgentSpecPatch>>;
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
    pub context_window: usize,
    pub model_ready: bool,
    collaboration: Option<CollaborationState>,
    plugins: Arc<PluginRegistry>,
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
            .submit(prompt.text, kind)
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
            .submit_exclusive(prompt.text)
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
            .last_assistant_text()
            .await
    }

    pub async fn cancel(&self) -> bool {
        self.instance.services.runtime.interrupt_turn().await
    }

    pub async fn close(&self) {
        if self.instance.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.instance.services.runtime.shutdown().await;
        if let Some(collaboration) = &self.instance.services.collaboration {
            let _ = collaboration.shutdown().await;
        }
        let _ = self.instance.services.plugins.shutdown().await;
        self.host
            .remove(self.session_id(), Arc::as_ptr(&self.instance) as usize)
            .await;
    }
}

struct AgentHostInner {
    manager: Arc<ConfigManager>,
    environment: Arc<AgentEnvironment>,
    catalog: Arc<crate::catalog::ModelCatalog>,
    cwd: PathBuf,
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
        catalog: Arc<crate::catalog::ModelCatalog>,
        cwd: PathBuf,
    ) -> Result<Self> {
        let cwd = cwd.canonicalize()?;
        Ok(Self {
            inner: Arc::new(AgentHostInner {
                manager,
                environment,
                catalog,
                cwd,
                active: Mutex::new(HashMap::new()),
                open_lock: Mutex::new(()),
            }),
        })
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
        self.open_inner(requested, spec, options, resume_pending)
            .await
    }

    async fn open_inner(
        &self,
        requested: Option<&str>,
        spec: AgentSpec,
        options: AgentOpenOptions,
        resume_pending: bool,
    ) -> Result<AgentHandle> {
        let _open = self.inner.open_lock.lock().await;
        let session = Session::open_deferred(requested, &self.inner.cwd, spec).await?;
        let restored = session.spec().await;
        validate_spec(&restored)?;
        if restored.parent_session_id.is_some() || restored.depth() != ROOT_AGENT_DEPTH {
            anyhow::bail!("the text interface may only open depth-1 sessions");
        }
        if let Some(handle) = self.active(session.id()).await {
            return Ok(handle);
        }
        let resume_pending = resume_pending && !session.pending_inputs().await?.is_empty();
        let services = self
            .build_services(session.clone(), options.collaboration_enabled)
            .await?;
        if let Err(error) = services.runtime.prepare_context().await {
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
}

/// Resolve a session's model backend and limits, falling back to the catalog
/// model's limits when no backend can be configured.
pub async fn resolve_session_backend(
    active: &crate::config::ActiveSettings,
    catalog: &crate::catalog::ModelCatalog,
    session_id: &str,
    manager: &Arc<ConfigManager>,
) -> (
    Option<Arc<dyn crate::model::ModelBackend>>,
    ModelLimits,
    Option<String>,
) {
    let configured = configured_backend(active, catalog, Some(session_id), manager.clone()).await;
    let warning = match &configured {
        Err(error) => Some(format!("model configuration is not usable: {error:#}")),
        Ok(_) => None,
    };
    match configured {
        Ok(Some((backend, limits))) => (Some(backend), limits, None),
        _ => {
            let limits = active
                .catalog_model(catalog)
                .await
                .map_or_else(ModelLimits::default, |model| model.limits());
            (None, limits, warning)
        }
    }
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
    let CapabilitySelection::Only(names) = &spec.tools else {
        return Ok(());
    };
    let unique = names.iter().collect::<HashSet<_>>();
    if unique.len() != names.len() {
        anyhow::bail!("Agent model tool is selected more than once");
    }
    if names.iter().any(|name| name.trim().is_empty()) {
        anyhow::bail!("Agent model tool name cannot be empty");
    }
    Ok(())
}

impl AgentHost {
    async fn build_services(
        &self,
        session: Session,
        collaboration_enabled: bool,
    ) -> Result<AgentServices> {
        let context_state = ContextState::new(session.clone());
        let spec = session.spec().await;
        let collaboration_state =
            collaboration_enabled.then(|| CollaborationState::new(session.clone()));
        let plugins = crate::builtins::plugins(&self.inner.cwd, self.inner.manager.directory());
        let startup_notices = plugins.startup_notices();
        let active = self
            .inner
            .manager
            .for_session(&spec.provider, &spec.model, spec.thinking)
            .await?;
        let mut model_tools = ModelToolRegistry::new();
        plugins.install(&mut PluginHost::new(
            &mut model_tools,
            self.inner.environment.clone(),
        ))?;
        if let Some(state) = &collaboration_state {
            model_tools.register(crate::builtins::traditional::SteerTool::new(state.clone()))?;
        }
        model_tools.register(crate::builtins::traditional::NotesTool::new(
            context_state.clone(),
        ))?;
        let codemode = crate::builtins::traditional::CodemodeTool::new();
        model_tools.register(codemode.clone())?;
        let model_tools = Arc::new(model_tools);
        codemode.bind(Arc::downgrade(&model_tools));
        let plugins = Arc::new(plugins);

        let mut startup_notices = startup_notices;
        startup_notices.extend(self.inner.catalog.warnings().await);
        let (backend, limits, configuration_warning) = resolve_session_backend(
            &active,
            &self.inner.catalog,
            session.id(),
            &self.inner.manager,
        )
        .await;
        if let Some(warning) = configuration_warning {
            startup_notices.push(warning);
        }
        let model_ready = backend.is_some();
        let context_window = limits.context_window;
        let initializer = Arc::new(AgentInitializer {
            session: session.clone(),
            plugins: plugins.clone(),
            model_tools: model_tools.clone(),
            startup_notices,
        });
        let runtime = Arc::new(AgentRuntime::new_deferred_with_context(
            backend,
            model_tools,
            session,
            initializer,
            limits,
            context_state,
        ));
        runtime.set_compaction_settings(active.compaction).await;
        runtime.set_max_output_tokens(spec.max_output_tokens).await;
        if let Some(state) = &collaboration_state {
            state.start(&runtime).await?;
        }
        Ok(AgentServices {
            runtime,
            context_window,
            model_ready,
            collaboration: collaboration_state,
            plugins,
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
    model_tools: Arc<ModelToolRegistry>,
    startup_notices: Vec<String>,
}

impl AgentInitializer {
    fn render_prompt(&self, spec: &AgentSpec) -> Result<String> {
        let tools = self
            .model_tools
            .descriptors_for(capability_names(&spec.tools))?
            .into_iter()
            .map(|descriptor| PromptEntry {
                name: descriptor.name,
                description: descriptor.description,
            })
            .collect::<Vec<_>>();
        let fragments = self.plugins.system_prompt_fragments()?;
        let generated = crate::prompts::simple_system_prompt(&tools, &fragments);
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
        let spec = self.session.spec().await;
        self.model_tools.select(capability_names(&spec.tools))?;
        let prompt = if new_session {
            let prompt = self.render_prompt(&spec)?;
            self.session
                .initialize_context(SessionContext {
                    system_prompt: prompt.clone(),
                })
                .await?;
            prompt
        } else {
            self.session.context().await.system_prompt
        };
        for text in self.startup_notices.clone() {
            self.session.append(EventKind::Notice { text }).await?;
        }
        Ok(prompt)
    }

    async fn render_system_prompt(&self, spec: &AgentSpec) -> Result<Option<String>> {
        Ok(Some(self.render_prompt(spec)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ThinkingLevel;
    use std::path::Path;

    fn spec() -> AgentSpec {
        AgentSpec::root(
            "provider",
            "model",
            ThinkingLevel::Medium,
            Path::new("/work"),
        )
    }

    #[test]
    fn agent_specs_reject_invalid_boundaries() {
        let mut invalid = spec();
        invalid.max_output_tokens = Some(0);
        assert!(validate_spec(&invalid).is_err());

        let invalid = spec().append_system_prompt("\n");
        assert!(validate_spec(&invalid).is_err());

        let invalid = spec().with_tools(["read", "read"]);
        assert!(validate_spec(&invalid).is_err());

        validate_spec(&spec().replace_system_prompt("child prompt")).unwrap();
    }
}
