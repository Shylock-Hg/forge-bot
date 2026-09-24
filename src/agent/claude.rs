//! Claude Code adapter.
//!
//! `claude --print <prompt>` runs non-interactively. It bypasses permission
//! checks by default (issue #33); `dangerously_skip_permissions = false` opts
//! out.

use crate::agent::command::CommandAgent;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Claude Code adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("claude", "claude")
        .args(["--print"])
        .prompt(PromptDelivery::Arg)
}

/// Build a Claude Code adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    let auto = config.dangerously_skip_permissions.unwrap_or(true);
    let agent = default_agent()
        .apply_config(config)
        .dangerously_skip_permissions(auto);
    if auto {
        agent.arg("--dangerously-skip-permissions")
    } else {
        agent
    }
}
