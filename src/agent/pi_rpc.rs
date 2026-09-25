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

use crate::agent::session::SessionStore;
use crate::agent::{Agent, AgentContext, AgentOutcome, AgentRequest, conversation_key};
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

/// Arguments for a `pi --mode rpc` process.
///
/// `session_id` is only used when sessions are persisted (`no_session =
/// false`), so an evicted process resumes its conversation from disk. With
/// `no_session = true` the process is explicitly ephemeral.
fn rpc_arguments(config: &PiRpcConfig, session_id: Option<&str>) -> Vec<String> {
    let mut args = vec!["--mode".to_owned(), "rpc".to_owned()];
    if config.approve {
        args.push("--approve".to_owned());
    }
    if config.no_session {
        args.push("--no-session".to_owned());
    } else if let Some(session_id) = session_id {
        args.push("--session-id".to_owned());
        args.push(session_id.to_owned());
    }
    if let Some(model) = &config.model {
        args.push("--model".to_owned());
        args.push(model.clone());
    }
    if let Some(provider) = &config.provider {
        args.push("--provider".to_owned());
        args.push(provider.clone());
    }
    args.extend(config.args.iter().cloned());
    args
}

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
    ///
    /// When `session_id` is set (and sessions are persisted) the process is
    /// started with `--session-id <id>`, so an evicted or restarted agent can
    /// resume the same on-disk conversation instead of cold-starting.
    pub fn spawn(
        config: &PiRpcConfig,
        workspace: &Path,
        credentials: &[(String, String)],
        session_id: Option<&str>,
    ) -> Result<Self> {
        let mut cmd = Command::new(&config.command);
        cmd.args(rpc_arguments(config, session_id));
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

    async fn next_record_before(&mut self, deadline: Option<Instant>) -> Result<Value> {
        let record = match deadline {
            Some(deadline) => {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or_else(|| self.timeout_error())?;
                match tokio::time::timeout(remaining, self.next_record()).await {
                    Err(_) => return Err(self.timeout_error()),
                    Ok(result) => result?,
                }
            }
            None => self.next_record().await?,
        };
        record.ok_or_else(|| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "pi exited before the run settled".into(),
        })
    }

    fn timeout_error(&self) -> BotError {
        BotError::Agent {
            name: "pi-rpc".into(),
            reason: format!("pi timed out (workspace {})", self.workspace.display()),
        }
    }

    /// Send a prompt and wait until `agent_settled`, returning the assistant's
    /// final text. When `timeout` is `None` the wait is unbounded: the call
    /// only returns once the agent settles or its process exits.
    pub async fn prompt(&mut self, message: &str, timeout: Option<Duration>) -> Result<String> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
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

    async fn get_last_assistant_text(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<Option<String>> {
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
    /// Conversation (issue / pull request) key -> last agent id. Prefer that
    /// agent when idle, while allowing another one to handle a busy thread.
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
    /// Conversation -> backend session id, so evicted processes resume.
    sessions: Arc<SessionStore>,
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
        let deadline = (self.config.timeout_secs != 0)
            .then(|| Instant::now() + Duration::from_secs(self.config.timeout_secs));

        loop {
            // Register before inspecting the pool so a release between the
            // inspection and the wait cannot be missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().expect("pi pool mutex poisoned");
                self.reap(&mut state);

                let preferred = state.conversations.get(key).copied();
                let idle = state
                    .agents
                    .iter()
                    .position(|entry| {
                        Some(entry.id) == preferred
                            && entry.workspace == workspace
                            && !entry.busy
                            && entry.client.is_some()
                    })
                    .or_else(|| {
                        state.agents.iter().position(|entry| {
                            entry.workspace == workspace && !entry.busy && entry.client.is_some()
                        })
                    });
                if let Some(index) = idle {
                    let entry = &mut state.agents[index];
                    entry.busy = true;
                    entry.key = key.to_owned();
                    let id = entry.id;
                    let pid = entry.pid;
                    let client = entry.client.take();
                    state.conversations.insert(key.to_owned(), id);
                    tracing::debug!(key, pid = ?pid, "reusing idle pi agent");
                    return Ok(PoolGuard {
                        inner: Arc::clone(self),
                        id,
                        client,
                    });
                }

                // A live process cannot change its working directory. Make
                // room for this workspace when the pool is full of idle
                // processes belonging to other workspaces.
                if state.agents.len() >= self.config.max_agents
                    && let Some(index) = state.agents.iter().position(|entry| !entry.busy)
                {
                    let mut evicted = state.agents.remove(index);
                    if let Some(client) = evicted.client.as_mut() {
                        client.kill();
                    }
                    state.conversations.retain(|_, id| *id != evicted.id);
                }

                if state.agents.len() < self.config.max_agents {
                    let id = Uuid::new_v4();
                    let session_id = (!self.config.no_session)
                        .then(|| self.sessions.deterministic_id("pi-rpc", key));
                    let client = PiRpcClient::spawn(
                        &self.config,
                        workspace,
                        credentials,
                        session_id.as_deref(),
                    )?;
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

            match deadline {
                Some(deadline) => {
                    let remaining =
                        deadline
                            .checked_duration_since(Instant::now())
                            .ok_or_else(|| BotError::Agent {
                                name: "pi-rpc".into(),
                                reason: "timed out waiting for an idle pi agent".into(),
                            })?;
                    if tokio::time::timeout(remaining, notified).await.is_err() {
                        return Err(BotError::Agent {
                            name: "pi-rpc".into(),
                            reason: "timed out waiting for an idle pi agent".into(),
                        });
                    }
                }
                None => notified.await,
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
    pub fn new(config: &PiRpcConfig, sessions: Arc<SessionStore>) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                config: config.clone(),
                sessions,
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
        let timeout = (self.inner.config.timeout_secs != 0)
            .then(|| Duration::from_secs(self.inner.config.timeout_secs));

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
    use crate::forge::IssueRef;
    use crate::location::ForgeKind;

    fn cfg() -> PiRpcConfig {
        PiRpcConfig {
            command: "cat".into(),
            max_agents: 1,
            timeout_secs: 5,
            ..Default::default()
        }
    }

    fn store() -> Arc<SessionStore> {
        Arc::new(SessionStore::default())
    }

    #[test]
    fn rpc_arguments_persist_a_session_id_by_default() {
        let config = PiRpcConfig {
            approve: true,
            no_session: false,
            model: Some("deepseek-flash".into()),
            provider: Some("deepseek".into()),
            ..Default::default()
        };
        let args = rpc_arguments(&config, Some("session-123"));
        assert_eq!(&args[..2], ["--mode", "rpc"]);
        assert!(args.contains(&"--approve".to_owned()));
        assert!(args.contains(&"--session-id".to_owned()));
        assert!(args.contains(&"session-123".to_owned()));
        assert!(!args.contains(&"--no-session".to_owned()));
    }

    #[test]
    fn rpc_arguments_are_ephemeral_when_sessions_are_disabled() {
        let config = PiRpcConfig {
            no_session: true,
            ..Default::default()
        };
        let args = rpc_arguments(&config, Some("session-123"));
        assert!(args.contains(&"--no-session".to_owned()));
        assert!(!args.iter().any(|arg| arg == "--session-id"));
    }

    #[test]
    fn session_ids_are_deterministic_per_conversation() {
        let agent = PiPoolAgent::new(&cfg(), store());
        let first = agent
            .inner
            .sessions
            .deterministic_id("pi-rpc", "forgejo:o/r:1");
        let again = agent
            .inner
            .sessions
            .deterministic_id("pi-rpc", "forgejo:o/r:1");
        let other = agent
            .inner
            .sessions
            .deterministic_id("pi-rpc", "forgejo:o/r:2");
        assert_eq!(first, again);
        assert_ne!(first, other);
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
        let agent = PiPoolAgent::new(&cfg(), store());
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
        let agent = PiPoolAgent::new(&cfg(), store());
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
    async fn busy_thread_spawns_another_agent_then_reuses_idle_one() {
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
        let agent = Arc::new(PiPoolAgent::new(&config, store()));

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
        // Wait for the first request to spawn and bind its agent. Poll
        // instead of sleeping a fixed amount, so the test is not
        // timing-sensitive when the suite runs under load.
        let mut bound = None;
        for _ in 0..500 {
            if let Some(id) = agent.conversation_binding(&key) {
                bound = Some(id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let bound = bound.expect("agent should be bound to the thread");
        assert_eq!(agent.live_agents(), 1);

        // The bound agent is busy, so the second request starts another one.
        let second = spawn(Arc::clone(&agent));
        for _ in 0..500 {
            if agent.live_agents() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(agent.live_agents(), 2);
        assert_ne!(agent.conversation_binding(&key), Some(bound));

        assert!(first.await.unwrap().unwrap().success);
        assert!(second.await.unwrap().unwrap().success);
        assert_eq!(agent.live_agents(), 2);

        // A different thread in the same workspace uses a free process.
        let mut other_context = context.clone();
        other_context.issue_number = Some(2);
        let outcome = agent.run(&request, &other_context).await.unwrap();
        assert!(outcome.success);
        assert_eq!(agent.live_agents(), 2);
        assert!(
            agent
                .conversation_binding(&conversation_key(&other_context))
                .is_some()
        );
    }

    #[tokio::test]
    async fn waits_only_when_pool_is_full() {
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
            max_agents: 1,
            timeout_secs: 10,
            ..Default::default()
        };
        config.env.insert("FAKE_PI_DELAY".into(), "1".into());
        let agent = Arc::new(PiPoolAgent::new(&config, store()));
        let workspace = dir.path();
        let first = agent.inner.acquire("first", workspace, &[]).await.unwrap();

        let waiting_agent = Arc::clone(&agent);
        let waiting_workspace = workspace.to_path_buf();
        let waiter = tokio::spawn(async move {
            waiting_agent
                .inner
                .acquire("second", &waiting_workspace, &[])
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        assert_eq!(agent.live_agents(), 1);

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(second.id, agent.conversation_binding("first").unwrap());
        assert_eq!(agent.live_agents(), 1);
    }

    #[tokio::test]
    async fn reuses_idle_agent_before_spawning_for_busy_thread() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_pi.py");
        std::fs::write(&script, FAKE_PI).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config = PiRpcConfig {
            command: script.display().to_string(),
            max_agents: 3,
            ..Default::default()
        };
        let agent = PiPoolAgent::new(&config, store());
        let workspace = dir.path();
        let bound = agent.inner.acquire("thread", workspace, &[]).await.unwrap();
        let other = agent.inner.acquire("other", workspace, &[]).await.unwrap();
        let other_id = other.id;
        drop(other);

        let reused = agent.inner.acquire("thread", workspace, &[]).await.unwrap();
        assert_eq!(reused.id, other_id);
        assert_eq!(agent.live_agents(), 2);
        assert_eq!(agent.conversation_binding("thread"), Some(other_id));
        drop(bound);
    }

    #[tokio::test]
    async fn replaces_idle_agent_from_another_workspace_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_pi.py");
        std::fs::write(&script, FAKE_PI).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config = PiRpcConfig {
            command: script.display().to_string(),
            max_agents: 1,
            ..Default::default()
        };
        let agent = PiPoolAgent::new(&config, store());
        let first_workspace = dir.path();
        let second_workspace = dir.path().join("second");
        std::fs::create_dir(&second_workspace).unwrap();
        let first = agent
            .inner
            .acquire("first", first_workspace, &[])
            .await
            .unwrap();
        let first_id = first.id;
        drop(first);

        let second = agent
            .inner
            .acquire("second", &second_workspace, &[])
            .await
            .unwrap();
        assert_ne!(second.id, first_id);
        assert_eq!(agent.live_agents(), 1);
        assert_eq!(agent.conversation_binding("first"), None);
    }

    #[tokio::test]
    async fn disabled_timeout_lets_a_slow_agent_finish() {
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
            max_agents: 1,
            timeout_secs: 0,
            ..Default::default()
        };
        config.env.insert("FAKE_PI_DELAY".into(), "1".into());
        let agent = PiPoolAgent::new(&config, store());

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

        // With the limit disabled the run has no deadline and must wait for
        // the fake agent to settle instead of failing.
        let outcome = agent.run(&request, &context).await.unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.summary, "fake-result");
    }

    #[test]
    fn conversation_key_folds_pr_onto_linked_issue() {
        let pr = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(12),
            is_pull_request: true,
            linked_issue: Some(IssueRef {
                repository: None,
                number: 5,
            }),
            ..Default::default()
        };
        assert_eq!(conversation_key(&pr), "forgejo:o/r:5");

        let unlinked = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(12),
            is_pull_request: true,
            linked_issue: None,
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

        // A cross-repository link keeps the linked issue's owner/repo, so the
        // PR and the issue it closes share one key.
        let cross_repo = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(12),
            is_pull_request: true,
            linked_issue: Some(IssueRef {
                repository: Some("other/repo".into()),
                number: 5,
            }),
            ..Default::default()
        };
        assert_eq!(conversation_key(&cross_repo), "forgejo:other/repo:5");
    }
}
