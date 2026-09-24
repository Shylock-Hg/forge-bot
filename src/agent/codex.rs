//! Codex CLI adapter.
//!
//! `codex exec` runs non-interactively and reads the prompt from stdin when no
//! prompt argument is given. There is no terminal to answer approval prompts,
//! so the adapter runs without the sandbox by default (issue #33); the Linux
//! sandbox needs `bwrap`, which is unavailable on some hosts. An operator can
//! opt back into the `workspace-write` sandbox with
//! `dangerously_skip_permissions = false`.

use crate::agent::command::CommandAgent;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Codex adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("codex", "codex")
        .args(["exec", "--skip-git-repo-check", "--color", "never"])
        .prompt(PromptDelivery::Stdin)
}

/// Build a Codex adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    let auto = config.dangerously_skip_permissions.unwrap_or(true);
    let agent = default_agent()
        .apply_config(config)
        .dangerously_skip_permissions(auto);
    if auto {
        agent.arg("--dangerously-bypass-approvals-and-sandbox")
    } else {
        agent.args(["--sandbox", "workspace-write"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

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
    fn bypasses_the_sandbox_by_default() {
        let agent = build(&AgentConfig::default());
        assert!(
            agent
                .arguments()
                .iter()
                .any(|a| a == "--dangerously-bypass-approvals-and-sandbox")
        );
        assert!(!agent.arguments().iter().any(|a| a == "--sandbox"));
    }

    #[test]
    fn opt_out_restores_the_workspace_write_sandbox() {
        let config = AgentConfig {
            dangerously_skip_permissions: Some(false),
            ..Default::default()
        };
        let agent = build(&config);
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
    fn user_args_override_defaults_but_still_get_the_bypass() {
        let config = AgentConfig {
            args: Some(vec!["exec".into(), "-".into()]),
            ..Default::default()
        };
        let agent = build(&config);
        assert_eq!(&agent.arguments()[..2], ["exec", "-"]);
        assert!(
            agent
                .arguments()
                .iter()
                .any(|a| a == "--dangerously-bypass-approvals-and-sandbox")
        );
    }
}
