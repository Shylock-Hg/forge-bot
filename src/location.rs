//! Parsing of forge URLs into a normalized [`ForgeLocation`].
//!
//! The gateway only ever forwards a location URL plus a message to the agent,
//! so the location is the single source of truth for "what are we looking at".
//! This module turns a URL such as
//!
//! ```text
//! https://forge.example.com/org/repo/pulls/123#issuecomment-456
//! ```
//!
//! into structured, forge agnostic data.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{BotError, Result};

/// Which forge a location belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    Forgejo,
    Gitea,
    GitHub,
    GitLab,
    Unknown,
}

impl ForgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ForgeKind::Forgejo => "forgejo",
            ForgeKind::Gitea => "gitea",
            ForgeKind::GitHub => "github",
            ForgeKind::GitLab => "gitlab",
            ForgeKind::Unknown => "unknown",
        }
    }

    /// Best-effort detection from a host name.
    pub fn from_host(host: &str) -> Self {
        let host = host.to_ascii_lowercase();
        if host.contains("github") {
            ForgeKind::GitHub
        } else if host.contains("gitlab") {
            ForgeKind::GitLab
        } else if host.contains("gitea") {
            ForgeKind::Gitea
        } else {
            // Forgejo instances are frequently self hosted without a telling
            // host name; default to the primary supported forge rather than
            // "unknown" so the adapter can be selected.
            ForgeKind::Forgejo
        }
    }
}

impl std::fmt::Display for ForgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The kind of object a location points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationKind {
    Issue,
    PullRequest,
    Repository,
    Other(String),
}

impl LocationKind {
    pub fn as_str(&self) -> &str {
        match self {
            LocationKind::Issue => "issue",
            LocationKind::PullRequest => "pull_request",
            LocationKind::Repository => "repository",
            LocationKind::Other(s) => s,
        }
    }
}

/// A normalized reference to a forge object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeLocation {
    pub forge: ForgeKind,
    /// Scheme + authority, e.g. `https://forge.example.com`.
    pub base_url: String,
    pub owner: String,
    pub repo: String,
    pub kind: LocationKind,
    /// Issue / pull request number, when applicable.
    pub number: Option<u64>,
    /// Comment id parsed from the fragment, when present.
    pub comment_id: Option<i64>,
}

