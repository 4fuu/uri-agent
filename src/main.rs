use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use uri_agent::config::Cli;
use uri_agent::model::RigBackend;
use uri_agent::output::OutputStore;
use uri_agent::protocol::ProtocolRegistry;
use uri_agent::runtime::{AgentRuntime, forward_task_notices};
use uri_agent::session::{EventKind, Session};
use uri_agent::task::TaskManager;
use uri_agent::tui::TuiInfo;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Cli::parse().resolve()?;
    let provider = config.provider.as_str();
    let session = Session::open(
        config.session.as_deref(),
        &config.cwd,
        provider,
        &config.model,
    )
    .await?;
    let tasks = TaskManager::new();
    let output = Arc::new(OutputStore::new(session.id(), config.output_limit).await?);
    let mut protocols = ProtocolRegistry::new(output, tasks.clone());
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

    let descriptors = protocols.descriptors();
    let system_prompt =
        uri_agent::prompts::system_prompt(&config.cwd, &protocols.prompt_protocols());
    let backend = Arc::new(RigBackend::from_environment(
        config.provider,
        &config.model,
    )?);
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
        TuiInfo {
            cwd: config.cwd,
            provider: provider.to_string(),
            model: config.model,
            session_id: session.id().to_string(),
        },
    )
    .await
}
