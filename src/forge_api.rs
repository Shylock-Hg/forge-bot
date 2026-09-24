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
use crate::forge::{ForgeMessage, ReplyTarget, ReviewCommentTarget};
use crate::location::{ForgeKind, ForgeLocation};

/// Minimal forge write API used by the gateway.
#[async_trait]
pub trait ForgeApi: Send + Sync {
    /// Post `body` as a comment on the issue / pull request at `location`.
    async fn post_comment(&self, location: &Url, body: &str) -> Result<()>;

    /// Post `body` as a reply to the comment that triggered `message`.
    ///
    /// The default keeps every reply in the issue / pull-request
    /// conversation. Forges that can thread an inline review comment override
    /// this so the answer stays attached to the code line it discusses.
    async fn reply(&self, message: &ForgeMessage, body: &str) -> Result<()> {
        self.post_comment(&message.location, body).await
    }
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

    /// Post a reply inside the inline review thread `target` identifies.
    ///
    /// Forgejo groups review comments by review, file path and line, so the
    /// reply is created through the review comment endpoint with the same
    /// coordinates as the original mention.
    async fn post_forgejo_review(
        &self,
        loc: &ForgeLocation,
        target: &ReviewCommentTarget,
        body: &str,
    ) -> Result<()> {
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
            "{}/api/v1/repos/{}/{}/pulls/{}/reviews/{}/comments",
            cfg.base_url.trim_end_matches('/'),
            loc.owner,
            loc.repo,
            number,
            target.review_id
        );
        let (new_position, old_position) = if target.line < 0 {
            (0, -target.line)
        } else {
            (target.line, 0)
        };
        let mut req = self.client.post(&url).json(&serde_json::json!({
            "body": body,
            "path": target.path,
            "new_position": new_position,
            "old_position": old_position,
            "extra_lines_count": target.extra_lines_count,
        }));
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

    async fn reply(&self, message: &ForgeMessage, body: &str) -> Result<()> {
        if let ReplyTarget::ReviewComment(target) = &message.reply_target {
            let loc = ForgeLocation::parse(&message.location)?;
            // Without a line the review endpoint cannot anchor the comment, so
            // fall back to a normal conversation reply.
            if target.line != 0 && matches!(loc.forge, ForgeKind::Forgejo | ForgeKind::Gitea) {
                return self.post_forgejo_review(&loc, target, body).await;
            }
        }
        self.post_comment(&message.location, body).await
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
    use crate::config::{Config, ForgejoConfig};
    use crate::forge::{ForgeMessage, ReplyTarget, ReviewCommentTarget};

    #[test]
    fn encodes_project_path() {
        assert_eq!(urlencoding("group/proj"), "group%2Fproj");
        assert_eq!(urlencoding("a.b-c_d"), "a.b-c_d");
    }

    fn review_message() -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local/a/b/pulls/22#issuecomment-9039").unwrap(),
            body: "@agent how long does this live?".into(),
            author: "shylock".into(),
            repository: "a/b".into(),
            comment_id: Some(9039),
            number: Some(22),
            is_pull_request: true,
            linked_issue: None,
            event: "pull_request_comment".into(),
            title: None,
            reply_target: ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 103,
                path: "src/agent/registry.rs".into(),
                line: 30,
                extra_lines_count: 0,
            }),
        }
    }

    #[tokio::test]
    async fn replies_inline_review_comments_on_forgejo() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            let _ = socket.write_all(response.as_bytes()).await;
            request
        });

        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: format!("http://{addr}"),
            token: Some("secret".into()),
            ..Default::default()
        });
        let api = HttpForgeApi::new(config).unwrap();
        api.reply(&review_message(), "It lives for one hour.")
            .await
            .unwrap();

        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/v1/repos/a/b/pulls/22/reviews/103/comments "),
            "unexpected request line: {request}"
        );
        let lower = request.to_lowercase();
        assert!(lower.contains("authorization: token secret"));
        assert!(request.contains("\"path\":\"src/agent/registry.rs\""));
        assert!(request.contains("\"new_position\":30"));
        assert!(request.contains("\"old_position\":0"));
        assert!(request.contains("It lives for one hour."));
    }

    #[tokio::test]
    async fn old_side_review_replies_use_the_old_position() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            let _ = socket.write_all(response.as_bytes()).await;
            request
        });

        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: format!("http://{addr}"),
            token: None,
            ..Default::default()
        });
        let api = HttpForgeApi::new(config).unwrap();
        let mut message = review_message();
        message.location = Url::parse("http://forge.local/a/b/pulls/22#issuecomment-8992").unwrap();
        message.reply_target = ReplyTarget::ReviewComment(ReviewCommentTarget {
            review_id: 103,
            path: "src/agent/registry.rs".into(),
            line: -12,
            extra_lines_count: 0,
        });
        api.reply(&message, "reply").await.unwrap();

        let request = server.await.unwrap();
        assert!(request.contains("\"new_position\":0"));
        assert!(request.contains("\"old_position\":12"));
    }
}
