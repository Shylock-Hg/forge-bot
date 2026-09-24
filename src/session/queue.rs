//! Bounded job queue and worker pool.

use std::sync::Arc;

use chrono::Utc;
use tokio::sync::{Semaphore, mpsc};
use uuid::Uuid;

use crate::agent::{AgentContext, AgentOutcome, AgentRequest};
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
    agents: crate::agent::AgentRegistry,
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
        agents: crate::agent::AgentRegistry,
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
                    &AgentOutcome::failure(error.to_string(), Default::default()),
                )
                .await;
                return;
            }
        };

        let agent = match self.agents.resolve(Some(&job.agent)) {
            Ok(agent) => agent,
            Err(error) => {
                self.finish(
                    &key,
                    &job,
                    &AgentOutcome::failure(error.to_string(), Default::default()),
                )
                .await;
                return;
            }
        };

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
            title: job.message.title.clone(),
            credentials,
        };

        if self.config.reply.ack {
            let ack = format!(
                "🤖 On it — running agent **{}**. I'll report back here when it finishes.",
                job.agent
            );
            self.reply(&job.message.location, &ack).await;
        }

        let outcome = agent.run(&request, &context).await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => AgentOutcome::failure(error.to_string(), Default::default()),
        };

        self.finish(&key, &job, &outcome).await;
    }

    async fn finish(&self, key: &str, job: &Job, outcome: &AgentOutcome) {
        if let Err(error) = self.sessions.finish(key, job.id, outcome) {
            tracing::warn!(%error, "failed to persist session result");
        }
        if let Err(error) = self.sessions.remove_job(job.id) {
            tracing::warn!(%error, "failed to remove persisted job");
        }

        if self.config.reply.result {
            let status = if outcome.success {
                "✅ finished"
            } else {
                "❌ failed"
            };
            let summary = truncate(&outcome.summary, 6000);
            let body = if summary.trim().is_empty() {
                format!(
                    "🤖 Agent **{}** {status} in {:?}.",
                    job.agent, outcome.duration
                )
            } else {
                format!(
                    "🤖 Agent **{}** {status} in {:?}.\n\n{}",
                    job.agent, outcome.duration, summary
                )
            };
            self.reply(&job.message.location, &body).await;
        }
    }

    async fn reply(&self, location: &url::Url, body: &str) {
        if let Err(error) = self.api.post_comment(location, body).await {
            tracing::warn!(%error, %location, "failed to post comment");
        }
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
    use crate::forge_api::NoopForgeApi;
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
            event: "issue_comment".into(),
            title: None,
        }
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
            AgentRegistry::from_config(&config),
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

    #[tokio::test]
    async fn unauthorized_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(test_config(dir.path()));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            AgentRegistry::from_config(&config),
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
}
