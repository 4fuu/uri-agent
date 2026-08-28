use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use uri_agent::catalog::ModelLimits;
use uri_agent::config::{Cli, Config};
use uri_agent::model::configured_backend;
use uri_agent::output::OutputStore;
use uri_agent::plugin::{
    CommandRegistry, ModelToolRegistry, PluginHost, PluginRegistry, TuiRegistry,
};
use uri_agent::prompts::PromptEntry;
use uri_agent::protocol::ProtocolRegistry;
use uri_agent::runtime::{AgentRuntime, RuntimeInitializer, forward_task_notices};
use uri_agent::session::{EventKind, Session, SessionChoice, SessionContext};
use uri_agent::skill::{SkillProtocol, SkillProtocolSource};
use uri_agent::subagent::SubagentService;
use uri_agent::task::TaskManager;
use uri_agent::tui::{TuiInfo, TuiOutcome, TuiServices, TuiTerminal};
use uri_agent::wasm_plugin::WasmPluginManager;

struct SessionRuntime {
    runtime: Arc<AgentRuntime>,
    protocols: Arc<ProtocolRegistry>,
    commands: Arc<CommandRegistry>,
    tui: Arc<TuiRegistry>,
    tasks: TaskManager,
    output: Arc<OutputStore>,
}

struct SessionInitializer {
    session: Session,
    plugins: Arc<PluginRegistry>,
    cwd: PathBuf,
    prompt_tools: Vec<PromptEntry>,
    prompt_protocols: Vec<PromptEntry>,
    reserved_protocols: HashSet<String>,
    skill_source: SkillProtocolSource,
    wasm_plugins: WasmPluginManager,
    startup_notices: Vec<String>,
}

