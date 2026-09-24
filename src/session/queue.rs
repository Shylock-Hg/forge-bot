//! Bounded job queue and worker pool.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{Semaphore, mpsc};
use uuid::Uuid;

use crate::agent::capacity::is_capacity_limited;
use crate::agent::{AgentContext, AgentOutcome, AgentRegistry, AgentRequest};
use crate::config::Config;
use crate::error::{BotError, Result};
use crate::forge::ForgeMessage;
use crate::forge_api::ForgeApi;
use crate::mention::Mention;
use crate::policy::Policy;
use crate::session::{Job, SessionStore};
use crate::workspace::WorkspaceManager;

/// Routes jobs from webhooks to agents.
pub struct Dispatcher {
    inner: Arc<Inner>,
}

struct Inner {
    config: Arc<Config>,
    agents: Arc<AgentRegistry>,
    sessions: Arc<SessionStore>,
    api: Arc<dyn ForgeApi>,
    workspaces: WorkspaceManager,
    policy: Policy,
    tx: mpsc::Sender<Job>,
}

impl Dispatcher {
    /// Create the dispatcher and start its worker pool.
    pub fn new(
        config: Arc<Config>,
        agents: Arc<AgentRegistry>,
        sessions: Arc<SessionStore>,
        api: Arc<dyn ForgeApi>,
        policy: Policy,
    ) -> Result<Arc<Self>> {
        let capacity = config.session.queue_capacity.max(1);
        let (tx, rx) = mpsc::channel(capacity);

        let inner = Arc::new(Inner {
            workspaces: WorkspaceManager::new(&config.workspace),
            config: config.clone(),
            agents,
            sessions,
            api,
            policy,
            tx,
        });

        // Recover jobs that were queued when the process stopped.
        if config.session.recover {
            match inner.sessions.pending_jobs() {
                Ok(jobs) if !jobs.is_empty() => {
                    tracing::info!(count = jobs.len(), "recovering pending jobs");
                    for job in jobs {
                        if let Err(error) = inner.tx.try_send(job) {
                            tracing::warn!(%error, "could not requeue recovered job");
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "failed to load pending jobs"),
            }
        }

        tokio::spawn(worker_loop(inner.clone(), rx));

        Ok(Arc::new(Self { inner }))
    }

    /// Authorize and enqueue a trigger.
    pub async fn submit(
        &self,
        message: ForgeMessage,
        mention: Mention,
        agent_name: &str,
    ) -> Result<Uuid> {
        self.inner.policy.authorize(&message)?;
        // Resolve eagerly so an unknown agent fails before we persist a job.
        let _ = self.inner.agents.get(agent_name)?;

        let job = Job {
            id: Uuid::new_v4(),
            message,
            mention,
            agent: agent_name.to_owned(),
            created_at: Utc::now(),
        };

        self.inner.sessions.save_job(&job)?;
        self.inner
            .tx
            .send(job.clone())
            .await
            .map_err(|_| BotError::Other(anyhow::anyhow!("job queue is closed")))?;

        // Acknowledge as soon as the job is accepted, before any worker or
        // agent capacity comes into play. Without this a mention in a busy
        // thread can go unanswered until the running agent settles.
        if self.inner.config.reply.ack {
            let ack = format!(
                "🤖 On it — running agent **{}**. I'll report back here when it finishes.",
                job.agent
            );
            if let Err(error) = self
                .inner
                .api
                .post_comment(&job.message.location, &ack)
                .await
            {
                tracing::warn!(%error, "failed to post acknowledgement");
            }
        }

        tracing::info!(job = %job.id, agent = %job.agent, repo = %job.message.repository, "job queued");
        Ok(job.id)
    }

    /// Access to the policy (used by tests and the HTTP layer).
    pub fn policy(&self) -> &Policy {
        &self.inner.policy
    }
}

/// Receive jobs and run them with bounded concurrency.
async fn worker_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<Job>) {
    let concurrency = inner.config.session.workers.max(1);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    tracing::info!(concurrency, "agent worker pool started");

    while let Some(job) = rx.recv().await {
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
        let inner = inner.clone();
        tokio::spawn(async move {
            inner.handle(job).await;
            drop(permit);
        });
    }
}

impl Inner {
    async fn handle(&self, job: Job) {
        let key = job.session_key();

        if let Err(error) = self.sessions.begin(&job) {
            tracing::warn!(%error, "failed to persist session start");
        }

        let credentials = self.config.credentials_for(job.message.forge);
        let workspace = match self.workspaces.prepare(&job.message, &credentials).await {
            Ok(workspace) => workspace,
            Err(error) => {
                self.finish(
                    &key,
                    &job,
                    &job.agent,
                    &permission_aware_failure(&job, &error),
                )
                .await;
                return;
            }
        };

        // Resolve eagerly so an unknown agent fails before we run anything.
        if let Err(error) = self.agents.get(&job.agent) {
            self.finish(
                &key,
                &job,
                &job.agent,
                &AgentOutcome::failure(error.to_string(), Default::default()),
            )
            .await;
            return;
        }

        let request = AgentRequest {
            location: job.message.location.clone(),
            message: job.mention.message_or_default().to_owned(),
        };
        let context = AgentContext {
            workspace,
            forge: Some(job.message.forge),
            repository: job.message.repository.clone(),
            issue_number: job.message.number,
            is_pull_request: job.message.is_pull_request,
            linked_issue: job.message.linked_issue.clone(),
            title: job.message.title.clone(),
            credentials,
        };

        // The requested agent first, then every other available agent. Agents
        // known to be at capacity are skipped entirely.
        let candidates = self.candidate_agents(&job.agent);
        if candidates.is_empty() {
            self.finish_no_agent(&key, &job).await;
            return;
        }

        let mut last_outcome: Option<AgentOutcome> = None;
        let mut used_agent = candidates[0].clone();
        let mut unavailable_hits = 0usize;

        for (index, name) in candidates.iter().enumerate() {
            used_agent = name.clone();

            // The initial "on it" acknowledgement is posted by `submit` as
            // soon as the mention is accepted, so only announce deviations
            // from the requested agent here.
            if index == 0 {
                if self.config.reply.ack && name != &job.agent {
                    let notice = format!(
                        "⚠️ Agent **{}** is at capacity; running agent **{name}** instead.",
                        job.agent
                    );
                    self.reply(&job.message.location, &notice).await;
                }
            } else if self.config.reply.ack {
                let previous = &candidates[index - 1];
                let notice = format!(
                    "⚠️ Agent **{previous}** hit a capacity limit; switching to **{name}**."
                );
                self.reply(&job.message.location, &notice).await;
            }

            let agent = match self.agents.get(name) {
                Ok(agent) => agent,
                Err(error) => {
                    last_outcome =
                        Some(AgentOutcome::failure(error.to_string(), Default::default()));
                    break;
                }
            };

            let cooldown = Duration::from_secs(self.config.capacity.cooldown_secs.max(1));

            let outcome = match agent.run(&request, &context).await {
                Ok(outcome) => outcome,
                Err(error) if error.is_agent_unavailable() => {
                    // The adapter cannot start at all (missing binary, wrong
                    // command, ...). Skip it like a capacity hit so the next
                    // configured agent gets a chance.
                    self.agents.mark_unavailable(name, cooldown);
                    unavailable_hits += 1;
                    tracing::warn!(job = %job.id, agent = %name, %error, "agent cannot be started");
                    last_outcome = Some(permission_aware_failure(&job, &error));
                    continue;
                }
                Err(error) => permission_aware_failure(&job, &error),
            };

            let capacity_limited = !outcome.success
                && is_capacity_limited(&outcome.summary, &self.config.capacity.markers);

            if capacity_limited {
                self.agents.mark_unavailable(name, cooldown);
                unavailable_hits += 1;
                tracing::warn!(job = %job.id, agent = %name, "agent hit a capacity limit");
                last_outcome = Some(outcome);
                continue;
            }

            last_outcome = Some(outcome);
            break;
        }

        let Some(outcome) = last_outcome else {
            self.finish_no_agent(&key, &job).await;
            return;
        };

        // Every candidate we tried was unavailable (capacity or could not
        // start).
        // When fallback is enabled this means nothing is available right now,
        // so say so explicitly instead of blaming one agent. With fallback
        // disabled the caller asked us not to look further, so report the run
        // itself.
        if self.config.capacity.fallback && unavailable_hits == candidates.len() {
            self.finish_no_agent(&key, &job).await;
        } else {
            self.finish(&key, &job, &used_agent, &outcome).await;
        }
    }

