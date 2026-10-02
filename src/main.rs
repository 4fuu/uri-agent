use anyhow::Result;
use clap::Parser;
use uri_agent::agent::{AgentHandle, AgentHost, AgentSpec};
use uri_agent::config::{Cli, Config};
use uri_agent::herdr::HerdrReporter;
use uri_agent::moshi::MoshiReporter;
use uri_agent::session::{EventKind, SessionChoice};
use uri_agent::tui::{TuiInfo, TuiOutcome, TuiServices, TuiTerminal};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(uri_agent::config::Subcommand::Docs { topic }) = &cli.command {
        print!("{}", uri_agent::builtins::docs_output(topic.as_deref())?);
        return Ok(());
    }
    let models = match &cli.command {
        Some(uri_agent::config::Subcommand::Models { all }) => Some(*all),
        _ => None,
    };
    if models.is_some() && (cli.acpv1 || cli.execute.is_some()) {
        anyhow::bail!("`models` cannot be combined with --acpv1 or --execute");
    }
    if cli.acpv1 {
        return uri_agent::acp::v1::serve(cli).await;
    }
    let execute_prompt = uri_agent::execute::read_prompt(cli.execute.as_deref()).await?;
    let mut config = Config::load(cli).await?;
    if let Some(all) = models {
        if let Some(warning) = config.refresh_catalog_for_cli().await {
            eprintln!("warning: {warning}");
        }
        let listing = config.model_listing(all).await;
        if listing.is_empty() {
            eprintln!(
                "no runnable models: configure a provider credential (see `uri-agent --help`), \
or pass --all to list every catalog model"
            );
        }
        print!("{listing}");
        return Ok(());
    }
    let host = AgentHost::new(
        config.manager.clone(),
        config.environment.clone(),
        config.catalog.clone(),
        config.cwd.clone(),
    )
    .await?;
    if let Some(prompt) = execute_prompt {
        return uri_agent::execute::run(&host, &config, &prompt).await;
    }
    let mut terminal = TuiTerminal::new()?;
    let herdr = HerdrReporter::from_env();
    let moshi = MoshiReporter::from_env();
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
            Some(agent) => {
                run_retained_session(
                    &config,
                    agent,
                    &mut terminal,
                    herdr.as_ref(),
                    moshi.as_ref(),
                )
                .await
            }
            None => {
                run_session(
                    &config,
                    &host,
                    &mut terminal,
                    herdr.as_ref(),
                    moshi.as_ref(),
                )
                .await
            }
        };
        let (outcome, agent) = match result {
            Ok(result) => result,
            Err(error) => {
                for agent in detached {
                    agent.close().await;
                }
                if let Some(reporter) = &herdr {
                    reporter.shutdown().await;
                }
                if let Some(reporter) = &moshi {
                    reporter.shutdown().await;
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
                if let Some(reporter) = &herdr {
                    reporter.shutdown().await;
                }
                if let Some(reporter) = &moshi {
                    reporter.shutdown().await;
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
    herdr: Option<&HerdrReporter>,
    moshi: Option<&MoshiReporter>,
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
    let (terminal_title, title_receiver) = tokio::sync::watch::channel(String::new());
    if let Some(reporter) = herdr {
        reporter
            .start(agent.services().runtime.clone(), title_receiver.clone())
            .await;
    }
    if let Some(reporter) = moshi {
        reporter
            .start(agent.services().runtime.clone(), title_receiver)
            .await;
    }
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
    show_session(
        config,
        agent,
        active,
        context_window,
        model_ready,
        terminal_title,
        terminal,
    )
    .await
}

async fn run_retained_session(
    config: &Config,
    agent: AgentHandle,
    terminal: &mut TuiTerminal,
    herdr: Option<&HerdrReporter>,
    moshi: Option<&MoshiReporter>,
) -> Result<(TuiOutcome, AgentHandle)> {
    let result = run_retained_session_inner(config, agent.clone(), terminal, herdr, moshi).await;
    if result.is_err() {
        agent.close().await;
    }
    result
}

async fn run_retained_session_inner(
    config: &Config,
    agent: AgentHandle,
    terminal: &mut TuiTerminal,
    herdr: Option<&HerdrReporter>,
    moshi: Option<&MoshiReporter>,
) -> Result<(TuiOutcome, AgentHandle)> {
    let runtime = agent.services().runtime.clone();
    let (terminal_title, title_receiver) = tokio::sync::watch::channel(String::new());
    if let Some(reporter) = herdr {
        reporter
            .start(runtime.clone(), title_receiver.clone())
            .await;
    }
    if let Some(reporter) = moshi {
        reporter.start(runtime.clone(), title_receiver).await;
    }
    let session = runtime.session();
    let settings = session.model_settings().await;
    let active = config
        .manager
        .for_session(&settings.provider, &settings.model, settings.thinking)
        .await?;
    let (backend, limits, configuration_warning) = uri_agent::agent::resolve_session_backend(
        &active,
        &config.catalog,
        session.id(),
        &config.manager,
    )
    .await;
    if let Some(warning) = configuration_warning {
        session.append(EventKind::Notice { text: warning }).await?;
    }
    let model_ready = backend.is_some();
    let context_window = limits.context_window;
    runtime.set_backend(backend, Some(limits)).await;
    agent.services().output.set_limit(active.output_limit);
    show_session(
        config,
        agent,
        active,
        context_window,
        model_ready,
        terminal_title,
        terminal,
    )
    .await
}

async fn show_session(
    config: &Config,
    agent: AgentHandle,
    active: uri_agent::config::ActiveSettings,
    context_window: usize,
    model_ready: bool,
    terminal_title: tokio::sync::watch::Sender<String>,
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
    let model_roles = agent.services().declared_model_roles()?;
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
                context_strategy: runtime.context_strategy().await,
                diagnostics_path: output.diagnostics_path(),
                terminal: active.terminal,
                key_display: active.key_display,
                layout: active.layout,
            },
            draft,
            model_roles,
            terminal_title,
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
