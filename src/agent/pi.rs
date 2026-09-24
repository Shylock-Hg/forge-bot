//! Pi coding agent adapter.
//!
//! Pi takes the prompt as a positional argument in `--print` mode. It also
//! accepts a session id and creates the session when it does not exist yet, so
//! every comment in a thread keeps the same conversation.

use std::sync::Arc;

use crate::agent::command::{CommandAgent, SessionStyle};
use crate::agent::session::SessionStore;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Pi adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("pi", "pi")
        .args(["--print", "--mode", "text"])
        .prompt(PromptDelivery::Arg)
}

/// Build a Pi adapter, applying user overrides.
pub fn build(config: &AgentConfig, sessions: Arc<SessionStore>) -> CommandAgent {
    default_agent().apply_config(config).session(
        SessionStyle {
            create_args: vec!["--session-id".into(), "{session}".into()],
            // `--session-id` creates the session if it is missing, so the same
            // call resumes an existing one.
            resume_args: vec!["--session-id".into(), "{session}".into()],
            resume_at: None,
            reply_from_file: false,
            capture_id: false,
            replace_on_resume: false,
        },
        sessions,
    )
}
