//! Kimi CLI adapter.
//!
//! The Kimi CLI is not installed everywhere; this adapter provides sensible
//! defaults that can be fully overridden through configuration.

use std::time::Duration;

use crate::agent::command::CommandAgent;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Kimi adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("kimi", "kimi")
        .args(["--print"])
        .prompt(PromptDelivery::Arg)
        .timeout(Duration::from_secs(3600))
}

/// Build a Kimi adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    default_agent().apply_config(config)
}
