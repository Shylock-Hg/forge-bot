//! Forgejo / Gitea webhook adapter.

use axum::http::HeaderMap;

use crate::config::ForgejoConfig;
use crate::error::{BotError, Result};
use crate::forge::{
    ForgeAdapter, ForgeMessage, parse_comment_payload, parse_description_payload,
    verify_hmac_sha256,
};
use crate::location::ForgeKind;

/// Forgejo webhook adapter.
#[derive(Debug, Clone)]
pub struct ForgejoAdapter {
    base_url: String,
    secret: Option<Vec<u8>>,
    bot_username: Option<String>,
    pub token: Option<String>,
}

impl ForgejoAdapter {
    pub fn new(config: &ForgejoConfig) -> Self {
        Self {
            base_url: config.base_url.trim_end_matches('/').to_owned(),
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
impl ForgeAdapter for ForgejoAdapter {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Forgejo
    }

    fn slug(&self) -> &'static str {
        "forgejo"
    }

    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()> {
        let Some(secret) = self.secret.as_ref() else {
            tracing::warn!("forgejo: no webhook secret configured; skipping verification");
            return Ok(());
        };

        let signature = headers
            .get("x-forgejo-signature")
            .or_else(|| headers.get("x-gitea-signature"))
            .and_then(|v| v.to_str().ok());

        let Some(signature) = signature else {
            return Err(BotError::Verification(
                "missing X-Forgejo-Signature header".into(),
            ));
        };

        if verify_hmac_sha256(secret, body, signature) {
            Ok(())
        } else {
            Err(BotError::Verification("signature mismatch".into()))
        }
    }

    fn event(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-forgejo-event")
            .or_else(|| headers.get("x-gitea-event"))
            .or_else(|| headers.get("x-github-event"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>> {
        let event = self.event(headers);

        match event.as_str() {
            // Gitea/Forgejo signal pull request comments with `issue_comment`
            // plus a `pull_request` object; some versions send
            // `pull_request_comment`.
            "issue_comment" | "pull_request_comment" | "pull_request_review_comment" | "" => {
                parse_comment_payload(ForgeKind::Forgejo, &self.base_url, body, &event)
            }
            // Mentions in an issue or pull-request description.
            "issues" | "pull_request" => {
                parse_description_payload(ForgeKind::Forgejo, &self.base_url, body, &event)
            }
            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{IssueRef, hmac_sha256_hex};

    fn adapter() -> ForgejoAdapter {
        ForgejoAdapter::new(&ForgejoConfig {
            base_url: "http://forge.local:3000".into(),
            webhook_secret: Some("hush".into()),
            token: None,
            bot_username: Some("botty".into()),
        })
    }

    fn headers(event: &str, sig: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forgejo-event", event.parse().unwrap());
        if let Some(sig) = sig {
            h.insert("x-forgejo-signature", sig.parse().unwrap());
        }
        h
    }

    const PAYLOAD: &str = r#"{
        "action": "created",
        "issue": {
            "number": 1,
            "title": "initial plan",
            "html_url": "http://forge.local:3000/shylock/forge-bot/issues/1"
        },
        "comment": {
            "id": 55,
            "body": "@agent do the thing",
            "html_url": "http://forge.local:3000/shylock/forge-bot/issues/1#issuecomment-55",
            "user": {"login": "shylock"}
        },
        "repository": {"full_name": "shylock/forge-bot"},
        "sender": {"login": "shylock"}
    }"#;

    #[test]
    fn verifies_and_parses() {
        let a = adapter();
        let body = PAYLOAD.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issue_comment", Some(&sig));
        let msgs = a.handle(&h, body).unwrap();
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.author, "shylock");
        assert_eq!(m.repository, "shylock/forge-bot");
        assert_eq!(m.number, Some(1));
        assert_eq!(m.comment_id, Some(55));
        assert!(!m.is_pull_request);
        assert_eq!(m.location.fragment(), Some("issuecomment-55"));
    }

    #[test]
    fn rejects_bad_signature() {
        let a = adapter();
        let h = headers("issue_comment", Some("nope"));
        assert!(a.handle(&h, PAYLOAD.as_bytes()).is_err());
    }

    #[test]
    fn ignores_non_comment_events() {
        let a = adapter();
        let body = PAYLOAD.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("push", Some(&sig));
        assert!(a.handle(&h, body).unwrap().is_empty());
    }

    #[test]
    fn ignores_edited_comments() {
        let a = adapter();
        let body = PAYLOAD
            .replace("\"action\": \"created\"", "\"action\": \"edited\"")
            .into_bytes();
        let sig = hmac_sha256_hex(b"hush", &body);
        let h = headers("issue_comment", Some(&sig));
        assert!(a.handle(&h, &body).unwrap().is_empty());
    }

    #[test]
    fn detects_pull_request() {
        let a = adapter();
        let raw = r#"{
            "action": "created",
            "issue": {"number": 3, "pull_request": {"url": "x"},
                      "html_url": "http://forge.local:3000/a/b/pulls/3"},
            "comment": {"id": 1, "body": "@agent x", "user": {"login": "u"}},
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issue_comment", Some(&sig));
        let m = &a.handle(&h, body).unwrap()[0];
        assert!(m.is_pull_request);
    }

    #[test]
    fn links_pull_request_to_issue_from_description() {
        let a = adapter();
        let raw = r#"{
            "action": "created",
            "issue": {
                "number": 12,
                "body": "This fixes #5 and adds tests.",
                "pull_request": {"url": "x"},
                "html_url": "http://forge.local:3000/a/b/pulls/12"
            },
            "comment": {"id": 3, "body": "@agent x", "user": {"login": "u"}},
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issue_comment", Some(&sig));
        let m = &a.handle(&h, body).unwrap()[0];
        assert!(m.is_pull_request);
        assert_eq!(m.number, Some(12));
        assert_eq!(
            m.linked_issue,
            Some(IssueRef {
                repository: None,
                number: 5,
            })
        );
    }

    #[test]
    fn links_pull_request_to_issue_in_another_repository() {
        let a = adapter();
        let raw = r#"{
            "action": "created",
            "issue": {
                "number": 12,
                "body": "This fixes other/repo#5 and adds tests.",
                "pull_request": {"url": "x"},
                "html_url": "http://forge.local:3000/a/b/pulls/12"
            },
            "comment": {"id": 3, "body": "@agent x", "user": {"login": "u"}},
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issue_comment", Some(&sig));
        let m = &a.handle(&h, body).unwrap()[0];
        assert_eq!(
            m.linked_issue,
            Some(IssueRef {
                repository: Some("other/repo".into()),
                number: 5,
            })
        );
    }

    #[test]
    fn parses_issue_description() {
        let a = adapter();
        let raw = r#"{
            "action": "opened",
            "issue": {
                "number": 4,
                "title": "broken build",
                "body": "@agent please look at this",
                "html_url": "http://forge.local:3000/a/b/issues/4",
                "user": {"login": "u"}
            },
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issues", Some(&sig));
        let msgs = a.handle(&h, body).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].number, Some(4));
        assert_eq!(msgs[0].comment_id, None);
        assert!(!msgs[0].is_pull_request);
        assert_eq!(msgs[0].body, "@agent please look at this");
        assert_eq!(msgs[0].title.as_deref(), Some("broken build"));
    }

    #[test]
    fn parses_pull_request_description() {
        let a = adapter();
        let raw = r#"{
            "action": "opened",
            "pull_request": {
                "number": 7,
                "title": "add feature",
                "body": "@agent review this",
                "html_url": "http://forge.local:3000/a/b/pulls/7",
                "user": {"login": "u"}
            },
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("pull_request", Some(&sig));
        let msgs = a.handle(&h, body).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_pull_request);
        assert_eq!(msgs[0].number, Some(7));
        assert_eq!(
            msgs[0].location.as_str(),
            "http://forge.local:3000/a/b/pulls/7"
        );
    }

    #[test]
    fn ignores_closed_description_events() {
        let a = adapter();
        let raw = r#"{
            "action": "closed",
            "issue": {"number": 4, "body": "@agent x",
                      "html_url": "http://forge.local:3000/a/b/issues/4"},
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issues", Some(&sig));
        assert!(a.handle(&h, body).unwrap().is_empty());
    }
}
