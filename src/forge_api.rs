//! Posting comments back to a forge.
//!
//! The gateway uses this for acknowledgements and result summaries. Agents may
//! also post comments themselves using the credentials exported into their
//! environment.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;

use crate::config::Config;
use crate::error::{BotError, Result};
use crate::location::{ForgeKind, ForgeLocation};

/// Minimal forge write API used by the gateway.
#[async_trait]
pub trait ForgeApi: Send + Sync {
    /// Post `body` as a comment on the issue / pull request at `location`.
    async fn post_comment(&self, location: &Url, body: &str) -> Result<()>;
}

/// A [`ForgeApi`] that posts over HTTP to the supported forges.
pub struct HttpForgeApi {
    client: reqwest::Client,
    config: Config,
}

impl HttpForgeApi {
    pub fn new(config: Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("forge-bot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client, config })
    }

    async fn post_forgejo(&self, loc: &ForgeLocation, body: &str) -> Result<()> {
        let cfg = self
            .config
            .forges
            .forgejo
            .as_ref()
            .ok_or_else(|| BotError::ForgeApi("forgejo is not configured".into()))?;
        let number = loc
            .number
            .ok_or_else(|| BotError::ForgeApi("location has no issue number".into()))?;
        let url = format!(
            "{}/api/v1/repos/{}/{}/issues/{}/comments",
            cfg.base_url.trim_end_matches('/'),
            loc.owner,
            loc.repo,
            number
        );
        let mut req = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }));
        if let Some(token) = &cfg.token {
            req = req.header("Authorization", format!("token {token}"));
        }
        send(req).await
    }

    async fn post_github(&self, loc: &ForgeLocation, body: &str) -> Result<()> {
        let cfg = self.config.forges.github.as_ref();
        let base = cfg
            .map(|c| c.base_url.trim_end_matches('/').to_owned())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "https://api.github.com".to_owned());
        let number = loc
            .number
            .ok_or_else(|| BotError::ForgeApi("location has no issue number".into()))?;
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            base, loc.owner, loc.repo, number
        );
        let mut req = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }));
        if let Some(token) = cfg.and_then(|c| c.token.as_ref()) {
            req = req
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", "application/vnd.github+json");
        }
        send(req).await
    }

    async fn post_gitlab(&self, loc: &ForgeLocation, body: &str) -> Result<()> {
        let cfg = self
            .config
            .forges
            .gitlab
            .as_ref()
            .ok_or_else(|| BotError::ForgeApi("gitlab is not configured".into()))?;
        let base = if cfg.base_url.is_empty() {
            "https://gitlab.com".to_owned()
        } else {
            cfg.base_url.trim_end_matches('/').to_owned()
        };
        let number = loc
            .number
            .ok_or_else(|| BotError::ForgeApi("location has no issue number".into()))?;
        let project = urlencoding(&format!("{}/{}", loc.owner, loc.repo));
        let resource = match loc.kind {
            crate::location::LocationKind::PullRequest => "merge_requests",
            _ => "issues",
        };
        let url = format!("{base}/api/v4/projects/{project}/{resource}/{number}/notes");
        let mut req = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }));
        if let Some(token) = &cfg.token {
            req = req.header("PRIVATE-TOKEN", token);
        }
        send(req).await
    }
}

#[async_trait]
impl ForgeApi for HttpForgeApi {
    async fn post_comment(&self, location: &Url, body: &str) -> Result<()> {
        let loc = ForgeLocation::parse(location)?;
        match loc.forge {
            ForgeKind::Forgejo | ForgeKind::Gitea => self.post_forgejo(&loc, body).await,
            ForgeKind::GitHub => self.post_github(&loc, body).await,
            ForgeKind::GitLab => self.post_gitlab(&loc, body).await,
            ForgeKind::Unknown => Err(BotError::UnsupportedForge(loc.forge.to_string())),
        }
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<()> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let text = response.text().await.unwrap_or_default();
    let detail = format!("forge returned {status}: {}", text.trim());
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(BotError::ForgePermissionDenied(detail));
    }
    Err(BotError::ForgeApi(detail))
}

/// A [`ForgeApi`] that drops comments, useful for tests and dry runs.
#[derive(Debug, Default, Clone)]
pub struct NoopForgeApi;

#[async_trait]
impl ForgeApi for NoopForgeApi {
    async fn post_comment(&self, location: &Url, body: &str) -> Result<()> {
        tracing::debug!(%location, %body, "noop forge api: dropping comment");
        Ok(())
    }
}

/// A [`ForgeApi`] that records every posted comment, for tests.
#[derive(Debug, Default)]
pub struct RecordingForgeApi {
    comments: std::sync::Mutex<Vec<(Url, String)>>,
}

impl RecordingForgeApi {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the comments posted so far, oldest first.
    pub fn comments(&self) -> Vec<(Url, String)> {
        self.comments
            .lock()
            .expect("recording forge api mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl ForgeApi for RecordingForgeApi {
    async fn post_comment(&self, location: &Url, body: &str) -> Result<()> {
        self.comments
            .lock()
            .expect("recording forge api mutex poisoned")
            .push((location.clone(), body.to_owned()));
        Ok(())
    }
}

/// Percent-encode a string for use inside a URL path segment.
fn urlencoding(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_project_path() {
        assert_eq!(urlencoding("group/proj"), "group%2Fproj");
        assert_eq!(urlencoding("a.b-c_d"), "a.b-c_d");
    }
}
