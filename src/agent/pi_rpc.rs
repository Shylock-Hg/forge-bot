//! Long-lived Pi RPC agent pool.
//!
//! `pi --mode rpc` is a persistent JSONL-controlled process. Instead of
//! spawning a fresh `pi` per request, this adapter keeps a small pool of them:
//! an incoming request reuses an idle agent for the same workspace, and a new
//! agent is spawned when none is available. Idle agents are evicted after a
//! configurable TTL.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::agent::{Agent, AgentContext, AgentOutcome, AgentRequest};
use crate::config::PiRpcConfig;
use crate::error::{BotError, Result};

/// Environment variables from the caller (an AaaU session, an interactive
/// shell, ...) that must not leak into a managed agent.
const SCRUBBED_ENV: &[&str] = &[
    "AAAU_SESSION_ID",
    "AAAU_EDITOR_SOCKET",
    "TERM_PROGRAM",
    "EDITOR",
    "VISUAL",
];

/// A single `pi --mode rpc` subprocess.
pub struct PiRpcClient {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    workspace: PathBuf,
}

impl PiRpcClient {
    /// Spawn a new RPC agent in `workspace`.
    pub fn spawn(
        config: &PiRpcConfig,
        workspace: &Path,
        credentials: &[(String, String)],
    ) -> Result<Self> {
        let mut cmd = Command::new(&config.command);
        cmd.arg("--mode").arg("rpc");
        if config.approve {
            cmd.arg("--approve");
        }
        if config.no_session {
            cmd.arg("--no-session");
        }
        if let Some(model) = &config.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(provider) = &config.provider {
            cmd.arg("--provider").arg(provider);
        }
        cmd.args(&config.args);
        cmd.envs(config.env.clone());
        cmd.envs(credentials.iter().cloned());
        for key in SCRUBBED_ENV {
            cmd.env_remove(key);
        }

        cmd.current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|error| BotError::Agent {
            name: "pi-rpc".into(),
            reason: format!("failed to spawn `{}`: {error}", config.command),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "pi stdin was not captured".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "pi stdout was not captured".into(),
        })?;

        tracing::debug!(workspace = %workspace.display(), "spawned pi rpc agent");

        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            workspace: workspace.to_path_buf(),
        })
    }

    /// Whether the child is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Terminate the child.
    pub fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    fn next_request_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        format!("req-{id}")
    }

    async fn send(&mut self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read the next JSONL record, skipping malformed lines.
    async fn next_record(&mut self) -> Result<Option<Value>> {
        loop {
            let Some(line) = self.lines.next_line().await? else {
                return Ok(None);
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(value) => return Ok(Some(value)),
                Err(error) => {
                    tracing::debug!(%error, "ignoring non-JSON line from pi");
                }
            }
        }
    }

    async fn next_record_before(&mut self, deadline: Instant) -> Result<Value> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| self.timeout_error())?;
        match tokio::time::timeout(remaining, self.next_record()).await {
            Err(_) => Err(self.timeout_error()),
            Ok(Ok(Some(record))) => Ok(record),
            Ok(Ok(None)) => Err(BotError::Agent {
                name: "pi-rpc".into(),
                reason: "pi exited before the run settled".into(),
            }),
            Ok(Err(error)) => Err(error),
        }
    }

    fn timeout_error(&self) -> BotError {
        BotError::Agent {
            name: "pi-rpc".into(),
            reason: format!("pi timed out (workspace {})", self.workspace.display()),
        }
    }

    /// Send a prompt and wait until `agent_settled`, returning the assistant's
    /// final text.
    pub async fn prompt(&mut self, message: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let request_id = self.next_request_id();
        self.send(&json!({
            "id": request_id,
            "type": "prompt",
            "message": message,
        }))
        .await?;

        let mut streamed = String::new();
        loop {
            let record = self.next_record_before(deadline).await?;
            match record["type"].as_str().unwrap_or_default() {
                "response" => {
                    if record["id"].as_str() == Some(request_id.as_str())
                        && record["success"].as_bool() == Some(false)
                    {
                        let error = record["error"]
                            .as_str()
                            .unwrap_or("pi rejected the prompt")
                            .to_owned();
                        return Err(BotError::Agent {
                            name: "pi-rpc".into(),
                            reason: error,
                        });
                    }
                }
                "message_update" => {
                    if let Some(event) = record.get("assistantMessageEvent")
                        && event["type"] == "text_delta"
                        && let Some(delta) = event["delta"].as_str()
                    {
                        streamed.push_str(delta);
                    }
                }
                "agent_settled" => break,
                _ => {}
            }
        }

        // Prefer pi's authoritative final message; fall back to the stream.
        match self.get_last_assistant_text(deadline).await {
            Ok(Some(text)) if !text.trim().is_empty() => Ok(text),
            _ => Ok(streamed),
        }
    }

    async fn get_last_assistant_text(&mut self, deadline: Instant) -> Result<Option<String>> {
        let request_id = self.next_request_id();
        self.send(&json!({
            "id": request_id,
            "type": "get_last_assistant_text",
        }))
        .await?;

        loop {
            let record = self.next_record_before(deadline).await?;
            if record["type"] == "response" && record["id"].as_str() == Some(request_id.as_str()) {
                return Ok(record["data"]["text"].as_str().map(str::to_owned));
            }
        }
    }
}

