//! Pi coding agent adapter.
//!
//! Pi takes the prompt as a positional argument in `--print` mode. It runs
//! with `--approve` by default (issue #33) so project-local files are trusted
//! without a prompt; `dangerously_skip_permissions = false` opts out.

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
    let auto = config.dangerously_skip_permissions.unwrap_or(true);
    let agent = default_agent()
        .apply_config(config)
        .dangerously_skip_permissions(auto);
    if auto { agent.arg("--approve") } else { agent }
}
