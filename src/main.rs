//! Command line entry point.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use forge_bot::agent::AgentRegistry;
use forge_bot::config::Config;

#[derive(Debug, Parser)]
#[command(
    name = "forge-bot",
    version,
    about = "Route forge @agent mentions to coding agents"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, env = "FORGE_BOT_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the webhook server (default).
    Serve,
    /// Validate the configuration and print a summary.
    Check,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    forge_bot::init_tracing();

    let config = Config::load(cli.config.as_deref())?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => forge_bot::serve(config).await?,
        Command::Check => print_summary(&config),
    }

    Ok(())
}

fn print_summary(config: &Config) {
    println!("bind:          {}", config.bind);
    println!("mention:       {}", config.mention);
    println!("default agent: {}", config.default_agent);

    let agents = AgentRegistry::from_config(config);
    println!("agents:        {}", agents.names().join(", "));

    let forges = forge_bot::build_adapters(config);
    let mut names: Vec<_> = forges.keys().cloned().collect();
    names.sort();
    println!("forges:        {}", names.join(", "));

    if names.is_empty() {
        eprintln!("warning: no forges are configured; the server will reject every webhook");
    }
    if config.policy.allowed_users.is_empty()
        && config.policy.allowed_repos.is_empty()
        && !config.policy.allow_all
    {
        eprintln!(
            "warning: the policy allows nobody; set allowed_users/allowed_repos or allow_all"
        );
    }
    if let Some(forgejo) = &config.forges.forgejo
        && forgejo.webhook_secret.is_none()
    {
        eprintln!("warning: forgejo webhook secret is not set; signatures will not be verified");
    }
}