    /// Agents to try for a job, in order.
    fn candidate_agents(&self, requested: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        if self.agents.is_available(requested) {
            candidates.push(requested.to_owned());
        }
        if self.config.capacity.fallback {
            for name in self.agents.available_names() {
                if name != requested {
                    candidates.push(name);
                }
            }
        }
        candidates
    }

    async fn finish(&self, key: &str, job: &Job, agent: &str, outcome: &AgentOutcome) {
        self.persist_outcome(key, job, agent, outcome);

        if self.config.reply.result {
            let status = if outcome.success {
                "✅ finished"
            } else {
                "❌ failed"
            };
            let summary = truncate(&outcome.summary, 6000);
            let body = if summary.trim().is_empty() {
                format!("🤖 Agent **{agent}** {status} in {:?}.", outcome.duration)
            } else {
                format!(
                    "🤖 Agent **{agent}** {status} in {:?}.\n\n{}",
                    outcome.duration, summary
                )
            };
            self.reply(&job.message.location, &body).await;
        }
    }

    /// Report that no agent can take the job. This is a terminal, actionable
    /// error, so it is always posted even when result replies are disabled.
    async fn finish_no_agent(&self, key: &str, job: &Job) {
        let outcome = AgentOutcome::failure(NO_AVAILABLE_AGENT, Default::default());
        self.persist_outcome(key, job, &job.agent, &outcome);
        self.reply(&job.message.location, NO_AVAILABLE_AGENT).await;
    }

