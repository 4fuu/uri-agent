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
use uri_agent::tui::TuiInfo;

#[tokio::main]
async fn main() -> Result<()> {
    run_session(Config::load(Cli::parse()).await?).await
}

async fn run_session(config: Config) -> Result<()> {
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
            editor_mode: initial.editor_mode,
            picker: initial.picker,
            picker_mode: initial.picker_mode,
        },
    )
    .await
}
