//! Claude Code adapter.
//!
//! `claude --print <prompt>` runs non-interactively.

use std::time::Duration;

use crate::agent::command::CommandAgent;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Claude Code adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("claude", "claude")
        .args(["--print"])
        .prompt(PromptDelivery::Arg)
        .timeout(Duration::from_secs(3600))
}

/// Build a Claude Code adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    let agent = default_agent().apply_config(config);
    if agent.dangerously_skip_permissions_enabled() {
        agent.arg("--dangerously-skip-permissions")
    } else {
        agent
    }
}
