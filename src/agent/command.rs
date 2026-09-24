//! Generic subprocess-backed agent adapter.
//!
//! Every concrete adapter is a thin configuration of [`CommandAgent`]: a
//! program, some arguments, and how the prompt is delivered. This keeps adding
//! a new CLI a matter of a few lines.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::{Agent, AgentContext, AgentOutcome, AgentRequest};
use crate::config::PromptDelivery;
use crate::error::{BotError, Result};
use crate::forge::ReplyTarget;

/// Maximum number of characters of captured output kept in the summary.
const OUTPUT_LIMIT: usize = 4000;

/// An [`Agent`] implemented by spawning a child process.
#[derive(Debug, Clone)]
pub struct CommandAgent {
    name: String,
    program: String,
    args: Vec<String>,
    prompt: PromptDelivery,
    timeout: Option<Duration>,
    env: BTreeMap<String, String>,
    dangerously_skip_permissions: bool,
}

impl CommandAgent {
    pub fn new(name: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            prompt: PromptDelivery::Stdin,
            timeout: None,
            env: BTreeMap::new(),
            dangerously_skip_permissions: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Replace the program that is executed.
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn prompt(mut self, delivery: PromptDelivery) -> Self {
        self.prompt = delivery;
        self
    }

    /// Set a wall-clock limit for the command. Without this the agent runs
    /// until its process exits.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env
            .extend(envs.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    pub fn dangerously_skip_permissions(mut self, yes: bool) -> Self {
        self.dangerously_skip_permissions = yes;
        self
    }

    pub fn dangerously_skip_permissions_enabled(&self) -> bool {
        self.dangerously_skip_permissions
    }

    /// Apply a partial user override, keeping defaults for unset fields.
    pub fn apply_config(mut self, config: &crate::config::AgentConfig) -> Self {
        if let Some(command) = &config.command {
            self.program = command.clone();
        }
        if let Some(args) = &config.args {
            self.args = args.clone();
        }
        if let Some(prompt) = config.prompt {
            self.prompt = prompt;
        }
        if let Some(timeout) = config.timeout_secs {
            // 0 disables the wall-clock limit entirely.
            self.timeout = (timeout != 0).then(|| Duration::from_secs(timeout));
        }
        if let Some(dangerous) = config.dangerously_skip_permissions {
            self.dangerously_skip_permissions = dangerous;
        }
        self.env.extend(config.env.clone());
        self
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// Arguments the adapter will pass to the program (including defaults and
    /// any applied overrides).
    pub fn arguments(&self) -> &[String] {
        &self.args
    }

    /// Build the prompt handed to the agent.
    fn prompt_text(&self, request: &AgentRequest, context: &AgentContext) -> String {
        let mut prompt = String::new();
        prompt.push_str("You are an autonomous coding agent invoked from a forge comment.\n\n");

        if let Some(forge) = context.forge {
            prompt.push_str(&format!("Forge: {forge}\n"));
        }
        prompt.push_str(&format!("Location: {}\n", request.location));
        if !context.repository.is_empty() {
            prompt.push_str(&format!("Repository: {}\n", context.repository));
        }
        if let Some(title) = &context.title {
            prompt.push_str(&format!("Title: {title}\n"));
        }
        prompt.push_str(&format!(
            "Your working directory: {}\n",
            context.workspace.display()
        ));
        prompt.push('\n');
        prompt.push_str("Requested work:\n");
        prompt.push_str(request.message.trim());
        prompt.push('\n');
        if let ReplyTarget::ReviewComment(target) = &context.reply_target {
            prompt.push_str(&format!(
                "\nThis mention is an inline pull-request review comment. Post any reply in \
                 the same review thread (review id {}, file `{}`, line {}) instead of \
                 opening a new top-level comment.\n",
                target.review_id, target.path, target.line
            ));
        }
        prompt.push_str(
            "\nUse the tools available to you (forge CLI/API, git, shell, filesystem) to \
             gather context, make changes, run tests, and commit/push when appropriate. \
             Reply on the forge when you are done. Forge credentials are available in \
             the environment.\n",
        );

        prompt
    }
}

#[async_trait::async_trait]
impl Agent for CommandAgent {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome> {
        let workspace: PathBuf = context.workspace.clone();
        if !workspace.as_os_str().is_empty() {
            tokio::fs::create_dir_all(&workspace).await?;
        }

        let prompt = self.prompt_text(request, context);
        let started = Instant::now();

        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .envs(self.env.clone())
            .envs(context.environment(request))
            .stdin(if self.prompt == PromptDelivery::Stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if !workspace.as_os_str().is_empty() {
            cmd.current_dir(&workspace);
        }

        if self.prompt == PromptDelivery::Arg {
            cmd.arg(&prompt);
        }

        tracing::info!(
            agent = %self.name,
            program = %self.program,
            workspace = %workspace.display(),
            "starting agent"
        );

        let program = self.program.clone();
        let name = self.name.clone();
        let prompt_for_spawn = prompt.clone();

        let run = async move {
            let mut child = cmd.spawn().map_err(|e| BotError::Agent {
                name: name.clone(),
                reason: format!("failed to spawn `{program}`: {e}"),
            })?;

            if let Some(mut stdin) = child.stdin.take() {
                if let Err(error) = stdin.write_all(prompt_for_spawn.as_bytes()).await {
                    // A one-shot command may exit before reading its prompt,
                    // which surfaces as a broken pipe. That is not itself a
                    // failure: the child's exit status and output are what
                    // matter, so carry on and let `wait_with_output` decide.
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) {
                        return Err(BotError::Agent {
                            name: name.clone(),
                            reason: format!("failed to write prompt to stdin: {error}"),
                        });
                    }
                }
                // Dropping stdin signals EOF to the child.
                drop(stdin);
            }

            child.wait_with_output().await.map_err(|e| BotError::Agent {
                name: name.clone(),
                reason: format!("failed while waiting for `{program}`: {e}"),
            })
        };

        let result = match self.timeout {
            Some(timeout) => match tokio::time::timeout(timeout, run).await {
                Err(_) => {
                    return Ok(AgentOutcome::failure(
                        format!("agent timed out after {timeout:?}"),
                        started.elapsed(),
                    ));
                }
                Ok(result) => result,
            },
            None => run.await,
        };

        match result {
            Err(err) => Err(err),
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let summary = summarize(&stdout, &stderr);

                if output.status.success() {
                    Ok(AgentOutcome::success(summary, started.elapsed()))
                } else {
                    Ok(AgentOutcome::failure(
                        format!(
                            "agent exited with {}: {}",
                            output
                                .status
                                .code()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "signal".into()),
                            summary
                        ),
                        started.elapsed(),
                    ))
                }
            }
        }
    }
}

/// Keep the tail of the output, preferring stdout over stderr.
fn summarize(stdout: &str, stderr: &str) -> String {
    let trimmed = stdout.trim();
    let source = if trimmed.is_empty() {
        stderr.trim()
    } else {
        trimmed
    };
    if source.len() <= OUTPUT_LIMIT {
        return source.to_owned();
    }
    let start = source.len() - OUTPUT_LIMIT;
    // Do not split a UTF-8 code point.
    let start = source
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= start)
        .unwrap_or(source.len());
    format!("…{}", &source[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_prefers_stdout() {
        assert_eq!(summarize("hello", "warn"), "hello");
        assert_eq!(summarize("", "warn"), "warn");
    }

    #[test]
    fn summarize_truncates_tail() {
        let long = "a".repeat(OUTPUT_LIMIT + 100);
        let out = summarize(&long, "");
        assert!(out.starts_with('…'));
        assert_eq!(out.chars().count(), OUTPUT_LIMIT + 1);
    }

    #[test]
    fn prompt_points_review_mentions_at_their_thread() {
        let agent = CommandAgent::new("echoer", "cat");
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/pulls/22#issuecomment-9039")
                .unwrap(),
            message: "why?".into(),
        };
        let mut context = AgentContext {
            repository: "o/r".into(),
            issue_number: Some(22),
            is_pull_request: true,
            ..Default::default()
        };

        // A normal conversation mention keeps the existing instructions.
        let conversation = agent.prompt_text(&request, &context);
        assert!(!conversation.contains("inline pull-request review comment"));

        context.reply_target = ReplyTarget::ReviewComment(crate::forge::ReviewCommentTarget {
            review_id: 103,
            path: "src/agent/registry.rs".into(),
            line: -12,
            extra_lines_count: 0,
        });
        let review = agent.prompt_text(&request, &context);
        assert!(review.contains("inline pull-request review comment"));
        assert!(review.contains("review id 103"));
        assert!(review.contains("src/agent/registry.rs"));
        assert!(review.contains("-12"));
        assert!(review.contains("instead of opening a new top-level comment"));
    }

    #[tokio::test]
    async fn runs_echo_with_stdin_prompt() {
        // `cat` ignores arguments and echoes stdin, standing in for an agent
        // that reads its prompt from stdin.
        let agent = CommandAgent::new("echoer", "cat").timeout(Duration::from_secs(5));
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "PING".into(),
        };
        let ctx = AgentContext::default();
        let outcome = agent.run(&request, &ctx).await.unwrap();
        // The prompt contains the message; cat echoes it back.
        assert!(outcome.success);
        assert!(outcome.summary.contains("PING"));
    }

    #[tokio::test]
    async fn missing_program_is_reported() {
        let agent = CommandAgent::new("nope", "definitely-not-a-real-binary-xyz")
            .timeout(Duration::from_secs(5));
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "x".into(),
        };
        let err = agent
            .run(&request, &AgentContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, BotError::Agent { .. }));
    }

    #[tokio::test]
    async fn config_timeout_zero_disables_the_limit() {
        let config = crate::config::AgentConfig {
            timeout_secs: Some(0),
            ..Default::default()
        };
        let agent = CommandAgent::new("sleeper", "sh")
            .arg("-c")
            .arg("sleep 1; printf done")
            .apply_config(&config);
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "x".into(),
        };
        let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
        assert!(outcome.success);
        assert!(outcome.summary.contains("done"));
    }
}