impl ForgeLocation {
    /// Parse a location URL. Never fails on an unknown forge; instead the forge
    /// is reported as [`ForgeKind::Unknown`] so callers can decide.
    pub fn parse(url: &Url) -> Result<Self> {
        let segments: Vec<String> = url
            .path_segments()
            .map(|s| s.filter(|p| !p.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default();

        if segments.len() < 2 {
            return Err(BotError::InvalidLocation {
                location: url.to_string(),
                reason: "expected at least `/<owner>/<repo>` in the path".into(),
            });
        }

        let forge = ForgeKind::from_host(url.host_str().unwrap_or_default());
        let owner = segments[0].clone();
        let repo = segments[1].clone();

        // Strip a trailing `.git` for clone URLs.
        let repo = repo.strip_suffix(".git").unwrap_or(&repo).to_owned();

        let mut kind = LocationKind::Repository;
        let mut number = None;

        // GitLab puts a `-` separator before its resource segments, e.g.
        // `/org/repo/-/issues/1`. Skip it when looking for the resource.
        let resource = segments
            .iter()
            .position(|s| s == "-")
            .map(|i| i + 1)
            .unwrap_or(2);

        match segments.get(resource).map(String::as_str) {
            Some("issues") => {
                kind = LocationKind::Issue;
                number = segments.get(resource + 1).and_then(|s| s.parse().ok());
            }
            Some("pulls") | Some("pull") | Some("merge_requests") => {
                kind = LocationKind::PullRequest;
                number = segments.get(resource + 1).and_then(|s| s.parse().ok());
            }
            Some("repos") => {
                // Forgejo API style: /api/v1/repos/owner/repo/...
                kind = LocationKind::Repository;
            }
            Some(other) => {
                kind = LocationKind::Other(other.to_owned());
                number = segments.get(resource + 1).and_then(|s| s.parse().ok());
            }
            None => {}
        }

        let comment_id = parse_comment_fragment(url.fragment());

        Ok(Self {
            forge,
            base_url: base_url(url),
            owner,
            repo,
            kind,
            number,
            comment_id,
        })
    }

    /// `owner/repo`, the canonical repository identifier.
    pub fn repository(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// The URL to the issue / pull request without the comment fragment.
    pub fn issue_url(&self) -> Result<Url> {
        let mut url = Url::parse(&self.base_url).map_err(|e| BotError::InvalidLocation {
            location: self.base_url.clone(),
            reason: e.to_string(),
        })?;
        let number = self.number.unwrap_or_default();
        let resource = match self.kind {
            LocationKind::PullRequest => "pulls",
            _ => "issues",
        };
        url.set_path(&format!(
            "/{}/{}/{}/{}",
            self.owner, self.repo, resource, number
        ));
        url.set_fragment(None);
        Ok(url)
    }
}

/// Extract the base URL (scheme + authority) from a URL.
fn base_url(url: &Url) -> String {
    let mut base = url.clone();
    base.set_path("");
    base.set_query(None);
    base.set_fragment(None);
    base.to_string().trim_end_matches('/').to_owned()
}

/// Parse a comment id from a URL fragment.
///
/// Handles the common forms:
/// * `#issuecomment-123` / `#issuecomment-456`
/// * `#issuecomment-123456` (GitHub/Forgejo)
/// * `#note_123` (GitLab)
/// * `#comment-123`
pub fn parse_comment_fragment(fragment: Option<&str>) -> Option<i64> {
    let fragment = fragment?;
    let digits = fragment
        .rsplit(|c: char| !c.is_ascii_digit() && c != '-')
        .find(|s| !s.is_empty())?;
    digits.trim_start_matches('-').parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parses_forgejo_pull_with_comment() {
        let loc = ForgeLocation::parse(&u(
            "https://forge.example.com/org/repo/pulls/123#issuecomment-456",
        ))
        .unwrap();
        assert_eq!(loc.forge, ForgeKind::Forgejo);
        assert_eq!(loc.owner, "org");
        assert_eq!(loc.repo, "repo");
        assert_eq!(loc.kind, LocationKind::PullRequest);
        assert_eq!(loc.number, Some(123));
        assert_eq!(loc.comment_id, Some(456));
        assert_eq!(loc.repository(), "org/repo");
        assert_eq!(loc.base_url, "https://forge.example.com");
    }

    #[test]
    fn parses_issue_without_comment() {
        let loc = ForgeLocation::parse(&u("http://forge.local:3000/a/b/issues/7")).unwrap();
        assert_eq!(loc.number, Some(7));
        assert_eq!(loc.comment_id, None);
        assert_eq!(loc.kind, LocationKind::Issue);
    }

    #[test]
    fn parses_gitlab_merge_request_note() {
        let loc = ForgeLocation::parse(&u(
            "https://gitlab.example.com/group/proj/-/merge_requests/9#note_42",
        ))
        .unwrap();
        assert_eq!(loc.forge, ForgeKind::GitLab);
        assert_eq!(loc.kind, LocationKind::PullRequest);
        assert_eq!(loc.number, Some(9));
        assert_eq!(loc.comment_id, Some(42));
    }

    #[test]
    fn parses_github_pull_fragment() {
        let loc = ForgeLocation::parse(&u(
            "https://github.com/octocat/hello-world/pull/1#issuecomment-123",
        ))
        .unwrap();
        assert_eq!(loc.forge, ForgeKind::GitHub);
        assert_eq!(loc.kind, LocationKind::PullRequest);
        assert_eq!(loc.number, Some(1));
        assert_eq!(loc.comment_id, Some(123));
    }

    #[test]
    fn malformed_path_is_rejected() {
        let err = ForgeLocation::parse(&u("https://forge.example.com/onlyowner")).unwrap_err();
        assert!(matches!(err, BotError::InvalidLocation { .. }));
    }

    #[test]
    fn strips_git_suffix() {
        let loc = ForgeLocation::parse(&u("https://forge.example.com/org/repo.git")).unwrap();
        assert_eq!(loc.repo, "repo");
    }
}
