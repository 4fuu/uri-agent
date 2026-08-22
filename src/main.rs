use anyhow::Result;
use clap::Parser;
use std::collections::HashSet;
use std::sync::Arc;
use uri_agent::catalog::ModelLimits;
use uri_agent::config::{Cli, Config};
use uri_agent::model::configured_backend;
use uri_agent::output::OutputStore;
use uri_agent::plugin::{CommandRegistry, PluginHost, TuiRegistry};
use uri_agent::prompts::ProtocolPrompt;
use uri_agent::protocol::{ProtocolDescriptor, ProtocolRegistry};
use uri_agent::runtime::{AgentRuntime, forward_task_notices};
use uri_agent::session::{EventKind, Session, SessionChoice, SessionContext};
use uri_agent::skill::SkillProtocol;
use uri_agent::task::TaskManager;
use uri_agent::tui::{TuiInfo, TuiOutcome, TuiServices};

struct SessionRuntime {
    runtime: Arc<AgentRuntime>,
    protocols: Vec<ProtocolDescriptor>,
    commands: Arc<CommandRegistry>,
    tui: Arc<TuiRegistry>,
    tasks: TaskManager,
    output: Arc<OutputStore>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = Config::load(Cli::parse()).await?;
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
            Some(session) => run_retained_session(&config, session).await,
            None => run_session(&config).await,
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

async fn run_session(config: &Config) -> Result<(TuiOutcome, SessionRuntime)> {
    let initial = config.manager.current().await;
    let plugins = uri_agent::builtins::plugins(&config.cwd);
    let plugin_protocols = plugins.protocol_descriptors()?;
    let plugin_prompt_fragments = plugins.system_prompt_fragments()?;
    let mut protocol_names = plugin_protocols
        .iter()
        .map(|descriptor| descriptor.name.clone())
        .collect::<HashSet<_>>();
    let mut prompt_protocols = plugin_protocols
        .iter()
        .map(|descriptor| ProtocolPrompt {
            name: descriptor.name.clone(),
            description: descriptor.description.clone(),
        })
        .collect::<Vec<_>>();
    let (discovered_skills, mut notices) = uri_agent::skill::discover(&config.cwd);
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
        prompt_protocols.push(ProtocolPrompt {
            name: protocol,
            description: format!("Skill “{}”: {}", snapshot.name, snapshot.description),
        });
        skill_snapshots.push(snapshot);
    }
    prompt_protocols.sort_by(|left, right| left.name.cmp(&right.name));
    let candidate_context = SessionContext {
        system_prompt: uri_agent::prompts::system_prompt(
            &prompt_protocols,
            &plugin_prompt_fragments,
        ),
        skills: skill_snapshots,
    };
    let requested = match &config.session {
        SessionChoice::New => None,
        SessionChoice::Latest => Some("latest"),
        SessionChoice::Existing(id) => Some(id.as_str()),
    };
    let session = Session::open(
        requested,
        &config.cwd,
        &initial.provider,
        &initial.model,
        initial.thinking,
        candidate_context,
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
    let frozen_context = session.context().await;
    if !session.is_new() {
        notices.clear();
    }
    let tasks = TaskManager::new();
    let output = Arc::new(OutputStore::new(session.id(), active.output_limit).await?);
    let mut protocols = ProtocolRegistry::new(output.clone(), tasks.clone());
    let mut commands = CommandRegistry::with_core_commands();
    let mut tui = TuiRegistry::default();
    plugins.install(&mut PluginHost {
        protocols: &mut protocols,
        commands: &mut commands,
        tui: &mut tui,
    })?;
    for snapshot in frozen_context.skills.clone() {
        let description = format!("skill {} at {}", snapshot.name, snapshot.path.display());
        let skill = match SkillProtocol::from_snapshot(snapshot) {
            Ok(skill) => skill,
            Err(error) => {
                notices.push(format!("skipped {description}: {error:#}"));
                continue;
            }
        };
        let description = format!(
            "{}:// for skill {}",
            skill.protocol_name(),
            skill.display_name()
        );
        if let Err(error) = protocols.register(skill) {
            notices.push(format!("skipped {description}: {error}"));
        }
    }
    notices.extend(config.catalog.warnings().await);

    let descriptors = protocols.descriptors();
    let configured = match configured_backend(&active, &config.catalog, Some(session.id())).await {
        Ok(configured) => configured,
        Err(error) => {
            notices.push(format!("model configuration is not usable: {error:#}"));
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
    let protocols = Arc::new(protocols);
    for notice in notices {
        session.append(EventKind::Notice { text: notice }).await?;
    }
    forward_task_notices(session.clone(), tasks.clone());
    let context_window = limits.context_window;
    let runtime = Arc::new(AgentRuntime::new(
        backend,
        protocols,
        session.clone(),
        frozen_context.system_prompt,
        limits,
    ));
    runtime.refresh_context_estimate().await;
    let commands = Arc::new(commands);
    let tui = Arc::new(tui);
    let session_runtime = SessionRuntime {
        runtime: runtime.clone(),
        protocols: descriptors,
        commands,
        tui,
        tasks,
        output,
    };
    show_session(config, session_runtime, active, context_window, model_ready).await
}

async fn run_retained_session(
    config: &Config,
    session_runtime: SessionRuntime,
) -> Result<(TuiOutcome, SessionRuntime)> {
    let runtime = session_runtime.runtime.clone();
    let result = run_retained_session_inner(config, session_runtime).await;
    if result.is_err() {
        runtime.shutdown().await;
    }
    result
}

async fn run_retained_session_inner(
    config: &Config,
    session_runtime: SessionRuntime,
) -> Result<(TuiOutcome, SessionRuntime)> {
    let runtime = session_runtime.runtime.clone();
    let session = runtime.session();
    let settings = session.model_settings().await;
    let active = config
        .manager
        .for_session(&settings.provider, &settings.model, settings.thinking)
        .await?;
    let configured = match configured_backend(&active, &config.catalog, Some(session.id())).await {
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
    show_session(config, session_runtime, active, context_window, model_ready).await
}

async fn show_session(
    config: &Config,
    session_runtime: SessionRuntime,
    active: uri_agent::config::ActiveSettings,
    context_window: usize,
    model_ready: bool,
) -> Result<(TuiOutcome, SessionRuntime)> {
    let runtime = session_runtime.runtime.clone();
    let draft = runtime.session().draft().await;
    let provider_count = config.catalog.providers().await.len();
    let outcome = uri_agent::tui::run(TuiServices {
        runtime: runtime.clone(),
        protocols: session_runtime.protocols.clone(),
        commands: session_runtime.commands.clone(),
        tui: session_runtime.tui.clone(),
        tasks: session_runtime.tasks.clone(),
        manager: config.manager.clone(),
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
            terminal: active.terminal,
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
