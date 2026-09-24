//! Long-lived Pi RPC agent pool.
//!
//! `pi --mode rpc` is a persistent JSONL-controlled process. Instead of
//! spawning a fresh `pi` per request, this adapter keeps a small pool of them:
//! an incoming request reuses an idle agent for the same workspace, and a new
//! agent is spawned when none is available. Idle agents are evicted after a
//! configurable TTL.

use std::collections::{HashMap, HashSet};
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

        if !workspace.as_os_str().is_empty() {
            cmd.current_dir(workspace);
        }
        cmd.stdin(Stdio::piped())
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

    /// Process id of the child, while it is alive.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
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
    /// Conversation (issue / pull request) key -> bound agent id. Recorded so
    /// every fragment of one thread is routed to the same agent instance, and
    /// reaped together with the agent it points at.
    conversations: HashMap<String, Uuid>,
}

struct PoolEntry {
    id: Uuid,
    key: String,
    pid: Option<u32>,
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
    /// Drop dead or expired idle agents, and any conversation mapping whose
    /// agent no longer exists. Callers hold the state lock.
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
                    pid = ?entry.pid,
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

        // A mapping is stale once its agent is gone. Dropping it lets the next
        // mention in that thread start a fresh agent instead of waiting for a
        // process that will never return.
        let live: HashSet<Uuid> = state.agents.iter().map(|entry| entry.id).collect();
        let before = state.conversations.len();
        state.conversations.retain(|_, id| live.contains(id));
        let evicted = before - state.conversations.len();
        if evicted > 0 {
            tracing::debug!(evicted, "evicted stale conversation mappings");
        }
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

                // Conversation affinity: a thread that is already bound to an
                // agent keeps using that agent, even if it is momentarily
                // busy. This is what stops two fragments of one issue being
                // handled by different instances.
                if let Some(id) = state.conversations.get(key).copied() {
                    if let Some(entry) = state.agents.iter_mut().find(|entry| entry.id == id) {
                        if !entry.busy && entry.client.is_some() {
                            entry.busy = true;
                            let client = entry.client.take();
                            tracing::debug!(key, pid = ?entry.pid, "reusing bound pi agent");
                            return Ok(PoolGuard {
                                inner: Arc::clone(self),
                                id,
                                client,
                            });
                        }
                        // Busy: wait for it to settle below rather than
                        // spawning a second agent for the same thread.
                    } else {
                        state.conversations.remove(key);
                    }
                } else if state.agents.len() < self.config.max_agents {
                    let id = Uuid::new_v4();
                    let client = PiRpcClient::spawn(&self.config, workspace, credentials)?;
                    let pid = client.pid();
                    state.agents.push(PoolEntry {
                        id,
                        key: key.to_owned(),
                        pid,
                        workspace: workspace.to_path_buf(),
                        client: None,
                        busy: true,
                        last_used: Instant::now(),
                    });
                    state.conversations.insert(key.to_owned(), id);
                    tracing::info!(key, pid = ?pid, max = self.config.max_agents, "spawned pi agent");
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
                    state.conversations.retain(|_, id| *id != self.id);
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

    /// Agent currently bound to a conversation key, if any. Internal data used
    /// to keep a thread on one instance; it is never sent to an agent.
    pub fn conversation_binding(&self, key: &str) -> Option<Uuid> {
        self.inner
            .state
            .lock()
            .expect("pi pool mutex poisoned")
            .conversations
            .get(key)
            .copied()
    }
}

#[async_trait::async_trait]
impl Agent for PiPoolAgent {
    fn name(&self) -> &str {
        "pi-rpc"
    }

    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome> {
        let started = Instant::now();
        let key = conversation_key(context);

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

/// Stable conversation key used to pin one thread to one agent instance.
///
/// A pull request is folded onto the issue it closes when the description
/// references one, so both threads share an agent. This is internal routing
/// data and is deliberately never rendered into the prompt.
fn conversation_key(context: &AgentContext) -> String {
    if context.repository.is_empty() {
        return context.workspace.to_string_lossy().into_owned();
    }
    let number = if context.is_pull_request {
        context.linked_issue_number.or(context.issue_number)
    } else {
        context.issue_number
    };
    match context.forge {
        Some(forge) => format!(
            "{}:{}:{}",
            forge.as_str(),
            context.repository,
            number.unwrap_or_default()
        ),
        None => format!("{}:{}", context.repository, number.unwrap_or_default()),
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
    use crate::location::ForgeKind;

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
            pid: None,
            workspace: PathBuf::from("/tmp"),
            client: None,
            busy: false,
            last_used: Instant::now(),
        });
        state.agents.push(PoolEntry {
            id: Uuid::new_v4(),
            key: "busy".into(),
            pid: None,
            workspace: PathBuf::from("/tmp"),
            client: None,
            busy: true,
            last_used: Instant::now(),
        });
        agent.inner.reap(&mut state);
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].key, "busy");
    }

    #[test]
    fn reaps_stale_conversation_mappings() {
        let agent = PiPoolAgent::new(&cfg());
        let mut state = agent.inner.state.lock().unwrap();
        let kept = Uuid::new_v4();
        state.agents.push(PoolEntry {
            id: kept,
            key: "live".into(),
            pid: None,
            workspace: PathBuf::from("/tmp"),
            client: None,
            busy: true,
            last_used: Instant::now(),
        });
        state.conversations.insert("live".into(), kept);
        state.conversations.insert("stale".into(), Uuid::new_v4());

        agent.inner.reap(&mut state);

        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.conversations.get("live"), Some(&kept));
        assert!(!state.conversations.contains_key("stale"));
    }

    /// Minimal `pi --mode rpc` stand-in: answers a prompt after an optional
    /// delay and serves `get_last_assistant_text`.
    const FAKE_PI: &str = r#"#!/usr/bin/env python3
