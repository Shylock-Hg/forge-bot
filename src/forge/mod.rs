//! Forge integrations.
//!
//! Every forge implements [`ForgeAdapter`]. The adapter is responsible for
//! verifying a webhook and normalizing its payload into a [`ForgeMessage`].
//! Everything downstream (mention detection, policy, agent selection) is forge
//! agnostic.

pub mod forgejo;
pub mod github;
pub mod gitlab;

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::Result;
use crate::location::ForgeKind;

/// A normalized comment / issue event received from a forge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeMessage {
    pub forge: ForgeKind,
    /// Canonical location of the object the comment belongs to.
    pub location: Url,
    /// Raw comment body (mention extraction happens later).
    pub body: String,
    /// Login of the comment author.
    pub author: String,
    /// `owner/repo`.
    pub repository: String,
    /// Id of the comment, when available.
    pub comment_id: Option<i64>,
    /// Issue / pull request number, when available.
    pub number: Option<u64>,
    /// True when the comment belongs to a pull request.
    pub is_pull_request: bool,
    /// Issue the pull request closes / fixes / resolves, when the description
    /// references one. This is internal routing metadata: it keeps a pull
    /// request and its issue on the same conversation, and is never sent to
    /// the agent.
    #[serde(default)]
    pub linked_issue: Option<u64>,
    /// The forge event name, e.g. `issue_comment`.
    pub event: String,
    /// Title of the issue / pull request, used to enrich the agent prompt.
    pub title: Option<String>,
}

impl ForgeMessage {
    /// Stable key identifying the conversation (thread) this message belongs
    /// to. Pull requests are folded onto their linked issue when one is known,
    /// so a pull request and the issue it closes share an agent/session. The
    /// key never leaves the gateway.
    pub fn conversation_key(&self) -> String {
        let number = if self.is_pull_request {
            self.linked_issue.or(self.number)
        } else {
            self.number
        };
        format!(
            "{}:{}:{}",
            self.forge,
            self.repository,
            number.unwrap_or_default()
        )
    }

    /// Repository identifier without the owner.
    pub fn repo_name(&self) -> &str {
        self.repository
            .split_once('/')
            .map(|(_, r)| r)
            .unwrap_or(&self.repository)
    }

    /// Owner part of the repository identifier.
    pub fn owner(&self) -> &str {
        self.repository
            .split_once('/')
            .map(|(o, _)| o)
            .unwrap_or_default()
    }
}

/// A forge webhook adapter.
#[async_trait::async_trait]
pub trait ForgeAdapter: Send + Sync {
    /// The forge this adapter serves.
    fn kind(&self) -> ForgeKind;

    /// Short name used in the webhook route, e.g. `forgejo`.
    fn slug(&self) -> &'static str;

    /// Verify the request signature. Implementations must use a constant time
    /// comparison and must not leak the expected value.
    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()>;

    /// Extract the event name from the request headers.
    fn event(&self, headers: &HeaderMap) -> String;

    /// Parse a verified payload into zero or more messages.
    ///
    /// Returning an empty vector means "this event is not actionable".
    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>>;

