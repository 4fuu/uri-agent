use crate::catalog::{ModelLimits, ThinkingLevel};
use crate::compaction;
use crate::config::{AgentEnvironment, ConfigManager, ModelRole, display_path};
use crate::model::{
    ModelBackend, ModelFailure, ModelFailureKind, ModelRequest, configured_backend,
    model_retry_delay, model_retry_policy,
};
use crate::output::OutputStore;
use crate::plugin::{CommandRegistry, ModelToolRegistry, PluginHost, PluginRegistry, TuiRegistry};
use crate::prompts::{self, PromptEntry};
use crate::protocol::ProtocolRegistry;
use crate::skill::SkillProtocolSource;
use crate::task::TaskManager;
use anyhow::{Context, Result, anyhow, bail};
use rig::completion::ToolDefinition;
use rig::message::{AssistantContent, Message, Text, ToolResultContent, UserContent};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SUBAGENT_DEPTH: usize = 1;

tokio::task_local! {
    static SUBAGENT_DEPTH: usize;
}

std::thread_local! {
    static BLOCKING_SUBAGENT_DEPTH: Cell<usize> = const { Cell::new(0) };
}

fn current_subagent_depth() -> usize {
    SUBAGENT_DEPTH
        .try_with(|depth| *depth)
        .unwrap_or_else(|_| BLOCKING_SUBAGENT_DEPTH.with(|depth| depth.get()))
}

fn next_subagent_depth() -> Result<usize> {
    let depth = current_subagent_depth();
    if depth >= MAX_SUBAGENT_DEPTH {
        bail!("a subagent cannot start another subagent");
    }
    Ok(depth + 1)
}

pub(crate) fn capture_subagent_depth() -> usize {
    current_subagent_depth()
}