import json, os, sys, time

delay = float(os.environ.get("FAKE_PI_DELAY", "0"))
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except ValueError:
        continue
    kind = message.get("type")
    request_id = message.get("id")
    if kind == "prompt":
        time.sleep(delay)
        print(json.dumps({"type": "response", "id": request_id, "success": True}), flush=True)
        print(json.dumps({"type": "agent_settled"}), flush=True)
    elif kind == "get_last_assistant_text":
        print(
            json.dumps(
                {
                    "type": "response",
                    "id": request_id,
                    "data": {"text": "fake-result"},
                }
            ),
            flush=True,
        )
"#;

    #[tokio::test]
    async fn same_thread_waits_for_its_agent_instead_of_spawning_another() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_pi.py");
        std::fs::write(&script, FAKE_PI).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut config = PiRpcConfig {
            command: script.display().to_string(),
            max_agents: 4,
            timeout_secs: 10,
            ..Default::default()
        };
        config.env.insert("FAKE_PI_DELAY".into(), "1".into());
        let agent = Arc::new(PiPoolAgent::new(&config));

        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let context = AgentContext {
            workspace: dir.path().to_path_buf(),
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        };
        let key = conversation_key(&context);

        let spawn = |agent: Arc<PiPoolAgent>| {
            let request = request.clone();
            let context = context.clone();
            tokio::spawn(async move { agent.run(&request, &context).await })
        };

        let first = spawn(Arc::clone(&agent));
        // Let the first request spawn and bind its agent.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let bound = agent.conversation_binding(&key).expect("bound agent");
        assert_eq!(agent.live_agents(), 1);

        // The second request is for the same thread. It must wait for the
        // bound agent rather than spawn a second instance.
        let second = spawn(Arc::clone(&agent));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(agent.live_agents(), 1, "no duplicate agent for one thread");
        assert_eq!(agent.conversation_binding(&key), Some(bound));

        assert!(first.await.unwrap().unwrap().success);
        assert!(second.await.unwrap().unwrap().success);
        assert_eq!(agent.live_agents(), 1);
        assert_eq!(agent.conversation_binding(&key), Some(bound));
    }

    #[test]
    fn conversation_key_folds_pr_onto_linked_issue() {
        let pr = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(12),
            is_pull_request: true,
            linked_issue_number: Some(5),
            ..Default::default()
        };
        assert_eq!(conversation_key(&pr), "forgejo:o/r:5");

        let unlinked = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(12),
            is_pull_request: true,
            linked_issue_number: None,
            ..Default::default()
        };
        assert_eq!(conversation_key(&unlinked), "forgejo:o/r:12");

        let issue = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(5),
            ..Default::default()
        };
        assert_eq!(conversation_key(&issue), "forgejo:o/r:5");
    }
}
