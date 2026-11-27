use anyhow::Result;
use clap::Parser;
use uat::agent::AgentHost;
use uat::config::{Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(uat::config::Subcommand::Docs { topic }) = &cli.command {
        print!("{}", uat::builtins::docs_output(topic.as_deref())?);
        return Ok(());
    }
    let models = match &cli.command {
        Some(uat::config::Subcommand::Models { all }) => Some(*all),
        _ => None,
    };
    let serve_listen = match &cli.command {
        Some(uat::config::Subcommand::Serve { listen }) => Some(listen.clone()),
        _ => None,
    };
    if models.is_some() && cli.execute.is_some() {
        anyhow::bail!("`models` cannot be combined with --execute");
    }
    if serve_listen.is_some() && cli.execute.is_some() {
        anyhow::bail!("`serve` cannot be combined with --execute");
    }
    let execute_prompt = uat::execute::read_prompt(cli.execute.as_deref()).await?;
    let config = Config::load(cli).await?;
    if let Some(all) = models {
        if let Some(warning) = config.refresh_catalog_for_cli().await {
            eprintln!("warning: {warning}");
        }
        let listing = config.model_listing(all).await;
        if listing.is_empty() {
            eprintln!(
                "no runnable models: configure a provider credential (see `uat --help`), \
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
    if let Some(listen) = serve_listen {
        return uat::serve::serve(host, config, &listen).await;
    }
    if let Some(prompt) = execute_prompt {
        return uat::execute::run(&host, &config, &prompt).await;
    }
    uat::repl::run(&host, &config).await
}