    /// Convenience helper combining verification and parsing.
    fn handle(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>> {
        self.verify(headers, body)?;
        self.parse(headers, body)
    }
}

/// Compute the lowercase hex HMAC-SHA256 of `body` using `secret`.
pub fn hmac_sha256_hex(secret: &[u8], body: &[u8]) -> String {
    use hmac::{KeyInit, Mac};

    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time equality for two byte slices.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verify an HMAC-SHA256 hex signature header, tolerating the optional
/// `sha256=` prefix used by GitHub.
pub fn verify_hmac_sha256(secret: &[u8], body: &[u8], provided: &str) -> bool {
    let expected = hmac_sha256_hex(secret, body);
    let provided = provided.strip_prefix("sha256=").unwrap_or(provided);
    constant_time_eq(expected.as_bytes(), provided.trim().as_bytes())
}

/// Shared parser for the `issue_comment` shaped payload used by Forgejo, Gitea
/// and GitHub. It normalizes the payload into a [`ForgeMessage`].
pub(crate) fn parse_comment_payload(
    forge: ForgeKind,
    base_url: &str,
    body: &[u8],
    event: &str,
) -> Result<Vec<ForgeMessage>> {
    use serde_json::Value;

    let payload: Value = serde_json::from_slice(body)?;

    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("created");
    if !action.is_empty() && action != "created" {
        return Ok(Vec::new());
    }

    let comment = payload
        .get("comment")
        .ok_or_else(|| crate::error::BotError::InvalidPayload("missing `comment` object".into()))?;
    let Some(body_text) = comment.get("body").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    let issue = payload.get("issue").or_else(|| payload.get("pull_request"));
    let repository = payload.get("repository").ok_or_else(|| {
        crate::error::BotError::InvalidPayload("missing `repository` object".into())
    })?;

    let full_name = repository
        .get("full_name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::error::BotError::InvalidPayload("missing repository.full_name".into())
        })?;

    let author = comment
        .get("user")
        .or_else(|| payload.get("sender"))
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let is_pull_request = issue
        .and_then(|i| i.get("pull_request"))
        .map(|v| !v.is_null())
        .unwrap_or_else(|| event.contains("pull_request"));

    let number = issue
        .and_then(|i| i.get("number"))
        .and_then(Value::as_u64)
        .or_else(|| payload.get("number").and_then(Value::as_u64));

    let comment_id = comment.get("id").and_then(Value::as_i64);
    let title = issue
        .and_then(|i| i.get("title"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let linked_issue = if is_pull_request {
        issue
            .and_then(|i| i.get("body"))
            .and_then(Value::as_str)
            .and_then(linked_issue_number)
    } else {
        None
    };

    let location = build_location(
        base_url,
        repository,
        issue,
        comment,
        full_name,
        number,
        is_pull_request,
    )?;

    Ok(vec![ForgeMessage {
        forge,
        location,
        body: body_text.to_owned(),
        author,
        repository: full_name.to_owned(),
        comment_id,
        number,
        is_pull_request,
        linked_issue,
        event: event.to_owned(),
        title,
    }])
}

/// Parse an `issues` / `pull_request` payload where the mention is in the
/// issue or pull-request description rather than a comment.
pub(crate) fn parse_description_payload(
    forge: ForgeKind,
    base_url: &str,
    body: &[u8],
    event: &str,
) -> Result<Vec<ForgeMessage>> {
    use serde_json::Value;

    let payload: Value = serde_json::from_slice(body)?;
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("opened");
    // Only react to creation/edits; ignore close, label, milestone, ...
    if !matches!(action, "opened" | "created" | "edited" | "") {
        return Ok(Vec::new());
    }

    let object = payload.get("issue").or_else(|| payload.get("pull_request"));
    let Some(object) = object else {
        return Ok(Vec::new());
    };
    let Some(body_text) = object.get("body").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    if body_text.trim().is_empty() {
        return Ok(Vec::new());
    }

    let repository = payload.get("repository").ok_or_else(|| {
        crate::error::BotError::InvalidPayload("missing `repository` object".into())
    })?;
    let full_name = repository
        .get("full_name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::error::BotError::InvalidPayload("missing repository.full_name".into())
        })?;

    let is_pull_request = payload.get("pull_request").is_some() || event.contains("pull_request");
    let author = object
        .get("user")
        .or_else(|| payload.get("sender"))
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let number = object.get("number").and_then(Value::as_u64);
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let linked_issue = if is_pull_request {
        linked_issue_number(body_text)
    } else {
        None
    };

    let location = description_location(
        base_url,
        repository,
        object,
        full_name,
        number,
        is_pull_request,
    )?;

    Ok(vec![ForgeMessage {
        forge,
        location,
        body: body_text.to_owned(),
        author,
        repository: full_name.to_owned(),
        comment_id: None,
        number,
        is_pull_request,
        linked_issue,
        event: event.to_owned(),
        title,
    }])
}

/// Find the first issue referenced with a closing keyword
/// (`close(s|d)`, `fix(es|ed)`, `resolve(s|d)`) in a description.
///
/// Forgejo, GitHub and GitLab all recognize those keywords to link a pull
/// request to the issue it addresses; the gateway only needs the number so it
/// can keep both threads on one agent.
pub(crate) fn linked_issue_number(body: &str) -> Option<u64> {
    // Longest forms first so `closes` is not shadowed by `close`.
    const KEYWORDS: [&str; 9] = [
        "closes", "closed", "close", "fixes", "fixed", "fix", "resolves", "resolved", "resolve",
    ];

    let lower = body.to_ascii_lowercase();
    // Track the earliest reference in the text so `Fixes #3, closes #4`
    // resolves to 3 regardless of the order keywords are checked.
    let mut best: Option<(usize, u64)> = None;
    for keyword in KEYWORDS {
        let mut from = 0;
        while let Some(offset) = lower[from..].find(keyword) {
            let start = from + offset;
            let end = start + keyword.len();
            from = end;

            // Require a token boundary before the keyword.
            if start > 0 && lower.as_bytes()[start - 1].is_ascii_alphanumeric() {
                continue;
            }

            let mut rest = lower[end..].trim_start();
            rest = rest.strip_prefix(':').unwrap_or(rest).trim_start();
            let Some(rest) = rest.strip_prefix('#') else {
                continue;
            };
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(number) = digits.parse::<u64>() {
                if best.is_none_or(|(seen, _)| start < seen) {
                    best = Some((start, number));
                }
                // The first valid reference for this keyword wins.
                break;
            }
        }
    }
    best.map(|(_, number)| number)
}

fn description_location(
    base_url: &str,
    repository: &serde_json::Value,
    object: &serde_json::Value,
    full_name: &str,
    number: Option<u64>,
    is_pull_request: bool,
) -> Result<Url> {
    if let Some(url) = object
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Url::parse(s).ok())
    {
        return Ok(url);
    }

    let base = repository
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{base_url}/{full_name}"));
    let resource = if is_pull_request { "pulls" } else { "issues" };
    Url::parse(&format!("{base}/{resource}/{}", number.unwrap_or_default())).map_err(|e| {
        crate::error::BotError::InvalidLocation {
            location: full_name.to_owned(),
            reason: e.to_string(),
        }
    })
}

/// Build the canonical location URL for a comment.
fn build_location(
    base_url: &str,
    repository: &serde_json::Value,
    issue: Option<&serde_json::Value>,
    comment: &serde_json::Value,
    full_name: &str,
    number: Option<u64>,
    is_pull_request: bool,
) -> Result<Url> {
    if let Some(url) = comment
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Url::parse(s).ok())
    {
        return Ok(url);
    }