#[async_trait]
impl RuntimeInitializer for SessionInitializer {
    async fn initialize(&self) -> Result<String> {
        let (context, skills, mut notices) = if self.session.is_new() {
            let plugins = self.plugins.clone();
            let cwd = self.cwd.clone();
            let prompt_tools = self.prompt_tools.clone();
            let mut prompt_protocols = self.prompt_protocols.clone();
            let mut protocol_names = self.reserved_protocols.clone();
            tokio::task::spawn_blocking(move || -> Result<_> {
                let prompt_fragments = plugins.system_prompt_fragments()?;
                let (discovered_skills, mut notices) = uri_agent::skill::discover(&cwd);
                let mut skills = Vec::new();
                let mut skill_snapshots = Vec::new();
                for skill in discovered_skills {
                    let snapshot = skill.snapshot();
                    let protocol = skill.protocol_name().to_string();
                    if !protocol_names.insert(protocol.clone()) {
                        notices.push(format!(
                            "skipped skill {} because protocol {}:// is already registered",
                            snapshot.path.display(),
                            protocol
                        ));
                        continue;
                    }
                    prompt_protocols.push(PromptEntry {
                        name: protocol,
                        description: format!("Skill “{}”: {}", snapshot.name, snapshot.description),
                    });
                    skill_snapshots.push(snapshot);
                    skills.push(skill);
                }
                prompt_protocols.sort_by(|left, right| left.name.cmp(&right.name));
                let context = SessionContext {
                    system_prompt: uri_agent::prompts::system_prompt(
                        &prompt_tools,
                        &prompt_protocols,
                        &prompt_fragments,
                    ),
                    skills: skill_snapshots,
                };
                Ok((context, skills, notices))
            })
            .await??
        } else {
            let context = self.session.context().await;
            let mut skills = Vec::new();
            let mut notices = Vec::new();
            for snapshot in context.skills.clone() {
                let description = format!("skill {} at {}", snapshot.name, snapshot.path.display());
                match SkillProtocol::from_snapshot(snapshot) {
                    Ok(skill) => skills.push(skill),
                    Err(error) => notices.push(format!("skipped {description}: {error:#}")),
                }
            }
            (context, skills, notices)
        };

        if self.session.is_new() {
            self.session.initialize_context(context.clone()).await?;
        }
        let mut reserved_protocols = self.reserved_protocols.clone();
        reserved_protocols.extend(skills.iter().map(|skill| skill.protocol_name().to_string()));
        self.skill_source.replace(skills);
        self.wasm_plugins
            .set_reserved_protocols(reserved_protocols)?;

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

        notices.extend(self.startup_notices.clone());
        for text in notices {
            self.session.append(EventKind::Notice { text }).await?;
        }
        Ok(context.system_prompt)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = Config::load(Cli::parse()).await?;
    let mut terminal = TuiTerminal::new()?;
    let mut detached_runtimes: Vec<SessionRuntime> = Vec::new();
    loop {
        let retained = match &config.session {
            SessionChoice::Existing(id) => detached_runtimes
                .iter()
                .position(|session| session.runtime.session().id() == id)
                .map(|index| detached_runtimes.remove(index)),
            SessionChoice::New | SessionChoice::Latest => None,
        };
        let result = match retained {
            Some(session) => run_retained_session(&config, session, &mut terminal).await,
            None => run_session(&config, &mut terminal).await,
        };
        let (outcome, session_runtime) = match result {
            Ok(result) => result,
            Err(error) => {
                for detached in detached_runtimes {
                    detached.runtime.shutdown().await;
                }
                return Err(error);
            }
        };
        match outcome {
            TuiOutcome::Quit => {
                for detached in detached_runtimes {
                    detached.runtime.shutdown().await;
                }
                return Ok(());
            }
            TuiOutcome::NewSession => config.session = SessionChoice::New,
            TuiOutcome::Resume(id) => config.session = SessionChoice::Existing(id),
        }
        // Preserve in-memory task/output routes as well as a possibly active turn
        // so switching back does not construct a second owner for the session.
        detached_runtimes.push(session_runtime);
    }
}

async fn run_session(
    config: &Config,
    terminal: &mut TuiTerminal,
) -> Result<(TuiOutcome, SessionRuntime)> {
    let initial = config.manager.current().await;
    let mut plugins = uri_agent::builtins::plugins(&config.cwd);
    let wasm_plugins = WasmPluginManager::new(config.manager.directory(), &config.cwd).await?;
    plugins.add(wasm_plugins.clone());
    let mut startup_notices = plugins.startup_notices();
    let plugin_protocols = plugins.protocol_descriptors()?;
    let prompt_tools = plugins
        .model_tool_descriptors()?
        .into_iter()
        .map(|descriptor| PromptEntry {
            name: descriptor.name,
            description: descriptor.description,
        })
        .collect::<Vec<_>>();
    let protocol_names = plugin_protocols
        .iter()
        .map(|descriptor| descriptor.name.clone())
        .collect::<HashSet<_>>();
    let prompt_protocols = plugin_protocols
        .iter()
        .map(|descriptor| PromptEntry {
            name: descriptor.name.clone(),
            description: descriptor.description.clone(),
        })
        .collect::<Vec<_>>();
    let requested = match &config.session {
        SessionChoice::New => None,
        SessionChoice::Latest => Some("latest"),
        SessionChoice::Existing(id) => Some(id.as_str()),
    };
    let session = Session::open_deferred(
        requested,
        &config.cwd,
        &initial.provider,
        &initial.model,
        initial.thinking,
    )
    .await?;
    let session_settings = session.model_settings().await;
    let active = config
        .manager
        .for_session(
            &session_settings.provider,
            &session_settings.model,
            session_settings.thinking,
        )
        .await?;
    let tasks = TaskManager::from_reports(session.task_reports().await);
    let output = Arc::new(OutputStore::new(session.id(), active.output_limit).await?);
    wasm_plugins.bind_output(output.clone())?;
    let mut protocols = ProtocolRegistry::new(output.clone(), tasks.clone());
    let mut model_tools = ModelToolRegistry::new();
    let mut commands = CommandRegistry::with_core_commands();
    let mut tui = TuiRegistry::default();
    let subagents = SubagentService::new(config.manager.clone());
    plugins.install(
        &mut PluginHost::new(
            &mut protocols,
            &mut model_tools,
            &mut commands,
            &mut tui,
            config.environment.clone(),
        )
        .with_credentials(config.manager.clone())
        .with_subagents(subagents.clone()),
    )?;
    wasm_plugins.set_reserved_model_tools(
        model_tools
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name),
    )?;
    model_tools.set_dynamic_source(Arc::new(wasm_plugins.clone()))?;
    startup_notices.extend(config.catalog.warnings().await);

