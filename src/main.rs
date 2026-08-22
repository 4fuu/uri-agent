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
use uri_agent::protocol::ProtocolRegistry;
use uri_agent::runtime::{AgentRuntime, forward_task_notices};
use uri_agent::session::{EventKind, Session, SessionChoice, SessionContext};
use uri_agent::skill::SkillProtocol;
use uri_agent::task::TaskManager;
use uri_agent::tui::{TuiInfo, TuiOutcome, TuiServices};

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = Config::load(Cli::parse()).await?;
    loop {
        match run_session(&config).await? {
            TuiOutcome::Quit => return Ok(()),
            TuiOutcome::NewSession => config.session = SessionChoice::New,
            TuiOutcome::Resume(id) => config.session = SessionChoice::Existing(id),
        }
    }
}

async fn run_session(config: &Config) -> Result<TuiOutcome> {
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
    let configured = match configured_backend(&initial, &config.catalog, Some(session.id())).await {
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
            initial
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
    let draft = session.draft().await;
    let provider_count = config.catalog.providers().await.len();

    uri_agent::tui::run(TuiServices {
        runtime,
        protocols: descriptors,
        commands,
        tui,
        tasks,
        manager: config.manager.clone(),
        catalog: config.catalog.clone(),
        output,
        info: TuiInfo {
            cwd: config.cwd.clone(),
            provider: initial.provider,
            model: initial.model,
            thinking: initial.thinking,
            session_id: session.id().to_string(),
            context_window,
            model_ready,
            provider_count,
            context_tokens: 0,
            terminal: initial.terminal,
        },
        draft,
    })
    .await
}
