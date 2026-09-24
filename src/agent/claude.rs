//! Claude Code adapter.
//!
//! `claude --print <prompt>` runs non-interactively. It bypasses permission
//! checks by default (issue #33); `dangerously_skip_permissions = false` opts
//! out. The first comment in a thread creates the session with `--session-id`,
//! later comments resume it with `--resume`, so the model keeps the conversation
//! context.

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
    let auto = config.dangerously_skip_permissions.unwrap_or(true);
    let agent = default_agent()
        .apply_config(config)
        .dangerously_skip_permissions(auto)
        .session(
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
    if auto {
        agent.arg("--dangerously-skip-permissions")
    } else {
        agent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<SessionStore> {
        Arc::new(SessionStore::default())
    }

    #[test]
    fn skips_permissions_by_default() {
        let agent = build(&AgentConfig::default(), store());
        assert!(
            agent
                .arguments()
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn can_opt_out_of_skipping_permissions() {
        let config = AgentConfig {
            dangerously_skip_permissions: Some(false),
            ..Default::default()
        };
        let agent = build(&config, store());
        assert!(
            !agent
                .arguments()
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
    }
}
