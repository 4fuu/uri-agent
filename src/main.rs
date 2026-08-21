use anyhow::Result;
use clap::Parser;
use std::collections::HashSet;
use std::sync::Arc;
use uri_agent::config::{Cli, Config};
use uri_agent::model::configured_backend;
use uri_agent::output::OutputStore;
use uri_agent::plugin::{CommandRegistry, TuiRegistry};
use uri_agent::prompts::ProtocolPrompt;
use uri_agent::protocol::ProtocolRegistry;
use uri_agent::runtime::{AgentRuntime, forward_task_notices};
use uri_agent::session::{EventKind, Session, SessionChoice, SessionContext};
use uri_agent::skill::SkillProtocol;
use uri_agent::task::TaskManager;
use uri_agent::tui::{TuiInfo, TuiServices};

#[tokio::main]
async fn main() -> Result<()> {
    run_session(Config::load(Cli::parse()).await?).await
}

async fn run_session(config: Config) -> Result<()> {
    let initial = config.manager.current().await;
    let builtins = uri_agent::builtins::available(&config.cwd);
    let mut protocol_names = builtins
        .iter()
        .map(|protocol| protocol.descriptor().name)
        .collect::<HashSet<_>>();
    let mut prompt_protocols = builtins
        .iter()
        .map(|protocol| {
            let descriptor = protocol.descriptor();
            ProtocolPrompt {
                name: descriptor.name,
                description: descriptor.description,
            }
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
        system_prompt: uri_agent::prompts::system_prompt(&config.cwd, &prompt_protocols),
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
        candidate_context,
    )
    .await?;
    let frozen_context = session.context().await;
    if !session.is_new() {
        notices.clear();
    }
    let tasks = TaskManager::new();
    let output = Arc::new(OutputStore::new(session.id(), initial.output_limit).await?);
    let mut protocols = ProtocolRegistry::new(output.clone(), tasks.clone());
    for protocol in builtins {
        protocols.register_boxed(protocol)?;
    }
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
    let context_window = initial
        .catalog_model(&config.catalog)
        .await
        .map_or(128_000, |model| model.context_window());
    let runtime = Arc::new(AgentRuntime::new(
        backend,
        protocols,
        session.clone(),
        frozen_context.system_prompt,
        context_window,
    ));
    let commands = Arc::new(CommandRegistry::with_core_commands());
    let tui = Arc::new(TuiRegistry::default());

    uri_agent::tui::run(TuiServices {
        runtime,
        protocols: descriptors,
        commands,
        tui,
        tasks,
        manager: config.manager,
        catalog: config.catalog,
        output,
        info: TuiInfo {
            cwd: config.cwd,
            provider: initial.provider,
            model: initial.model,
            session_id: session.id().to_string(),
            editor: initial.editor,
            editor_mode: initial.editor_mode,
            picker: initial.picker,
            picker_mode: initial.picker_mode,
        },
    })
    .await
}
