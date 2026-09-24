//! forge-bot — a forge-agnostic bot that routes `@agent` mentions to coding
//! agents.
//!
//! The architecture is deliberately small:
//!
//! ```text
//! Forge → webhook → ForgeAdapter → ForgeMessage
//!       → mention + policy → Dispatcher → AgentAdapter → CLI agent
//! ```
//!
//! The gateway only ever hands an agent a location URL and a message; the agent
//! is responsible for reading context, changing code, testing, pushing and
//! replying.

pub mod agent;
pub mod config;
pub mod error;
pub mod forge;
pub mod forge_api;
pub mod location;
pub mod mention;
pub mod policy;
pub mod poller;
pub mod session;
pub mod webhook;
pub mod workspace;

use std::collections::HashMap;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use crate::agent::AgentRegistry;
use crate::config::Config;
use crate::error::Result;
use crate::forge::ForgeAdapter;
use crate::forge::forgejo::ForgejoAdapter;
use crate::forge::github::GithubAdapter;
use crate::forge::gitlab::GitlabAdapter;
use crate::forge_api::HttpForgeApi;
use crate::policy::Policy;
use crate::session::{Dispatcher, SessionStore};
use crate::webhook::AppState;

/// Initialize tracing. Safe to call more than once.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("forge_bot=info,tower_http=info,axum=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Instantiate the forge adapters enabled by configuration.
pub fn build_adapters(config: &Config) -> HashMap<String, Arc<dyn ForgeAdapter>> {
    let mut adapters: HashMap<String, Arc<dyn ForgeAdapter>> = HashMap::new();

    if let Some(forgejo) = &config.forges.forgejo {
        adapters.insert("forgejo".into(), Arc::new(ForgejoAdapter::new(forgejo)));
        // Gitea shares the Forgejo wire format.
        adapters.insert("gitea".into(), Arc::new(ForgejoAdapter::new(forgejo)));
    }
    if let Some(github) = &config.forges.github {
        adapters.insert("github".into(), Arc::new(GithubAdapter::new(github)));
    }
    if let Some(gitlab) = &config.forges.gitlab {
        adapters.insert("gitlab".into(), Arc::new(GitlabAdapter::new(gitlab)));
    }

    adapters
}

/// Build the policy from configuration, adding the configured bot users to the
/// ignore list.
pub fn build_policy(config: &Config) -> Policy {
    let mut policy = Policy::new(&config.policy);
    if let Some(forgejo) = &config.forges.forgejo
        && let Some(user) = &forgejo.bot_username
    {
        policy.ignore_user(user);
    }
    if let Some(github) = &config.forges.github
        && let Some(user) = &github.bot_username
    {
        policy.ignore_user(user);
    }
    if let Some(gitlab) = &config.forges.gitlab
        && let Some(user) = &gitlab.bot_username
    {
        policy.ignore_user(user);
    }
    policy
}

/// Assemble the complete application state.
pub fn build_app(config: Config) -> Result<AppState> {
    let config = Arc::new(config);

    let adapters = build_adapters(&config);
    let agents = AgentRegistry::from_config(&config);
    let policy = build_policy(&config);
    let sessions = Arc::new(SessionStore::open(config::expand_tilde(
        &config.session.dir,
    ))?);
    let api = Arc::new(HttpForgeApi::new((*config).clone())?);
    let dispatcher = Dispatcher::new(config.clone(), agents, sessions, api, policy)?;

    Ok(AppState::new(
        config.clone(),
        adapters,
        AgentRegistry::from_config(&config),
        dispatcher,
    ))
}

/// Run the webhook server until the process is stopped.
pub async fn serve(config: Config) -> Result<()> {
    let bind = config.bind.clone();
    let poller_enabled = config.poller.enabled;
    let app = build_app(config)?;

    if poller_enabled {
        let poller = Arc::new(poller::Poller::new(
            app.config.clone(),
            app.dispatcher.clone(),
        )?);
        tokio::spawn(poller.run());
    }

    let router = webhook::router(app);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| crate::error::BotError::Config(format!("cannot bind {bind}: {e}")))?;

    tracing::info!(%bind, "forge-bot listening");
    axum::serve(listener, router).await?;
    Ok(())
}

/// Run only the polling ingester (no webhook listener).
pub async fn poll(config: Config) -> Result<()> {
    let app = build_app(config)?;
    let poller = Arc::new(poller::Poller::new(
        app.config.clone(),
        app.dispatcher.clone(),
    )?);
    poller.run().await;
    Ok(())
}
