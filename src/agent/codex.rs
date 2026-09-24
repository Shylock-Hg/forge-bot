//! Codex CLI adapter.
//!
//! `codex exec` runs non-interactively and reads the prompt from stdin when no
//! prompt argument is given. There is no terminal to answer approval prompts,
//! so the adapter also selects a sandbox: `workspace-write` by default, which
//! lets the agent edit the checkout, or a full bypass when the operator opts in
//! with `dangerously_skip_permissions = true`.
//!
//! Codex persists each conversation under a `thread_id`. A later comment in the
//! same thread resumes it with `codex exec resume <id>`, which keeps the model
//! context (and the provider's prompt cache) warm.

use std::sync::Arc;

use crate::agent::command::{CommandAgent, SessionStyle};
use crate::agent::session::SessionStore;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Codex adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("codex", "codex")
        .args(["exec", "--skip-git-repo-check", "--color", "never"])
        .prompt(PromptDelivery::Stdin)
}

/// Build a Codex adapter, applying user overrides.
pub fn build(config: &AgentConfig, sessions: Arc<SessionStore>) -> CommandAgent {
    let agent = default_agent().apply_config(config);
    let bypass = agent.dangerously_skip_permissions_enabled();
    let agent = if bypass {
        agent.arg("--dangerously-bypass-approvals-and-sandbox")
    } else {
        agent.args(["--sandbox", "workspace-write"])
    };

    // `codex exec resume` rejects `--color`/`--sandbox`, so the resume command
    // stands alone and inherits the sandbox recorded on the session.
    let mut resume_args = vec![
        "exec".into(),
        "resume".into(),
        "{session}".into(),
        "--skip-git-repo-check".into(),
        "-o".into(),
        "{reply_file}".into(),
    ];
    if bypass {
        resume_args.push("--dangerously-bypass-approvals-and-sandbox".into());
    }

    agent.session(
        SessionStyle {
            // A fresh conversation reports its id on stdout as `thread.started`
            // and writes the final message to `-o`.
            create_args: vec!["--json".into(), "-o".into(), "{reply_file}".into()],
            resume_args,
            resume_at: None,
            reply_from_file: true,
            capture_id: true,
            replace_on_resume: true,
        },
        sessions,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    fn store() -> Arc<SessionStore> {
        Arc::new(SessionStore::default())
    }

    #[test]
    fn defaults_use_codex_exec_with_stdin_prompt() {
        let agent = default_agent();
        assert_eq!(agent.name(), "codex");
        assert_eq!(agent.program(), "codex");
        assert_eq!(
            &agent.arguments()[..3],
            ["exec", "--skip-git-repo-check", "--color"]
        );
    }

    #[test]
    fn enables_workspace_write_sandbox_by_default() {
        let agent = build(&AgentConfig::default(), store());
        assert!(agent.arguments().iter().any(|a| a == "--sandbox"));
        assert!(agent.arguments().iter().any(|a| a == "workspace-write"));
        assert!(
            !agent
                .arguments()
                .iter()
                .any(|a| a == "--dangerously-bypass-approvals-and-sandbox")
        );
    }

    #[test]
    fn bypass_replaces_the_sandbox_when_requested() {
        let config = AgentConfig {
            dangerously_skip_permissions: Some(true),
            ..Default::default()
        };
        let agent = build(&config, store());
        assert!(
            agent
                .arguments()
                .iter()
                .any(|a| a == "--dangerously-bypass-approvals-and-sandbox")
        );
        assert!(!agent.arguments().iter().any(|a| a == "--sandbox"));
    }

    #[test]
    fn user_args_override_defaults_but_still_get_a_sandbox() {
        let config = AgentConfig {
            args: Some(vec!["exec".into(), "-".into()]),
            ..Default::default()
        };
        let agent = build(&config, store());
        assert_eq!(&agent.arguments()[..2], ["exec", "-"]);
        assert!(agent.arguments().iter().any(|a| a == "workspace-write"));
    }
}
