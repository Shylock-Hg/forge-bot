//! Polling ingester.
//!
//! A repository collaborator without admin rights cannot create a Forgejo
//! webhook. Polling the issue-comments API is a drop-in alternative: it feeds
//! the exact same [`Dispatcher`] pipeline the webhook handler uses, so all the
//! mention detection, authorization, agent selection and reply logic is
//! shared.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::config::Config;
use crate::error::{BotError, Result};
use crate::forge::ForgeMessage;
use crate::location::ForgeKind;
use crate::mention::extract_mention;
use crate::session::Dispatcher;

/// High-water mark for one repository.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Cursor {
    last_id: i64,
    last_time: Option<String>,
}

/// Cached list of repositories to poll.
#[derive(Default)]
struct RepoCache {
    repos: Vec<String>,
    refreshed: Option<Instant>,
}

/// Watches repositories for new `@agent` mentions.
pub struct Poller {
    config: Arc<Config>,
    dispatcher: Arc<Dispatcher>,
    client: reqwest::Client,
    state_path: PathBuf,
    cursors: Mutex<HashMap<String, Cursor>>,
    repos: Mutex<RepoCache>,
}

impl Poller {
    pub fn new(config: Arc<Config>, dispatcher: Arc<Dispatcher>) -> Result<Self> {
        let state_dir = crate::config::expand_tilde(&config.session.dir);
        std::fs::create_dir_all(&state_dir)?;
        let state_path = state_dir.join("poller.json");

        let cursors = match std::fs::read_to_string(&state_path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                tracing::warn!(%error, "ignoring unreadable poller state");
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("forge-bot/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self {
            config,
            dispatcher,
            client,
            state_path,
            cursors: Mutex::new(cursors),
            repos: Mutex::new(RepoCache::default()),
        })
    }

