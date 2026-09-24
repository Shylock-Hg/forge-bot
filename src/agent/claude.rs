//! Claude Code adapter.
//!
//! `claude --print <prompt>` runs non-interactively. The first comment in a
//! thread creates the session with `--session-id`, later comments resume it
//! with `--resume`, so the model keeps the conversation context.

use std::sync::Arc;

use crate::agent::command::{CommandAgent, SessionStyle};
use crate::agent::session::SessionStore;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Claude Code adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("claude", "claude")
        .args(["--print"])
        .prompt(PromptDelivery::Arg)
}

/// Build a Claude Code adapter, applying user overrides.
pub fn build(config: &AgentConfig, sessions: Arc<SessionStore>) -> CommandAgent {
    let agent = default_agent().apply_config(config).session(
        SessionStyle {
            create_args: vec!["--session-id".into(), "{session}".into()],
            resume_args: vec!["--resume".into(), "{session}".into()],
            resume_at: None,
            reply_from_file: false,
            capture_id: false,
            replace_on_resume: false,
        },
        sessions,
    );
    if agent.dangerously_skip_permissions_enabled() {
        agent.arg("--dangerously-skip-permissions")
    } else {
        agent
    }
}
