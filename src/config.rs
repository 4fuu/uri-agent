use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Provider {
    Openai,
    Anthropic,
    Gemini,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    /// Model provider. The matching provider API key must be present in the environment.
    #[arg(long, env = "URI_AGENT_PROVIDER", default_value = "openai")]
    pub provider: Provider,

    /// Provider model identifier.
    #[arg(long, env = "URI_AGENT_MODEL")]
    pub model: Option<String>,

    /// Working directory exposed to built-in protocols.
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,

    /// Resume an existing session by ID. Use `latest` for the most recent session.
    #[arg(long)]
    pub session: Option<String>,

    /// Maximum model-visible bytes before complete output is written to a file.
    #[arg(long, default_value_t = 32 * 1024)]
    pub output_limit: usize,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    pub cwd: PathBuf,
    pub session: Option<String>,
    pub output_limit: usize,
}

impl Cli {
    pub fn resolve(self) -> Result<Config> {
        let cwd = self
            .cwd
            .canonicalize()
            .with_context(|| format!("working directory does not exist: {}", self.cwd.display()))?;
        let model = self.model.unwrap_or_else(|| match self.provider {
            Provider::Openai => "gpt-5.2".to_string(),
            Provider::Anthropic => "claude-sonnet-4-6".to_string(),
            Provider::Gemini => "gemini-3-flash-preview".to_string(),
        });
        Ok(Config {
            provider: self.provider,
            model,
            cwd,
            session: self.session,
            output_limit: self.output_limit.max(1024),
        })
    }
}