    /// Run until the process exits.
    pub async fn run(self: Arc<Self>) {
        let interval = Duration::from_secs(self.config.poller.interval_secs.max(1));
        tracing::info!(
            repositories = ?self.config.poller.repositories,
            interval_secs = interval.as_secs(),
            discover = self.config.poller.repositories.is_empty(),
            "poller started"
        );
        loop {
            if let Err(error) = self.tick().await {
                tracing::warn!(%error, "poll failed");
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// One polling pass over every repository in scope.
    pub async fn tick(&self) -> Result<()> {
        let forgejo = self
            .config
            .forges
            .forgejo
            .as_ref()
            .ok_or_else(|| BotError::Config("poller requires a [forgejo] section".into()))?;

        let base = forgejo.base_url.trim_end_matches('/').to_owned();
        let token = forgejo.token.clone();

        for repo in self.repositories(&base, token.as_deref()).await? {
            if let Err(error) = self.poll_repo(&base, token.as_deref(), &repo).await {
                tracing::warn!(repo = %repo, %error, "failed to poll repository");
            }
        }

        self.persist();
        Ok(())
    }

    /// Repositories to poll: the explicit list when configured, otherwise all
    /// repositories visible to the token (refreshed periodically).
    async fn repositories(&self, base: &str, token: Option<&str>) -> Result<Vec<String>> {
        if !self.config.poller.repositories.is_empty() {
            return Ok(self.config.poller.repositories.clone());
        }

        let ttl = Duration::from_secs(self.config.poller.discover_interval_secs.max(1));
        let fresh = {
            let cache = self.repos.lock().expect("poller repo cache poisoned");
            !cache.repos.is_empty()
                && cache
                    .refreshed
                    .map(|at| at.elapsed() < ttl)
                    .unwrap_or(false)
        };
        if fresh {
            return Ok(self
                .repos
                .lock()
                .expect("poller repo cache poisoned")
                .repos
                .clone());
        }

        match self.discover_repositories(base, token).await {
            Ok(repos) => {
                tracing::info!(count = repos.len(), "discovered repositories");
                let mut cache = self.repos.lock().expect("poller repo cache poisoned");
                cache.repos = repos.clone();
                cache.refreshed = Some(Instant::now());
                Ok(repos)
            }
            Err(error) => {
                // Keep serving the previous list if discovery fails.
                let cached = self
                    .repos
                    .lock()
                    .expect("poller repo cache poisoned")
                    .repos
                    .clone();
                if cached.is_empty() {
                    Err(error)
                } else {
                    tracing::warn!(%error, "repository discovery failed; using cached list");
                    Ok(cached)
                }
            }
        }
    }

    /// Enumerate every repository visible to the token.
    async fn discover_repositories(&self, base: &str, token: Option<&str>) -> Result<Vec<String>> {
        let limit = self.config.poller.page_limit.max(1);
        let url = format!("{base}/api/v1/repos/search");
        let mut repos = Vec::new();

        for page in 1..=100 {
            let mut request = self.client.get(&url).query(&[
                ("limit", limit.to_string()),
                ("page", page.to_string()),
                ("sort", "id".to_owned()),
                ("order", "asc".to_owned()),
            ]);
            if let Some(token) = token {
                request = request.header("Authorization", format!("token {token}"));
            }

            let response = request.send().await?;
            if !response.status().is_success() {
                return Err(BotError::ForgeApi(format!(
                    "repository search returned {}",
                    response.status()
                )));
            }
            let payload: Value = response.json().await?;
            let page_repos = repos_from_search_page(&payload);
            let full_page = payload["data"].as_array().map(Vec::len).unwrap_or(0) >= limit;
            repos.extend(page_repos);
            if !full_page {
                break;
            }
        }

        repos.sort();
        repos.dedup();
        Ok(repos)
    }

    async fn poll_repo(&self, base: &str, token: Option<&str>, repo: &str) -> Result<()> {
        let since = self.since_for(repo);
        let limit = self.config.poller.page_limit.max(1);
        let url = format!("{base}/api/v1/repos/{repo}/issues/comments");

        let mut request = self
            .client
            .get(&url)
            .query(&[("limit", limit.to_string()), ("since", since)]);
        if let Some(token) = token {
            request = request.header("Authorization", format!("token {token}"));
        }

        let response = request.send().await?;
        let status = response.status();
        // Repositories the token cannot read are expected when scanning the
        // whole instance; skip them quietly.
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
        ) {
            tracing::debug!(repo, %status, "skipping inaccessible repository");
            return Ok(());
        }
        if !status.is_success() {
            return Err(BotError::ForgeApi(format!(
                "comments query returned {status}"
            )));
        }
        let mut comments: Vec<Value> = response.json().await?;
        comments.sort_by_key(|comment| comment["id"].as_i64().unwrap_or_default());

        let mut cursor = self.cursor(repo);
        for comment in comments {
            let id = comment["id"].as_i64().unwrap_or_default();
            if id <= cursor.last_id {
                continue;
            }
            // Advance the cursor even for comments we ignore so they are not
            // reconsidered on the next pass.
            cursor.last_id = id;
            if let Some(created) = comment["created_at"].as_str() {
                cursor.last_time = Some(normalize_time(created));
            }

            let Some(message) = message_from_comment(repo, &comment) else {
                continue;
            };
            if self.dispatcher.policy().is_ignored(&message.author) {
                continue;
            }
            let Some(mention) = extract_mention(&message.body, self.config.trigger()) else {
                continue;
            };

            let agent_name = mention
                .agent
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| self.dispatcher.default_agent_name().to_owned());

            match self.dispatcher.submit(message, mention, &agent_name).await {
                Ok(job_id) => tracing::info!(%job_id, repo, "accepted polled trigger"),
                Err(BotError::Unauthorized(reason)) => {
                    tracing::info!(%reason, repo, "ignored unauthorized trigger");
                }
                Err(error) => tracing::warn!(%error, repo, "failed to enqueue polled trigger"),
            }
        }

        self.set_cursor(repo, cursor);
        Ok(())
    }

    fn cursor(&self, repo: &str) -> Cursor {
        self.cursors
            .lock()
            .expect("poller mutex poisoned")
            .get(repo)
            .cloned()
            .unwrap_or_default()
    }

    fn set_cursor(&self, repo: &str, cursor: Cursor) {
        self.cursors
            .lock()
            .expect("poller mutex poisoned")
            .insert(repo.to_owned(), cursor);
    }

    /// The `since` query value for a repository.
    fn since_for(&self, repo: &str) -> String {
        if let Some(time) = self.cursor(repo).last_time {
            return time;
        }
        let lookback = chrono::Duration::seconds(self.config.poller.lookback_secs as i64);
        (Utc::now() - lookback).to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    fn persist(&self) {
        let cursors = self.cursors.lock().expect("poller mutex poisoned");
        match serde_json::to_vec_pretty(&*cursors) {
            Ok(raw) => {
                let tmp = self.state_path.with_extension("json.tmp");
                if std::fs::write(&tmp, raw)
                    .and_then(|_| std::fs::rename(&tmp, &self.state_path))
                    .is_err()
                {
                    tracing::warn!(path = %self.state_path.display(), "failed to persist poller state");
                }
            }
            Err(error) => tracing::warn!(%error, "failed to serialize poller state"),
        }
    }
}

/// Extract the `owner/repo` names from a `/repos/search` page, skipping
/// repositories without issues or that are archived.
fn repos_from_search_page(payload: &Value) -> Vec<String> {
    payload["data"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|repo| repo["has_issues"].as_bool().unwrap_or(true))
        .filter(|repo| !repo["archived"].as_bool().unwrap_or(false))
        .filter_map(|repo| repo["full_name"].as_str().map(str::to_owned))
        .collect()
}

/// Normalize a Forgejo timestamp to UTC RFC3339 so it can be sent back as a
/// `since` query value without an unescaped `+` offset.
fn normalize_time(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|time| {
            time.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|_| raw.to_owned())
}

/// Convert a Forgejo comment payload into a normalized message.
fn message_from_comment(repo: &str, comment: &Value) -> Option<ForgeMessage> {
    let body = comment["body"].as_str()?.to_owned();
    let author = comment["user"]["login"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let location = comment["html_url"]
        .as_str()
        .and_then(|s| Url::parse(s).ok())?;

    let comment_id = comment["id"].as_i64();
    let number = comment["issue_url"]
        .as_str()
        .and_then(|url| url.rsplit('/').next())
        .and_then(|tail| tail.parse::<u64>().ok());
    let is_pull_request = comment["pull_request_url"]
        .as_str()
        .map(|url| !url.is_empty())
        .unwrap_or(false);

    Some(ForgeMessage {
        forge: ForgeKind::Forgejo,
        location,
        body,
        author,
        repository: repo.to_owned(),
        comment_id,
        number,
        is_pull_request,
        linked_issue: None,
        event: "issue_comment".into(),
        title: None,
        reply_target: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filters_search_page() {
        let payload = json!({
            "ok": true,
            "data": [
                {"full_name": "a/one", "has_issues": true, "archived": false},
                {"full_name": "a/two", "has_issues": false, "archived": false},
                {"full_name": "a/three", "has_issues": true, "archived": true},
                {"full_name": "a/four", "has_issues": true, "archived": false}
            ]
        });
        assert_eq!(
            repos_from_search_page(&payload),
            vec!["a/one".to_string(), "a/four".to_string()]
        );
    }

    #[test]
    fn converts_comment_to_message() {
        let comment = json!({
            "id": 77,
            "body": "@agent do it",
            "html_url": "http://forge.local:3000/o/r/issues/3#issuecomment-77",
            "issue_url": "http://forge.local:3000/o/r/issues/3",
            "pull_request_url": "",
            "user": { "login": "alice" }
        });
        let message = message_from_comment("o/r", &comment).unwrap();
        assert_eq!(message.author, "alice");
        assert_eq!(message.number, Some(3));
        assert_eq!(message.comment_id, Some(77));
        assert!(!message.is_pull_request);
        assert_eq!(message.forge, ForgeKind::Forgejo);
    }

    #[test]
    fn normalizes_offsets_to_utc() {
        assert_eq!(
            normalize_time("2026-09-24T20:49:34+08:00"),
            "2026-09-24T12:49:34Z"
        );
        assert_eq!(
            normalize_time("2026-09-24T12:49:34Z"),
            "2026-09-24T12:49:34Z"
        );
    }

    #[test]
    fn detects_pull_request_comments() {
        let comment = json!({
            "id": 1,
            "body": "@agent x",
            "html_url": "http://forge.local:3000/o/r/pulls/5#issuecomment-1",
            "issue_url": "http://forge.local:3000/o/r/issues/5",
            "pull_request_url": "http://forge.local:3000/o/r/pulls/5",
            "user": { "login": "bob" }
        });
        let message = message_from_comment("o/r", &comment).unwrap();
        assert!(message.is_pull_request);
        assert_eq!(message.number, Some(5));
    }
}
