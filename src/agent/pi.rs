//! Pi coding agent adapter.
//!
//! Pi takes the prompt as a positional argument in `--print` mode.

use crate::agent::command::CommandAgent;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Pi adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("pi", "pi")
        .args(["--print", "--mode", "text"])
        .prompt(PromptDelivery::Arg)
}

/// Build a Pi adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    default_agent().apply_config(config)
}
