//! Codex CLI adapter.
//!
//! `codex exec` reads the prompt from stdin when none is given as an argument.

use std::time::Duration;

use crate::agent::command::CommandAgent;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Codex adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("codex", "codex")
        .args(["exec", "--skip-git-repo-check"])
        .prompt(PromptDelivery::Stdin)
        .timeout(Duration::from_secs(3600))
}

/// Build a Codex adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    let agent = default_agent().apply_config(config);
    if agent.dangerously_skip_permissions_enabled() {
        agent.arg("--dangerously-bypass-approvals-and-sandbox")
    } else {
        agent
    }
}