/// State of the pool.
#[derive(Default)]
struct PoolState {
    agents: Vec<PoolEntry>,
}

struct PoolEntry {
    id: Uuid,
    key: String,
    workspace: PathBuf,
    client: Option<PiRpcClient>,
    busy: bool,
    last_used: Instant,
}

/// Shared pool internals.
struct PoolInner {
    config: PiRpcConfig,
    state: Mutex<PoolState>,
    notify: Notify,
}

impl PoolInner {
    /// Drop dead or expired idle agents. Callers hold the state lock.
    fn reap(&self, state: &mut PoolState) {
        let ttl = Duration::from_secs(self.config.idle_ttl_secs.max(1));
        state.agents.retain_mut(|entry| {
            if entry.busy {
                return true;
            }

            // Reap an idle agent that already exited (`try_wait` on liveness).
            let alive = entry
                .client
                .as_mut()
                .map(PiRpcClient::is_alive)
                .unwrap_or(false);
            if !alive {
                entry.client = None;
                return false;
            }

            if entry.last_used.elapsed() >= ttl {
                tracing::info!(
                    key = %entry.key,
                    workspace = %entry.workspace.display(),
                    "evicting idle pi agent"
                );
                if let Some(client) = entry.client.as_mut() {
                    client.kill();
                }
                entry.client = None;
                return false;
            }
            true
        });
    }

    /// Check out a client for `key`, spawning one if necessary.
    async fn acquire(
        self: &Arc<Self>,
        key: &str,
        workspace: &Path,
        credentials: &[(String, String)],
    ) -> Result<PoolGuard> {
        let deadline = Instant::now() + Duration::from_secs(self.config.timeout_secs.max(60));

        loop {
            {
                let mut state = self.state.lock().expect("pi pool mutex poisoned");
                self.reap(&mut state);

                if let Some(entry) = state
                    .agents
                    .iter_mut()
                    .find(|entry| entry.key == key && !entry.busy && entry.client.is_some())
                {
                    entry.busy = true;
                    let id = entry.id;
                    let client = entry.client.take();
                    tracing::debug!(key, "reusing idle pi agent");
                    return Ok(PoolGuard {
                        inner: Arc::clone(self),
                        id,
                        client,
                    });
                }

                if state.agents.len() < self.config.max_agents {
                    let id = Uuid::new_v4();
                    let client = PiRpcClient::spawn(&self.config, workspace, credentials)?;
                    state.agents.push(PoolEntry {
                        id,
                        key: key.to_owned(),
                        workspace: workspace.to_path_buf(),
                        client: None,
                        busy: true,
                        last_used: Instant::now(),
                    });
                    tracing::info!(key, max = self.config.max_agents, "spawned pi agent");
                    return Ok(PoolGuard {
                        inner: Arc::clone(self),
                        id,
                        client: Some(client),
                    });
                }
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| BotError::Agent {
                    name: "pi-rpc".into(),
                    reason: "timed out waiting for an idle pi agent".into(),
                })?;
            if tokio::time::timeout(remaining, self.notify.notified())
                .await
                .is_err()
            {
                return Err(BotError::Agent {
                    name: "pi-rpc".into(),
                    reason: "timed out waiting for an idle pi agent".into(),
                });
            }
        }
    }
}

/// A checked-out agent. Returning it to the pool happens on drop.
pub struct PoolGuard {
    inner: Arc<PoolInner>,
    id: Uuid,
    client: Option<PiRpcClient>,
}