    if let Some(url) = issue
        .and_then(|i| i.get("html_url"))
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Url::parse(s).ok())
    {
        return Ok(url);
    }

    let repo_url = repository
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{base_url}/{full_name}"));
    let resource = if is_pull_request { "pulls" } else { "issues" };
    let number = number.unwrap_or_default();
    Url::parse(&format!("{repo_url}/{resource}/{number}")).map_err(|e| {
        crate::error::BotError::InvalidLocation {
            location: repo_url,
            reason: e.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 test case 1.
        let key = [0x0b; 20];
        let data = b"Hi There";
        assert_eq!(
            hmac_sha256_hex(&key, data),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn extracts_linked_issue_from_closing_keywords() {
        assert_eq!(linked_issue_number("Fixes #42"), Some(42));
        assert_eq!(linked_issue_number("closes: #7"), Some(7));
        assert_eq!(linked_issue_number("Resolved #100 and more"), Some(100));
        assert_eq!(linked_issue_number("This does not close anything"), None);
        // `closely` must not be mistaken for `close`.
        assert_eq!(linked_issue_number("closely #3"), None);
        // The first keyword wins.
        assert_eq!(linked_issue_number("Fixes #3, closes #4"), Some(3));
    }

    #[test]
    fn verify_accepts_prefix_and_rejects_tampering() {
        let secret = b"topsecret";
        let body = br#"{"action":"created"}"#;
        let sig = hmac_sha256_hex(secret, body);
        assert!(verify_hmac_sha256(secret, body, &sig));
        assert!(verify_hmac_sha256(secret, body, &format!("sha256={sig}")));
        assert!(!verify_hmac_sha256(secret, body, "deadbeef"));
        assert!(!verify_hmac_sha256(secret, b"other", &sig));
    }
}
