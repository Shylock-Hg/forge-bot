//! GitHub webhook adapter.
//!
//! GitHub reuses the same `issue_comment` payload shape as Forgejo/Gitea, so
//! parsing is delegated to the shared helper. Verification uses the
//! `X-Hub-Signature-256` HMAC header.

use axum::http::HeaderMap;

use crate::config::GithubConfig;
use crate::error::{BotError, Result};
use crate::forge::{ForgeAdapter, ForgeMessage, parse_comment_payload, verify_hmac_sha256};
use crate::location::ForgeKind;

/// GitHub webhook adapter.
#[derive(Debug, Clone)]
pub struct GithubAdapter {
    base_url: String,
    secret: Option<Vec<u8>>,
    bot_username: Option<String>,
    pub token: Option<String>,
}

impl GithubAdapter {
    pub fn new(config: &GithubConfig) -> Self {
        let base_url = if config.base_url.is_empty() {
            "https://github.com".to_owned()
        } else {
            config.base_url.trim_end_matches('/').to_owned()
        };
        Self {
            base_url,
            secret: config
                .webhook_secret
                .as_ref()
                .map(|s| s.as_bytes().to_vec()),
            bot_username: config.bot_username.clone(),
            token: config.token.clone(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn bot_username(&self) -> Option<&str> {
        self.bot_username.as_deref()
    }
}

#[async_trait::async_trait]
impl ForgeAdapter for GithubAdapter {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn slug(&self) -> &'static str {
        "github"
    }

    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()> {
        let Some(secret) = self.secret.as_ref() else {
            tracing::warn!("github: no webhook secret configured; skipping verification");
            return Ok(());
        };

        let signature = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| BotError::Verification("missing X-Hub-Signature-256 header".into()))?;

        if verify_hmac_sha256(secret, body, signature) {
            Ok(())
        } else {
            Err(BotError::Verification("signature mismatch".into()))
        }
    }

    fn event(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-github-event")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>> {
        let event = self.event(headers);
        if event != "issue_comment" && !event.is_empty() {
            return Ok(Vec::new());
        }
        parse_comment_payload(ForgeKind::GitHub, &self.base_url, body, &event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::hmac_sha256_hex;

    #[test]
    fn parses_github_issue_comment() {
        let a = GithubAdapter::new(&GithubConfig {
            base_url: String::new(),
            webhook_secret: Some("s".into()),
            token: None,
            bot_username: None,
        });
        let raw = r#"{
            "action": "created",
            "issue": {"number": 12, "title": "bug",
                      "html_url": "https://github.com/o/r/issues/12"},
            "comment": {"id": 9, "body": "@agent fix",
                        "html_url": "https://github.com/o/r/issues/12#issuecomment-9",
                        "user": {"login": "dev"}},
            "repository": {"full_name": "o/r"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"s", body);
        let mut h = HeaderMap::new();
        h.insert("x-github-event", "issue_comment".parse().unwrap());
        h.insert(
            "x-hub-signature-256",
            format!("sha256={sig}").parse().unwrap(),
        );
        let msgs = a.handle(&h, body).unwrap();
        assert_eq!(msgs[0].forge, ForgeKind::GitHub);
        assert_eq!(msgs[0].number, Some(12));
    }
}