pub(crate) fn with_blocking_subagent_depth<T>(depth: usize, operation: impl FnOnce() -> T) -> T {
    BLOCKING_SUBAGENT_DEPTH.with(|current| {
        struct RestoreDepth<'a> {
            current: &'a Cell<usize>,
            previous: usize,
        }

        impl Drop for RestoreDepth<'_> {
            fn drop(&mut self) {
                self.current.set(self.previous);
            }
        }

        let previous = current.replace(depth);
        let _restore = RestoreDepth { current, previous };
        operation()
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubagentSystemPrompt {
    Append(String),
    Replace(String),
}

#[derive(Clone, Debug)]
pub struct SubagentRequest {
    pub prompt: String,
    pub system_prompt: SubagentSystemPrompt,
    /// `None` inherits every currently registered direct tool. `Some` is an
    /// exact override, including an empty tool set.
    pub tools: Option<Vec<String>>,
    /// `None` inherits every currently registered protocol. `Some` is an
    /// exact override, including an empty protocol set.
    pub protocols: Option<Vec<String>>,
    pub working_directory: Option<PathBuf>,
    pub max_output_tokens: Option<usize>,
    pub timeout: Duration,
}

impl SubagentRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            system_prompt: SubagentSystemPrompt::Append(String::new()),
            tools: None,
            protocols: None,
            working_directory: None,
            max_output_tokens: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = SubagentSystemPrompt::Append(prompt.into());
        self
    }

    pub fn replace_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = SubagentSystemPrompt::Replace(prompt.into());
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_protocols(
        mut self,
        protocols: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.protocols = Some(protocols.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_working_directory(mut self, working_directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(working_directory.into());
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: usize) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn validate(&self) -> Result<()> {
        if self.prompt.trim().is_empty() {
            bail!("subagent prompt cannot be empty");
        }
        let system_bytes = match &self.system_prompt {
            SubagentSystemPrompt::Append(prompt) | SubagentSystemPrompt::Replace(prompt) => {
                prompt.len()
            }
        };
        if system_bytes.saturating_add(self.prompt.len()) > MAX_INPUT_BYTES {
            bail!("subagent system prompt and input exceed {MAX_INPUT_BYTES} bytes");
        }
        if self.max_output_tokens == Some(0) {
            bail!("subagent max output tokens must be greater than zero");
        }
        if self.timeout.is_zero() || self.timeout > MAX_TIMEOUT {
            bail!(
                "subagent timeout must be greater than zero and at most {} seconds",
                MAX_TIMEOUT.as_secs()
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubagentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost: f64,
}

impl SubagentUsage {
    fn add(&mut self, usage: rig::completion::Usage, limits: &ModelLimits) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(usage.reasoning_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cached_input_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        self.cost += limits.cost.total(
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
        );
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubagentResponse {
    pub text: String,
    pub role: String,
    pub provider: String,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub usage: Option<SubagentUsage>,
}

#[derive(Clone)]
struct RuntimeContext {
    cwd: PathBuf,
    protocols: Arc<ProtocolRegistry>,
    model_tools: Arc<ModelToolRegistry>,
    plugins: Arc<PluginRegistry>,
    environment: Arc<AgentEnvironment>,
    output: Arc<OutputStore>,
}

#[derive(Clone)]
struct Toolbox {
    protocols: Arc<ProtocolRegistry>,
    model_tools: Arc<ModelToolRegistry>,
    fragments: Vec<String>,
}

#[derive(Default)]
struct CapabilityExclusions {
    tools: Vec<String>,
    protocols: Vec<String>,
}

#[derive(Clone)]
#[doc(hidden)]
pub struct SubagentService {
    manager: Arc<ConfigManager>,
    runtime: Arc<OnceLock<RuntimeContext>>,
}

impl SubagentService {
    pub fn new(manager: Arc<ConfigManager>) -> Self {
        Self {
            manager,
            runtime: Arc::new(OnceLock::new()),
        }
    }

    pub fn bind(
        &self,
        cwd: PathBuf,
        protocols: Arc<ProtocolRegistry>,
        model_tools: Arc<ModelToolRegistry>,
        plugins: Arc<PluginRegistry>,
        environment: Arc<AgentEnvironment>,
        output: Arc<OutputStore>,
    ) -> Result<()> {
        self.runtime
            .set(RuntimeContext {
                cwd,
                protocols,
                model_tools,
                plugins,
                environment,
                output,
            })
            .map_err(|_| anyhow!("subagent runtime is already bound"))
    }

    pub(crate) async fn complete(
        &self,
        role_name: &str,
        request: SubagentRequest,
    ) -> Result<SubagentResponse> {
        self.complete_excluding(role_name, request, Vec::new(), Vec::new())
            .await
    }

    pub(crate) async fn complete_excluding(
        &self,
        role_name: &str,
        request: SubagentRequest,
        excluded_tools: Vec<String>,
        excluded_protocols: Vec<String>,
    ) -> Result<SubagentResponse> {
        let depth = next_subagent_depth()?;
        request.validate()?;
        let timeout = request.timeout;
        let role_name = role_name.to_string();
        let exclusions = CapabilityExclusions {
            tools: excluded_tools,
            protocols: excluded_protocols,
        };
        SUBAGENT_DEPTH
            .scope(
                depth,
                tokio::time::timeout(timeout, async {
                    let (role, settings) = self
                        .manager
                        .for_model_role(&role_name)
                        .await?
                        .with_context(|| format!("model role {role_name:?} is not configured"))?;
                    let catalog = self.manager.catalog();
                    let (backend, limits) =
                        configured_backend(&settings, &catalog, None, self.manager.clone())
                            .await?
                            .with_context(|| {
                                format!(
                                    "model role {role_name:?} has no usable credential for {}/{}",
                                    role.provider, role.model
                                )
                            })?;
                    backend.prepare().await?;
                    let toolbox = self.toolbox(request.working_directory.as_deref()).await?;
                    complete_with_backend(
                        &role_name,
                        &role,
                        &limits,
                        backend.as_ref(),
                        toolbox,
                        request,
                        &exclusions,
                    )
                    .await
                }),
            )
            .await
            .with_context(|| format!("subagent role {role_name:?} timed out after {timeout:?}"))?
    }

    async fn toolbox(&self, requested_cwd: Option<&Path>) -> Result<Toolbox> {
        let runtime = self
            .runtime
            .get()
            .cloned()
            .ok_or_else(|| anyhow!("subagent runtime is not attached"))?;
        let Some(requested_cwd) = requested_cwd else {
            let plugins = runtime.plugins.clone();
            let fragments = tokio::task::spawn_blocking(move || plugins.system_prompt_fragments())
                .await
                .context("subagent prompt worker failed")??;
            return Ok(Toolbox {
                protocols: runtime.protocols,
                model_tools: runtime.model_tools,
                fragments,
            });
        };
        let cwd = tokio::fs::canonicalize(requested_cwd)
            .await
            .with_context(|| {
                format!(
                    "cannot resolve subagent working directory {}",
                    display_path(requested_cwd)
                )
            })?;
        if cwd == runtime.cwd {
            let plugins = runtime.plugins.clone();
            let fragments = tokio::task::spawn_blocking(move || plugins.system_prompt_fragments())
                .await
                .context("subagent prompt worker failed")??;
            return Ok(Toolbox {
                protocols: runtime.protocols,
                model_tools: runtime.model_tools,
                fragments,
            });
        }
        build_toolbox(
            &cwd,
            self.manager.clone(),
            runtime.environment,
            runtime.output,
        )
        .await
    }
}

async fn build_toolbox(
    cwd: &Path,
    manager: Arc<ConfigManager>,
    environment: Arc<AgentEnvironment>,
    output: Arc<OutputStore>,
) -> Result<Toolbox> {
    let cwd = cwd.to_path_buf();
    let plugins = Arc::new(crate::builtins::subagent_plugins(&cwd));
    let fragment_plugins = plugins.clone();
    let fragments = tokio::task::spawn_blocking(move || fragment_plugins.system_prompt_fragments())
        .await
        .context("subagent prompt worker failed")??;
    let mut protocols = ProtocolRegistry::new(output, TaskManager::new());
    let mut model_tools = ModelToolRegistry::new();
    let mut commands = CommandRegistry::default();
    let mut tui = TuiRegistry::default();
    plugins.install(
        &mut PluginHost::new(
            &mut protocols,
            &mut model_tools,
            &mut commands,
            &mut tui,
            environment,
        )
        .with_credentials(manager),
    )?;
    let reserved = protocols
        .descriptors()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect::<HashSet<_>>();
    let (skills, _) = crate::skill::discover(&cwd);
    let mut names = reserved;
    let skills = skills
        .into_iter()
        .filter(|skill| names.insert(skill.protocol_name().to_string()))
        .collect::<Vec<_>>();
    let skill_source = SkillProtocolSource::default();
    skill_source.replace(skills);
    protocols.set_dynamic_source(Arc::new(skill_source))?;
    Ok(Toolbox {
        protocols: Arc::new(protocols),
        model_tools: Arc::new(model_tools),
        fragments,
    })
}

async fn complete_with_backend(
    role_name: &str,
    role: &ModelRole,
    limits: &ModelLimits,
    backend: &dyn ModelBackend,
    toolbox: Toolbox,
    request: SubagentRequest,
    exclusions: &CapabilityExclusions,
) -> Result<SubagentResponse> {
    let tool_names = select_capabilities(
        request.tools,
        toolbox
            .model_tools
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name),
        &exclusions.tools,
        "model tool",
    )?;
    let protocol_names = select_capabilities(
        request.protocols,
        toolbox
            .protocols
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name),
        &exclusions.protocols,
        "protocol",
    )?;
    let tools = toolbox.model_tools.restricted_definitions(&tool_names)?;
    let protocols = Arc::new(toolbox.protocols.restricted(&protocol_names)?);
    let system = subagent_system_prompt(
        &request.system_prompt,
        &tools,
        &protocols,
        &toolbox.fragments,
    )?;
    let mut history = vec![Message::user(request.prompt)];
    let mut usage = SubagentUsage::default();
    let mut has_usage = false;
    loop {
        let model_request = ModelRequest {
            estimated_context: compaction::estimate_request_tokens(&system, &history, &tools),
            system: system.clone(),
            history: history.clone(),
            tools: tools.clone(),
            max_output_tokens: request.max_output_tokens,
        };
        let response = complete_once(backend, model_request).await?;
        if let Some(round_usage) = response.usage {
            usage.add(round_usage, limits);
            has_usage = true;
        }
        let assistant_message = Message::Assistant {
            id: None,
            content: response.content.clone(),
        };
        let tool_calls = response
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        history.push(assistant_message);
        if tool_calls.is_empty() {
            let text = response
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            if text.is_empty() {
                bail!("subagent model returned no text");
            }
            return Ok(SubagentResponse {
                text,
                role: role_name.to_string(),
                provider: role.provider.clone(),
                model: role.model.clone(),
                thinking: role.thinking,
                usage: has_usage.then_some(usage),
            });
        }
        if tools.is_empty() {
            bail!("subagent model returned tool calls when no tools were enabled");
        }
        let mut results = Vec::with_capacity(tool_calls.len());
        for call in tool_calls {
            let name = call.function.name.clone();
            let output = if tool_names.iter().any(|allowed| allowed == &name) {
                toolbox
                    .model_tools
                    .dispatch(&name, &call.function.arguments, &protocols)
                    .await
                    .unwrap_or_else(|error| format!("Error: {error:#}"))
            } else {
                format!("Error: unknown subagent model tool: {name}")
            };
            results.push(UserContent::tool_result_for(
                call.id,
                call.provider,
                name,
                vec![ToolResultContent::Text(Text::new(output))],
            ));
        }
        history.push(Message::User { content: results });
    }
}

fn select_capabilities(
    requested: Option<Vec<String>>,
    available: impl IntoIterator<Item = String>,
    excluded: &[String],
    kind: &str,
) -> Result<Vec<String>> {
    if let Some(requested) = requested {
        if let Some(name) = requested.iter().find(|name| excluded.contains(name)) {
            bail!("subagent {kind} is unavailable in this call: {name}");
        }
        return Ok(requested);
    }
    Ok(available
        .into_iter()
        .filter(|name| !excluded.contains(name))
        .collect())
}

fn subagent_system_prompt(
    mode: &SubagentSystemPrompt,
    tools: &[ToolDefinition],
    protocols: &ProtocolRegistry,
    fragments: &[String],
) -> Result<String> {
    match mode {
        SubagentSystemPrompt::Replace(prompt) => {
            if !tools.is_empty() || !protocols.descriptors().is_empty() {
                bail!(
                    "a subagent system prompt can be replaced only when tools and protocols are empty"
                );
            }
            Ok(prompt.clone())
        }
        SubagentSystemPrompt::Append(additional) => {
            let tool_entries = tools
                .iter()
                .map(|tool| PromptEntry {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                })
                .collect::<Vec<_>>();
            let mut system =
                prompts::system_prompt(&tool_entries, &protocols.prompt_protocols(), fragments);
            if !additional.trim().is_empty() {
                if !system.ends_with('\n') {
                    system.push('\n');
                }
                system.push_str(additional);
                if !additional.ends_with('\n') {
                    system.push('\n');
                }
            }
            Ok(system)
        }
    }
}

async fn complete_once(
    backend: &dyn ModelBackend,
    request: ModelRequest,
) -> Result<crate::model::ModelResponse> {
    let mut retries = HashMap::<ModelFailureKind, usize>::new();
    loop {
        let (deltas, _receiver) = mpsc::unbounded_channel();
        match backend.complete(request.clone(), deltas).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                let Some(failure) = error.downcast_ref::<ModelFailure>() else {
                    return Err(error).context("subagent model request failed");
                };
                let Some(policy) = model_retry_policy(failure.kind()) else {
                    return Err(error).context("subagent model request failed");
                };
                let attempt = retries.entry(failure.kind()).or_default();
                if *attempt >= policy.max_retries {
                    return Err(error).context("subagent model request exhausted retries");
                }
                *attempt += 1;
                tokio::time::sleep(model_retry_delay(failure, policy, *attempt)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use rig::completion::Usage;
    use rig::message::{Text, ToolCall, ToolCallId, ToolFunction};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct EchoTool;

    #[async_trait]
    impl crate::plugin::ModelTool for EchoTool {
        fn descriptor(&self) -> crate::plugin::ModelToolDescriptor {
            crate::plugin::ModelToolDescriptor {
                name: "echo".to_string(),
                description: "Echo one message".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"],
                    "additionalProperties": false
                }),
            }
        }

        async fn execute(
            &self,
            arguments: &serde_json::Value,
            _protocols: &ProtocolRegistry,
        ) -> Result<String> {
            Ok(arguments["message"].as_str().unwrap().to_string())
        }
    }

    struct FakeBackend {
        requests: Mutex<Vec<ModelRequest>>,
        responses: Mutex<Vec<Vec<AssistantContent>>>,
    }

    #[async_trait]
    impl ModelBackend for FakeBackend {
        async fn complete(
            &self,
            request: ModelRequest,
            _deltas: mpsc::UnboundedSender<crate::model::ModelDelta>,
        ) -> Result<crate::model::ModelResponse> {
            self.requests.lock().unwrap().push(request);
            let content = self.responses.lock().unwrap().remove(0);
            Ok(crate::model::ModelResponse {
                content,
                usage: Some(Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    ..Usage::default()
                }),
                context_tokens: Some(15),
                finish_reason: None,
            })
        }
    }

    async fn empty_toolbox() -> Toolbox {
        let session_id = format!("subagent-test-{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        Toolbox {
            protocols: Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            model_tools: Arc::new(ModelToolRegistry::new()),
            fragments: Vec::new(),
        }
    }

    #[tokio::test]
    async fn replacement_completion_is_isolated_and_reports_the_serving_role() {
        let backend = FakeBackend {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(vec![vec![AssistantContent::Text(Text::new(
                "  generated title  ",
            ))]]),
        };
        let role = ModelRole {
            provider: "example".to_string(),
            model: "small-model".to_string(),
            thinking: ThinkingLevel::Low,
        };
        let response = complete_with_backend(
            "small",
            &role,
            &ModelLimits::default(),
            &backend,
            empty_toolbox().await,
            SubagentRequest::new("Fix the parser")
                .with_tools(std::iter::empty::<String>())
                .with_protocols(std::iter::empty::<String>())
                .replace_system_prompt("Write a title.")
                .with_max_output_tokens(32),
            &CapabilityExclusions::default(),
        )
        .await
        .unwrap();

        assert_eq!(response.text, "generated title");
        assert_eq!(response.role, "small");
        assert_eq!(response.provider, "example");
        assert_eq!(response.model, "small-model");
        assert_eq!(response.usage.unwrap().output_tokens, 3);
        let requests = backend.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].system, "Write a title.");
        assert!(requests[0].tools.is_empty());
        assert_eq!(requests[0].max_output_tokens, Some(32));
        assert_eq!(requests[0].history.len(), 1);
    }

    #[tokio::test]
    async fn replacement_prompt_rejects_nonempty_capabilities() {
        let protocols = empty_toolbox().await.protocols;
        let tool = ToolDefinition {
            name: "demo".to_string(),
            description: "demo".to_string(),
            parameters: serde_json::json!({}),
        };
        assert!(
            subagent_system_prompt(
                &SubagentSystemPrompt::Replace("replacement".to_string()),
                &[tool],
                &protocols,
                &[],
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn subagent_runs_selected_tools_and_appends_instructions_to_generated_prompt() {
        let backend = FakeBackend {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(vec![
                vec![AssistantContent::ToolCall(ToolCall::new(
                    ToolCallId::new("call-1").unwrap(),
                    ToolFunction::new(
                        "echo".to_string(),
                        serde_json::json!({"message": "tool result"}),
                    ),
                ))],
                vec![AssistantContent::Text(Text::new("finished"))],
            ]),
        };
        let role = ModelRole {
            provider: "example".to_string(),
            model: "tool-model".to_string(),
            thinking: ThinkingLevel::Medium,
        };
        let mut toolbox = empty_toolbox().await;
        Arc::get_mut(&mut toolbox.model_tools)
            .unwrap()
            .register(EchoTool)
            .unwrap();
        toolbox.fragments.push("project fragment".to_string());
        let response = complete_with_backend(
            "default",
            &role,
            &ModelLimits::default(),
            &backend,
            toolbox,
            SubagentRequest::new("Use the tool")
                .with_tools(["echo"])
                .with_protocols(std::iter::empty::<String>())
                .append_system_prompt("Additional subagent rule."),
            &CapabilityExclusions::default(),
        )
        .await
        .unwrap();

        assert_eq!(response.text, "finished");
        assert_eq!(response.usage.unwrap().output_tokens, 6);
        let requests = backend.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools.len(), 1);
        assert!(requests[0].system.contains("project fragment"));
        assert!(requests[0].system.ends_with("Additional subagent rule.\n"));
        assert_eq!(requests[1].history.len(), 3);
        assert!(matches!(&requests[1].history[2], Message::User { content }
            if matches!(&content[0], UserContent::ToolResult(result)
                if matches!(&result.content[0], ToolResultContent::Text(text)
                    if text.text == "tool result"))));
    }

    #[tokio::test]
    async fn custom_working_directory_rebuilds_rooted_protocols_and_prompt_fragments() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let project = root.path().join("project");
        tokio::fs::create_dir_all(&project).await.unwrap();
        tokio::fs::write(project.join("AGENTS.md"), "Custom subagent rules.")
            .await
            .unwrap();
        tokio::fs::write(project.join("sample.txt"), "custom cwd content")
            .await
            .unwrap();
        let manager = ConfigManager::load_for_test(&config, &project)
            .await
            .unwrap();
        let environment = Arc::new(AgentEnvironment::load(&config).await.unwrap());
        let output = Arc::new(
            OutputStore::new(
                &format!("subagent-cwd-{}", uuid::Uuid::now_v7().simple()),
                1024,
            )
            .await
            .unwrap(),
        );
        let toolbox = build_toolbox(&project, manager, environment, output)
            .await
            .unwrap();

        assert!(
            toolbox
                .fragments
                .iter()
                .any(|fragment| fragment.contains("Custom subagent rules."))
        );
        toolbox.protocols.read("file://help", "").await.unwrap();
        assert_eq!(
            toolbox
                .protocols
                .read("file://sample.txt", "")
                .await
                .unwrap(),
            "custom cwd content\n"
        );
    }

    #[test]
    fn request_rejects_empty_oversized_and_unbounded_inputs() {
        assert!(SubagentRequest::new(" ").validate().is_err());
        assert!(
            SubagentRequest::new("x".repeat(MAX_INPUT_BYTES + 1))
                .validate()
                .is_err()
        );
        assert!(
            SubagentRequest::new("prompt")
                .with_timeout(MAX_TIMEOUT + Duration::from_secs(1))
                .validate()
                .is_err()
        );
        assert!(
            SubagentRequest::new("prompt")
                .with_max_output_tokens(0)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn capability_exclusions_filter_inheritance_and_reject_explicit_selection() {
        let available = ["read", "plugin_tool"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let excluded = ["plugin_tool".to_string()];
        assert_eq!(
            select_capabilities(None, available.clone(), &excluded, "model tool").unwrap(),
            vec!["read"]
        );
        let error = select_capabilities(
            Some(vec!["plugin_tool".to_string()]),
            available,
            &excluded,
            "model tool",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unavailable in this call"));
    }

    #[tokio::test]
    async fn subagent_depth_is_one_across_async_and_blocking_plugin_calls() {
        assert_eq!(next_subagent_depth().unwrap(), 1);
        SUBAGENT_DEPTH
            .scope(1, async {
                let error = next_subagent_depth().unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "a subagent cannot start another subagent"
                );

                let depth = capture_subagent_depth();
                let error = tokio::task::spawn_blocking(move || {
                    with_blocking_subagent_depth(depth, next_subagent_depth)
                })
                .await
                .unwrap()
                .unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "a subagent cannot start another subagent"
                );
            })
            .await;
        assert_eq!(capture_subagent_depth(), 0);
    }
}