    fn persist_outcome(&self, key: &str, job: &Job, agent: &str, outcome: &AgentOutcome) {
        if let Err(error) = self.sessions.finish(key, job.id, agent, outcome) {
            tracing::warn!(%error, "failed to persist session result");
        }
        if let Err(error) = self.sessions.remove_job(job.id) {
            tracing::warn!(%error, "failed to remove persisted job");
        }
    }

    async fn reply(&self, location: &url::Url, body: &str) {
        match self.api.post_comment(location, body).await {
            Ok(()) => {}
            Err(error) if error.is_permission_denied() => {
                tracing::warn!(
                    %location,
                    %error,
                    "cannot reply: the forge denied comment permission"
                );
            }
            Err(error) => tracing::warn!(%error, %location, "failed to post comment"),
        }
    }
}

/// Reply posted when every configured agent is at capacity.
pub const NO_AVAILABLE_AGENT: &str = "No available agent. Every configured agent has hit a quota or capacity limit; \
     please try again later.";

/// Build a failure outcome, replacing forge permission errors with a clear
/// user-facing message.
fn permission_aware_failure(job: &Job, error: &BotError) -> AgentOutcome {
    if error.is_permission_denied() {
        tracing::warn!(
            job = %job.id,
            repo = %job.message.repository,
            %error,
            "forge denied permission"
        );
        AgentOutcome::failure(
            format!(
                "🔒 Permission Deny of {}: the bot is not allowed to access `{}`. \
                 Please ask a repository owner to grant the bot access, then mention it again.",
                job.message.forge, job.message.repository
            ),
            Default::default(),
        )
    } else {
        AgentOutcome::failure(error.to_string(), Default::default())
    }
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_owned();
    }
    let truncated: String = input.chars().take(max).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::forge_api::{NoopForgeApi, RecordingForgeApi};
    use crate::location::ForgeKind;
    use url::Url;

