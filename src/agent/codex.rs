//! Codex CLI adapter.
//!
//! `codex exec` runs non-interactively and reads the prompt from stdin when no
//! prompt argument is given. It always runs with
//! `--dangerously-bypass-approvals-and-sandbox` (issue #33): the bot is
//! non-interactive, and the Linux sandbox needs `bwrap`, which is unavailable
//! on some hosts. Codex is never run inside its sandbox.

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
    default_agent()
        .apply_config(config)
        .arg("--dangerously-bypass-approvals-and-sandbox")
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
    fn always_bypasses_the_sandbox() {
        for config in [
            AgentConfig::default(),
            AgentConfig {
                dangerously_skip_permissions: Some(false),
                ..Default::default()
            },
        ] {
            let agent = build(&config);
            assert!(
                agent
                    .arguments()
                    .iter()
                    .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"),
                "codex must never run in a sandbox"
            );
            assert!(!agent.arguments().iter().any(|a| a == "--sandbox"));
        }
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