    let configured = match configured_backend(
        &active,
        &config.catalog,
        Some(session.id()),
        config.manager.clone(),
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
                .catalog_model(&config.catalog)
                .await
                .map_or_else(ModelLimits::default, |model| model.limits()),
        ),
    };
    let skill_source = SkillProtocolSource::default();
    protocols.set_dynamic_source(Arc::new(skill_source.clone()))?;
    protocols.set_dynamic_source(Arc::new(wasm_plugins.clone()))?;
    protocols
        .restore_help_read_names(session.successful_protocol_help_reads().await)
        .await;
    let protocols = Arc::new(protocols);
    let model_tools = Arc::new(model_tools);
    let plugins = Arc::new(plugins);
    subagents.bind(
        config.cwd.clone(),
        protocols.clone(),
        model_tools.clone(),
        plugins.clone(),
        config.environment.clone(),
        output.clone(),
    )?;
    wasm_plugins.bind_host(Arc::downgrade(&protocols))?;
    let context_window = limits.context_window;
    let initializer = Arc::new(SessionInitializer {
        session: session.clone(),
        plugins,
        cwd: config.cwd.clone(),
        prompt_tools,
        prompt_protocols,
        reserved_protocols: protocol_names,
        skill_source,
        wasm_plugins,
        startup_notices,
    });
    let runtime = Arc::new(AgentRuntime::new_deferred(
        backend,
        protocols.clone(),
        model_tools,
        session.clone(),
        initializer,
        limits,
    ));
    forward_task_notices(session.clone(), tasks.clone(), Arc::downgrade(&runtime));
    runtime.set_compaction_settings(active.compaction).await;
    let startup_runtime = runtime.clone();
    tokio::spawn(async move {
        if startup_runtime.prepare_context().await.is_ok() {
            startup_runtime.refresh_context_estimate().await;
        }
    });
    let commands = Arc::new(commands);
    let tui = Arc::new(tui);
    let session_runtime = SessionRuntime {
        runtime: runtime.clone(),
        protocols,
        commands,
        tui,
        tasks,
        output,
    };
    show_session(
        config,
        session_runtime,
        active,
        context_window,
        model_ready,
        terminal,
    )
    .await
}

async fn run_retained_session(
    config: &Config,
    session_runtime: SessionRuntime,
    terminal: &mut TuiTerminal,
) -> Result<(TuiOutcome, SessionRuntime)> {
    let runtime = session_runtime.runtime.clone();
    let result = run_retained_session_inner(config, session_runtime, terminal).await;
    if result.is_err() {
        runtime.shutdown().await;
    }
    result
}

async fn run_retained_session_inner(
    config: &Config,
    session_runtime: SessionRuntime,
    terminal: &mut TuiTerminal,
) -> Result<(TuiOutcome, SessionRuntime)> {
    let runtime = session_runtime.runtime.clone();
    let session = runtime.session();
    let settings = session.model_settings().await;
    let active = config
        .manager
        .for_session(&settings.provider, &settings.model, settings.thinking)
        .await?;
    let configured = match configured_backend(
        &active,
        &config.catalog,
        Some(session.id()),
        config.manager.clone(),
    )
    .await
    {
        Ok(configured) => configured,
        Err(error) => {
            session
                .append(EventKind::Notice {
                    text: format!("model configuration is not usable: {error:#}"),
                })
                .await?;
            None
        }
    };
    let model_ready = configured.is_some();
    let (backend, limits) = match configured {
        Some((backend, limits)) => (Some(backend), limits),
        None => (
            None,
            active
                .catalog_model(&config.catalog)
                .await
                .map_or_else(ModelLimits::default, |model| model.limits()),
        ),
    };
    let context_window = limits.context_window;
    runtime.set_backend(backend, Some(limits)).await;
    session_runtime.output.set_limit(active.output_limit);
    show_session(
        config,
        session_runtime,
        active,
        context_window,
        model_ready,
        terminal,
    )
    .await
}

async fn show_session(
    config: &Config,
    session_runtime: SessionRuntime,
    active: uri_agent::config::ActiveSettings,
    context_window: usize,
    model_ready: bool,
    terminal: &mut TuiTerminal,
) -> Result<(TuiOutcome, SessionRuntime)> {
    let runtime = session_runtime.runtime.clone();
    let draft = runtime.session().draft().await;
    let provider_count = config.catalog.providers().await.len();
    let outcome = terminal
        .run(TuiServices {
            runtime: runtime.clone(),
            protocols: session_runtime.protocols.clone(),
            commands: session_runtime.commands.clone(),
            tui: session_runtime.tui.clone(),
            tasks: session_runtime.tasks.clone(),
            manager: config.manager.clone(),
            environment: config.environment.clone(),
            catalog: config.catalog.clone(),
            output: session_runtime.output.clone(),
            info: TuiInfo {
                cwd: config.cwd.clone(),
                provider: active.provider,
                model: active.model,
                thinking: active.thinking,
                session_id: runtime.session().id().to_string(),
                context_window,
                model_ready,
                provider_count,
                context_tokens: runtime.estimated_context(),
                context_accuracy: runtime.context_usage().accuracy,
                compaction_enabled: active.compaction.enabled,
                diagnostics_path: session_runtime.output.diagnostics_path(),
                terminal: active.terminal,
                key_display: active.key_display,
            },
            draft,
        })
        .await;
    match outcome {
        Ok(outcome) => Ok((outcome, session_runtime)),
        Err(error) => {
            runtime.shutdown().await;
            Err(error)
        }
    }
}