    fn test_config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.session.dir = dir.to_path_buf();
        config.session.workers = 1;
        config.workspace.enabled = false;
        config.reply.ack = false;
        config.reply.result = false;
        config
    }

    fn message(repo: &str) -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            body: "@agent:custom go".into(),
            author: "alice".into(),
            repository: repo.into(),
            comment_id: Some(1),
            number: Some(1),
            is_pull_request: false,
            linked_issue: None,
            event: "issue_comment".into(),
            title: None,
        }
    }

    #[tokio::test]
    async fn acknowledges_when_the_job_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.reply.ack = true;
        config.policy.allow_all = true;
        config.agents.overrides.insert(
            "custom".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        let config = Arc::new(config);

        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingForgeApi::new());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions,
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("custom".into()),
                    message: "go".into(),
                },
                "custom",
            )
            .await
            .unwrap();

        let comments = api.comments();
        assert_eq!(comments.len(), 1, "exactly one acknowledgement");
        assert!(comments[0].1.contains("On it"));
        assert!(comments[0].1.contains("custom"));
    }

    #[tokio::test]
    async fn submit_persists_and_runs_custom_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.agents.overrides.insert(
            "custom".into(),
            crate::config::AgentConfig {
                // `cat` echoes the prompt, so the run succeeds.
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        config.policy.allow_all = true;
        let config = Arc::new(config);

        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions.clone(),
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();

        let id = dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("custom".into()),
                    message: "go".into(),
                },
                "custom",
            )
            .await
            .unwrap();
        assert!(!id.is_nil());

        // Wait for the worker to finish.
        for _ in 0..100 {
            if sessions.pending_jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(sessions.pending_jobs().unwrap().is_empty());
        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(true));
    }

    #[test]
    fn permission_failure_is_user_facing() {
        let job = Job {
            id: Uuid::new_v4(),
            message: message("o/r"),
            mention: Mention {
                agent: None,
                message: "x".into(),
            },
            agent: "pi-rpc".into(),
            created_at: Utc::now(),
        };
        let outcome = permission_aware_failure(
            &job,
            &BotError::ForgePermissionDenied("forge returned 403".into()),
        );
        assert!(!outcome.success);
        assert!(outcome.summary.contains("Permission Deny"));
        assert!(outcome.summary.contains("o/r"));
    }

    #[tokio::test]
    async fn unauthorized_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(test_config(dir.path()));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions,
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();

        let err = dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: None,
                    message: "go".into(),
                },
                "codex",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BotError::Unauthorized(_)));
    }

    /// Forge API that records every comment, so tests can assert on replies.
    #[derive(Default)]
    struct RecordingApi {
        comments: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingApi {
        fn comments(&self) -> Vec<String> {
            self.comments.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ForgeApi for RecordingApi {
        async fn post_comment(&self, _location: &url::Url, body: &str) -> crate::error::Result<()> {
            self.comments.lock().unwrap().push(body.to_owned());
            Ok(())
        }
    }

    async fn wait_for_drain(sessions: &SessionStore) {
        for _ in 0..200 {
            if sessions.pending_jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(sessions.pending_jobs().unwrap().is_empty());
    }

    /// Registry with every built-in disabled, so fallback order is
    /// deterministic in tests.
    fn isolated_registry(config: &Config, keep: &[&str]) -> Arc<AgentRegistry> {
        let registry = AgentRegistry::from_config(config);
        for name in registry.names() {
            if !keep.contains(&name.as_str()) {
                registry.mark_unavailable(&name, Duration::from_secs(3600));
            }
        }
        Arc::new(registry)
    }

    fn capacity_config(dir: &std::path::Path) -> Config {
        let mut config = test_config(dir);
        config.policy.allow_all = true;
        // A shell that reports a quota error and exits non-zero.
        config.agents.overrides.insert(
            "capacity-agent".into(),
            crate::config::AgentConfig {
                command: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo 'You have hit your usage limit' >&2; exit 1".into(),
                ]),
                ..Default::default()
            },
        );
        // A shell that reports a provider overload and exits non-zero.
        config.agents.overrides.insert(
            "overloaded-agent".into(),
            crate::config::AgentConfig {
                command: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo 'overloaded_error: The server is currently overloaded' >&2; exit 1"
                        .into(),
                ]),
                ..Default::default()
            },
        );
        // A shell that echoes its stdin, standing in for a healthy agent.
        config.agents.overrides.insert(
            "good-agent".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        config
    }

    #[tokio::test]
    async fn falls_back_to_another_agent_when_capacity_limited() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(
            session.runs[0].success,
            Some(true),
            "summary: {:?}",
            session.runs[0].summary
        );
        assert_eq!(session.runs[0].agent, "good-agent");
        assert!(
            !registry.is_available("capacity-agent"),
            "capacity-limited agent should be skipped"
        );
    }

    #[tokio::test]
    async fn falls_back_when_the_provider_is_overloaded() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["overloaded-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("overloaded-agent".into()),
                    message: "go".into(),
                },
                "overloaded-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(
            session.runs[0].success,
            Some(true),
            "summary: {:?}",
            session.runs[0].summary
        );
        assert_eq!(session.runs[0].agent, "good-agent");
        assert!(!registry.is_available("overloaded-agent"));
    }

    #[tokio::test]
    async fn falls_back_when_an_agent_cannot_be_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        // An adapter whose binary is not installed: it cannot be started.
        config.agents.overrides.insert(
            "missing-agent".into(),
            crate::config::AgentConfig {
                command: Some("definitely-not-a-real-binary-xyz".into()),
                ..Default::default()
            },
        );
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["missing-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("missing-agent".into()),
                    message: "go".into(),
                },
                "missing-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(true));
        assert_eq!(session.runs[0].agent, "good-agent");
        assert!(!registry.is_available("missing-agent"));
    }

    #[tokio::test]
    async fn reports_no_available_agent_when_capacity_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["capacity-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(false));
        assert!(
            session.runs[0]
                .summary
                .as_deref()
                .unwrap()
                .contains("No available agent")
        );
        assert!(
            api.comments()
                .iter()
                .any(|body| body.contains("No available agent")),
            "the bot must reply that no agent is available"
        );
    }

    #[tokio::test]
    async fn capacity_failure_is_reported_when_fallback_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.capacity.fallback = false;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        // The capacity error itself is surfaced; `good-agent` was never asked.
        assert_eq!(session.runs[0].success, Some(false));
        assert_eq!(session.runs[0].agent, "capacity-agent");
        assert!(
            session.runs[0]
                .summary
                .as_deref()
                .unwrap()
                .contains("usage limit"),
            "summary: {:?}",
            session.runs[0].summary
        );
        assert!(!registry.is_available("capacity-agent"));
        assert!(registry.is_available("good-agent"));
    }

    #[tokio::test]
    async fn skips_an_agent_already_known_to_be_capacity_limited() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        // A previous job already exhausted the requested agent.
        registry.mark_unavailable("capacity-agent", Duration::from_secs(3600));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(true));
        assert_eq!(session.runs[0].agent, "good-agent");
    }

    /// The ack is posted once by `submit` as soon as the mention is accepted;
    /// a later fallback must only add the "switching" notice, not repeat the
    /// "on it" acknowledgement.
    #[tokio::test]
    async fn ack_is_not_duplicated_when_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        // A previous job already exhausted the requested agent.
        registry.mark_unavailable("capacity-agent", Duration::from_secs(3600));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(true));
        assert_eq!(session.runs[0].agent, "good-agent");

        let comments = api.comments();
        assert_eq!(
            comments.iter().filter(|c| c.contains("On it")).count(),
            1,
            "the prompt acknowledgement must not be duplicated: {comments:?}"
        );
        assert!(
            comments
                .iter()
                .any(|c| c.contains("at capacity") && c.contains("instead")),
            "the fallback should be announced: {comments:?}"
        );
    }
}
