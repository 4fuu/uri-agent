//! Line-oriented REPL for the simple mode. It keeps the terminal surface to
//! one input line and one final answer per turn; tool progress goes to stderr.

use crate::agent::AgentHost;
use crate::config::Config;
use crate::execute;
use crate::session::SessionChoice;
use anyhow::Result;
use std::io::{IsTerminal, Write};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

pub async fn run(host: &AgentHost, config: &Config) -> Result<()> {
    let initial = config.manager.current().await;
    let requested = match &config.session {
        SessionChoice::New => None,
        SessionChoice::Latest => Some("latest"),
        SessionChoice::Existing(id) => Some(id.as_str()),
    };
    let spec = execute::simple_spec(
        &initial.provider,
        &initial.model,
        initial.thinking,
        &config.cwd,
        false,
    );
    let agent = host.open_root(requested, spec).await?;
    let runtime = agent.services().runtime.clone();
    runtime.prepare_context().await?;
    runtime.refresh_context_estimate().await;
    eprintln!("session {}", agent.session_id());

    let terminal = std::io::stdin().is_terminal();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        if terminal {
            eprint!("> ");
            std::io::stderr().flush()?;
        }
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/quit" | "/exit") {
            break;
        }
        let updates = runtime.session().subscribe();
        match execute::run_turn(
            &runtime,
            line,
            updates,
            |progress| eprintln!("{progress}"),
            CancellationToken::new(),
        )
        .await
        {
            Ok(answer) => {
                println!("{answer}");
                std::io::stdout().flush()?;
            }
            Err(error) => eprintln!("error: {error:#}"),
        }
    }
    agent.close().await;
    Ok(())
}