impl PoolGuard {
    fn client_mut(&mut self) -> Result<&mut PiRpcClient> {
        self.client.as_mut().ok_or_else(|| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "agent was invalidated".into(),
        })
    }

    /// Drop the underlying client instead of returning it to the pool.
    fn invalidate(&mut self) {
        if let Some(mut client) = self.client.take() {
            client.kill();
        }
    }
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        let client = self.client.take();
        {
            let mut state = self.inner.state.lock().expect("pi pool mutex poisoned");
            if let Some(entry) = state.agents.iter_mut().find(|entry| entry.id == self.id) {
                entry.last_used = Instant::now();
                entry.busy = false;

                let kept = match client {
                    Some(client) => {
                        let mut client = client;
                        if client.is_alive() {
                            entry.client = Some(client);
                            true
                        } else {
                            entry.client = None;
                            false
                        }
                    }
                    None => {
                        entry.client = None;
                        false
                    }
                };
                if !kept {
                    state.agents.retain(|entry| entry.id != self.id);
                }
            }
        }
        self.inner.notify.notify_waiters();
    }
}

/// Pooled Pi RPC adapter.
pub struct PiPoolAgent {
    inner: Arc<PoolInner>,
}

impl PiPoolAgent {
    pub fn new(config: &PiRpcConfig) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                config: config.clone(),
                state: Mutex::new(PoolState::default()),
                notify: Notify::new(),
            }),
        }
    }

    /// Number of live agents (mainly useful for tests/diagnostics).
    pub fn live_agents(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("pi pool mutex poisoned")
            .agents
            .len()
    }
}

#[async_trait::async_trait]
impl Agent for PiPoolAgent {
    fn name(&self) -> &str {
        "pi-rpc"
    }

    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome> {
        let started = Instant::now();
        let key = if context.repository.is_empty() {
            context.workspace.to_string_lossy().into_owned()
        } else {
            format!(
                "{}#{}",
                context.repository,
                context.issue_number.unwrap_or_default()
            )
        };

        let mut guard = self
            .inner
            .acquire(&key, &context.workspace, &context.credentials)
            .await?;
        let prompt = build_prompt(request, context);
        let timeout = Duration::from_secs(self.inner.config.timeout_secs.max(1));

        match guard.client_mut()?.prompt(&prompt, timeout).await {
            Ok(text) => Ok(AgentOutcome::success(text, started.elapsed())),
            Err(error) => {
                guard.invalidate();
                Ok(AgentOutcome::failure(error.to_string(), started.elapsed()))
            }
        }
    }
}

/// Build the prompt sent to a pooled pi agent.
fn build_prompt(request: &AgentRequest, context: &AgentContext) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are the coding agent responding to a forge comment.\n");
    prompt.push_str("Work in the current directory (the repository checkout).\n\n");
    prompt.push_str(&format!("Location: {}\n", request.location));
    if !context.repository.is_empty() {
        prompt.push_str(&format!("Repository: {}\n", context.repository));
    }
    if let Some(title) = &context.title {
        prompt.push_str(&format!("Title: {title}\n"));
    }
    prompt.push_str(&format!(
        "Working directory: {}\n\n",
        context.workspace.display()
    ));
    prompt.push_str("Requested work:\n");
    prompt.push_str(request.message.trim());
    prompt.push('\n');
    prompt.push_str(
        "\nWhen finished, reply with a concise summary of what you did and any \
         findings. Do not post to the forge yourself; the gateway relays your \
         final message as the comment reply.\n",
    );
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PiRpcConfig {
        PiRpcConfig {
            command: "cat".into(),
            max_agents: 1,
            timeout_secs: 5,
            ..Default::default()
        }
    }

    #[test]
    fn prompt_includes_request() {
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "fix the bug".into(),
        };
        let context = AgentContext {
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        };
        let prompt = build_prompt(&request, &context);
        assert!(prompt.contains("fix the bug"));
        assert!(prompt.contains("o/r"));
        assert!(prompt.contains("Do not post to the forge"));
    }

    #[test]
    fn reaps_dead_idle_agents_but_keeps_busy_ones() {
        let agent = PiPoolAgent::new(&cfg());
        let mut state = agent.inner.state.lock().unwrap();
        state.agents.push(PoolEntry {
            id: Uuid::new_v4(),
            key: "dead".into(),
            workspace: PathBuf::from("/tmp"),
            client: None,
            busy: false,
            last_used: Instant::now(),
        });
        state.agents.push(PoolEntry {
            id: Uuid::new_v4(),
            key: "busy".into(),
            workspace: PathBuf::from("/tmp"),
            client: None,
            busy: true,
            last_used: Instant::now(),
        });
        agent.inner.reap(&mut state);
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].key, "busy");
    }
}
