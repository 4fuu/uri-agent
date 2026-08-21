use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use uri_agent::config::{Cli, Config};
use uri_agent::model::configured_backend;
use uri_agent::output::OutputStore;
use uri_agent::protocol::ProtocolRegistry;
use uri_agent::runtime::{AgentRuntime, forward_task_notices};
use uri_agent::session::{EventKind, Session, SessionChoice};
use uri_agent::task::TaskManager;
use uri_agent::tui::{SessionLaunch, TuiExit, TuiInfo};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let startup_cwd = cli
        .cwd
        .clone()
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    let mut launch = initial_launch(&cli, &startup_cwd).await?;
    while let Some(selected) = launch {
        let config = Config::load(cli_for_launch(&cli, &selected)).await?;
        let exit = run_session(config).await?;
        launch = match exit {
            TuiExit::Quit => None,
            TuiExit::Sessions => uri_agent::tui::select_session(&selected.cwd).await?,
        };
    }
    Ok(())
}

async fn initial_launch(cli: &Cli, startup_cwd: &std::path::Path) -> Result<Option<SessionLaunch>> {
    if cli.cwd.is_none() && !cli.continue_session && cli.session.is_none() {
        return uri_agent::tui::select_session(startup_cwd).await;
    }
    let session = if cli.continue_session {
        SessionChoice::Latest
    } else if let Some(id) = &cli.session {
        SessionChoice::Existing(id.clone())
    } else {
        SessionChoice::New
    };
    let cwd = if cli.cwd.is_some() {
        startup_cwd.to_path_buf()
    } else {
        let sessions = Session::list(startup_cwd).await?;
        let summary = match &session {
            SessionChoice::Latest => sessions.first(),
            SessionChoice::Existing(id) => sessions.iter().find(|session| &session.id == id),
            SessionChoice::New => None,
        };
        summary
            .map(|session| session.cwd.clone())
            .unwrap_or_else(|| startup_cwd.to_path_buf())
            .canonicalize()?
    };
    Ok(Some(SessionLaunch { cwd, session }))
}

fn cli_for_launch(cli: &Cli, launch: &SessionLaunch) -> Cli {
    let mut selected = cli.clone();
    selected.cwd = Some(launch.cwd.clone());
    selected.continue_session = matches!(launch.session, SessionChoice::Latest);
    selected.session = match &launch.session {
        SessionChoice::Existing(id) => Some(id.clone()),
        SessionChoice::New | SessionChoice::Latest => None,
    };
    selected
}

async fn run_session(config: Config) -> Result<TuiExit> {
    let initial = config.manager.current().await;
    let requested = match &config.session {
        SessionChoice::New => None,
        SessionChoice::Latest => Some("latest"),
        SessionChoice::Existing(id) => Some(id.as_str()),
    };
    let session = Session::open(requested, &config.cwd, &initial.provider, &initial.model).await?;
    let tasks = TaskManager::new();
    let output = Arc::new(OutputStore::new(session.id(), initial.output_limit).await?);
    let mut protocols = ProtocolRegistry::new(output.clone(), tasks.clone());
    uri_agent::builtins::register(&mut protocols, &config.cwd)?;

    let (skills, mut notices) = uri_agent::skill::discover(&config.cwd);
    for skill in skills {
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
    let system_prompt =
        uri_agent::prompts::system_prompt(&config.cwd, &protocols.prompt_protocols());
    let backend = match configured_backend(&initial, &config.catalog).await {
        Ok(backend) => backend,
        Err(error) => {
            notices.push(format!("model configuration is not usable: {error:#}"));
            None
        }
    };
    let protocols = Arc::new(protocols);
    for notice in notices {
        session.append(EventKind::Notice { text: notice }).await?;
    }
    forward_task_notices(session.clone(), tasks.clone());
    let runtime = Arc::new(AgentRuntime::new(
        backend,
        protocols,
        session.clone(),
        system_prompt,
    ));

    uri_agent::tui::run(
        runtime,
        descriptors,
        tasks,
        config.manager,
        config.catalog,
        output,
        TuiInfo {
            cwd: config.cwd,
            provider: initial.provider,
            model: initial.model,
            session_id: session.id().to_string(),
            editor: initial.editor,
        },
    )
    .await
}
