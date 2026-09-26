//! Antigravity CLI adapter.
//!
//! `agy --print <prompt>` runs one headless turn and writes the response to
//! stdout. In headless mode, tools that need approval are denied unless the
//! operator configures permissions or uses `--dangerously-skip-permissions`.

use crate::agent::command::CommandAgent;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Antigravity CLI adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("agy", "agy")
        .arg("--print")
        .prompt(PromptDelivery::Arg)
}

/// Build an Antigravity CLI adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    let auto = config.dangerously_skip_permissions.unwrap_or(true);
    let agent = default_agent()
        .apply_config(config)
        .dangerously_skip_permissions(auto);
    if auto {
        agent.arg("--dangerously-skip-permissions")
    } else {
        agent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentContext, AgentRequest};

    #[test]
    fn defaults_run_headless_with_permissions() {
        let agent = build(&AgentConfig::default());
        assert_eq!(agent.name(), "agy");
        assert_eq!(agent.program(), "agy");
        assert_eq!(
            agent.arguments(),
            ["--print", "--dangerously-skip-permissions"]
        );
    }

    #[test]
    fn permissions_can_be_configured_externally() {
        let agent = build(&AgentConfig {
            dangerously_skip_permissions: Some(false),
            ..Default::default()
        });
        assert_eq!(agent.arguments(), ["--print"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn passes_prompt_as_argument_and_captures_reply() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-agy.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n[ \"$1\" = --print ] || exit 1\n[ \"$2\" = --dangerously-skip-permissions ] || exit 2\ncase \"$3\" in *PING*) printf 'agy reply' ;; *) exit 3 ;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let agent = build(&AgentConfig {
            command: Some(script.display().to_string()),
            ..Default::default()
        });
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "PING".into(),
        };
        let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.summary, "agy reply");
    }
}
