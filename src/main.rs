use anyhow::Result;
use clap::Parser;
use uri_agent::agent::{AgentHandle, AgentHost, AgentSpec};
use uri_agent::catalog::ModelLimits;
use uri_agent::config::{Cli, Config};
use uri_agent::model::configured_backend;
use uri_agent::session::{EventKind, SessionChoice};
use uri_agent::tui::{TuiInfo, TuiOutcome, TuiServices, TuiTerminal};

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = Config::load(Cli::parse()).await?;
    let host = AgentHost::new(
        config.manager.clone(),
        config.environment.clone(),
        config.catalog.clone(),
        config.cwd.clone(),
    )
    .await?;
    if config.background {
        return host.run_background().await;
    }
    let mut terminal = TuiTerminal::new()?;
    let mut detached = Vec::<AgentHandle>::new();
    loop {
        let retained = match &config.session {
            SessionChoice::Existing(id) => detached
                .iter()
                .position(|agent| agent.session_id() == id)
                .map(|index| detached.remove(index)),
            SessionChoice::New | SessionChoice::Latest => None,
        };
        let result = match retained {
            Some(agent) => run_retained_session(&config, agent, &mut terminal).await,
            None => run_session(&config, &host, &mut terminal).await,
        };
        let (outcome, agent) = match result {
            Ok(result) => result,
            Err(error) => {
                for agent in detached {
                    agent.close().await;
                }
                return Err(error);
            }
        };
        match outcome {
            TuiOutcome::Quit => {
                agent.close().await;
                for agent in detached {
                    agent.close().await;
                }
                return Ok(());
            }
            TuiOutcome::NewSession => config.session = SessionChoice::New,
            TuiOutcome::Resume(id) => config.session = SessionChoice::Existing(id),
        }
        detached.push(agent);
    }
}

async fn run_session(
    config: &Config,
    host: &AgentHost,
    terminal: &mut TuiTerminal,
) -> Result<(TuiOutcome, AgentHandle)> {
    let initial = config.manager.current().await;
    let requested = match &config.session {
        SessionChoice::New => None,
        SessionChoice::Latest => Some("latest"),
        SessionChoice::Existing(id) => Some(id.as_str()),
    };
    let agent = host
        .open_root(
            requested,
            AgentSpec::root(
                &initial.provider,
                &initial.model,
                initial.thinking,
                &config.cwd,
            ),
        )
        .await?;
    let startup_runtime = agent.services().runtime.clone();
    tokio::spawn(async move {
        if startup_runtime.prepare_context().await.is_ok() {
            startup_runtime.refresh_context_estimate().await;
        }
    });
    let settings = agent.spec().await;
    let active = config
        .manager
        .for_session(&settings.provider, &settings.model, settings.thinking)
        .await?;
    let context_window = agent.services().context_window;
    let model_ready = agent.services().model_ready;
    show_session(config, agent, active, context_window, model_ready, terminal).await
}

async fn run_retained_session(
    config: &Config,
    agent: AgentHandle,
    terminal: &mut TuiTerminal,
) -> Result<(TuiOutcome, AgentHandle)> {
    let result = run_retained_session_inner(config, agent.clone(), terminal).await;
    if result.is_err() {
        agent.close().await;
    }
    result
}

async fn run_retained_session_inner(
    config: &Config,
    agent: AgentHandle,
    terminal: &mut TuiTerminal,
) -> Result<(TuiOutcome, AgentHandle)> {
    let runtime = agent.services().runtime.clone();
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
    agent.services().output.set_limit(active.output_limit);
    show_session(config, agent, active, context_window, model_ready, terminal).await
}

async fn show_session(
    config: &Config,
    agent: AgentHandle,
    active: uri_agent::config::ActiveSettings,
    context_window: usize,
    model_ready: bool,
    terminal: &mut TuiTerminal,
) -> Result<(TuiOutcome, AgentHandle)> {
    let runtime = agent.services().runtime.clone();
    let protocols = agent.services().protocols.clone();
    let commands = agent.services().commands.clone();
    let tui = agent.services().tui.clone();
    let tasks = agent.services().tasks.clone();
    let output = agent.services().output.clone();
    let draft = runtime.session().draft().await;
    let provider_count = config.catalog.providers().await.len();
    let outcome = terminal
        .run(TuiServices {
            runtime: runtime.clone(),
            protocols,
            commands,
            tui,
            tasks,
            manager: config.manager.clone(),
            environment: config.environment.clone(),
            catalog: config.catalog.clone(),
            output: output.clone(),
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
                diagnostics_path: output.diagnostics_path(),
                terminal: active.terminal,
                key_display: active.key_display,
            },
            draft,
        })
        .await;
    match outcome {
        Ok(outcome) => Ok((outcome, agent)),
        Err(error) => {
            agent.close().await;
            Err(error)
        }
    }
}
