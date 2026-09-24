//! Repository checkout management.
//!
//! Before invoking an agent we can prepare a working copy of the repository so
//! that CLI agents start inside the code they are asked to change. Preparing a
//! workspace is optional; with `workspace.enabled = false` agents run in an
//! empty directory and are expected to clone/access the forge themselves.

use std::path::{Path, PathBuf};

use tokio::process::Command;
use url::Url;

use crate::config::WorkspaceConfig;
use crate::error::{BotError, Result};
use crate::forge::ForgeMessage;
use crate::location::ForgeKind;

/// Creates and updates per-issue checkouts.
#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    enabled: bool,
    reuse: bool,
    root: PathBuf,
    git_author_name: String,
    git_author_email: String,
}

impl WorkspaceManager {
    pub fn new(config: &WorkspaceConfig) -> Self {
        Self {
            enabled: config.enabled,
            reuse: config.reuse,
            root: crate::config::expand_tilde(&config.root),
            git_author_name: config.git_author_name.clone(),
            git_author_email: config.git_author_email.clone(),
        }
    }

    /// Path used for a message, whether or not it exists yet.
    pub fn path_for(&self, message: &ForgeMessage) -> PathBuf {
        let owner = sanitize(message.owner());
        let repo = sanitize(message.repo_name());
        let suffix = message
            .number
            .map(|n| format!("-{n}"))
            .unwrap_or_else(|| "-latest".to_owned());
        self.root.join(format!("{owner}__{repo}{suffix}"))
    }

    /// Ensure a checkout exists for `message` and return its path.
    pub async fn prepare(
        &self,
        message: &ForgeMessage,
        credentials: &[(String, String)],
    ) -> Result<PathBuf> {
        let dir = self.path_for(message);
        tokio::fs::create_dir_all(&dir).await?;

        if !self.enabled {
            return Ok(dir);
        }

        let clone_url = authenticated_clone_url(message, credentials)?;

        if !dir.join(".git").exists() {
            tracing::info!(dir = %dir.display(), repo = %message.repository, "cloning repository");
            run_git(
                dir.parent().unwrap_or(Path::new(".")),
                &["clone", clone_url.as_str(), dir.to_string_lossy().as_ref()],
            )
            .await
            .map_err(|e| BotError::Agent {
                name: "workspace".into(),
                reason: format!("git clone failed: {e}"),
            })?;
        } else if self.reuse {
            tracing::info!(dir = %dir.display(), "updating existing workspace");
            // Best effort: a dirty workspace or a detached HEAD must not stop
            // the job.
            if let Err(e) = run_git(&dir, &["fetch", "--all", "--prune"]).await {
                tracing::warn!(error = %e, "git fetch failed");
            }
        }

        // Make sure the agent can create commits.
        let _ = run_git(&dir, &["config", "user.name", &self.git_author_name]).await;
        let _ = run_git(&dir, &["config", "user.email", &self.git_author_email]).await;
        // Never store credentials on disk: clone with them, then strip them.
        let clean_url = clean_clone_url(message);
        let _ = run_git(&dir, &["remote", "set-url", "origin", &clean_url]).await;

        Ok(dir)
    }
}

/// Build a clone URL with credentials embedded for a single fetch.
fn authenticated_clone_url(
    message: &ForgeMessage,
    credentials: &[(String, String)],
) -> Result<Url> {
    let mut url = Url::parse(&clean_clone_url(message)).map_err(|e| BotError::InvalidLocation {
        location: message.repository.clone(),
        reason: e.to_string(),
    })?;

    let token = credential(credentials, message.forge);
    let Some(token) = token else {
        return Ok(url);
    };

    match message.forge {
        ForgeKind::GitHub => {
            let _ = url.set_username("x-access-token");
            let _ = url.set_password(Some(&token));
        }
        _ => {
            // Forgejo/Gitea/GitLab accept the token as the username.
            let _ = url.set_username(&token);
        }
    }
    Ok(url)
}

/// Clone URL without credentials.
fn clean_clone_url(message: &ForgeMessage) -> String {
    let mut base = message.location.clone();
    base.set_path("");
    base.set_query(None);
    base.set_fragment(None);
    let base = base.to_string();
    let base = base.trim_end_matches('/');
    format!("{base}/{}/{}.git", message.owner(), message.repo_name())
}

/// Pick the best credential for a forge from the exported variables.
fn credential(credentials: &[(String, String)], forge: ForgeKind) -> Option<String> {
    let keys: &[&str] = match forge {
        ForgeKind::Forgejo | ForgeKind::Gitea => &["FORGEJO_TOKEN", "FORGE_TOKEN"],
        ForgeKind::GitHub => &["GITHUB_TOKEN", "GH_TOKEN", "FORGE_TOKEN"],
        ForgeKind::GitLab => &["GITLAB_TOKEN", "FORGE_TOKEN"],
        ForgeKind::Unknown => &["FORGE_TOKEN"],
    };
    for key in keys {
        if let Some((_, value)) = credentials.iter().find(|(k, _)| k == key)
            && !value.is_empty()
        {
            return Some(value.clone());
        }
    }
    None
}

/// Replace any character that is unsafe in a path with `_`.
fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Run a git command, returning an error with stderr on failure.
async fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = format!("git {} failed: {}", args.join(" "), stderr.trim());
        let lower = stderr.to_lowercase();
        if stderr.contains("403")
            || stderr.contains("401")
            || lower.contains("authentication failed")
            || lower.contains("could not read username")
            || lower.contains("permission denied")
        {
            Err(BotError::ForgePermissionDenied(message))
        } else {
            Err(BotError::Other(anyhow::anyhow!(message)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local:3000/Org/My-Repo/issues/3").unwrap(),
            body: "@agent x".into(),
            author: "u".into(),
            repository: "Org/My-Repo".into(),
            comment_id: None,
            number: Some(3),
            is_pull_request: false,
            linked_issue: None,
            event: "issue_comment".into(),
            title: None,
        }
    }

    #[test]
    fn builds_path_safely() {
        let manager = WorkspaceManager::new(&WorkspaceConfig {
            root: PathBuf::from("/tmp/ws"),
            ..Default::default()
        });
        let path = manager.path_for(&msg());
        assert_eq!(path, PathBuf::from("/tmp/ws/Org__My-Repo-3"));
    }

    #[test]
    fn builds_clean_clone_url() {
        assert_eq!(
            clean_clone_url(&msg()),
            "http://forge.local:3000/Org/My-Repo.git"
        );
    }

    #[test]
    fn embeds_token_for_forgejo() {
        let creds = vec![("FORGEJO_TOKEN".to_string(), "abc123".to_string())];
        let url = authenticated_clone_url(&msg(), &creds).unwrap();
        assert_eq!(url.username(), "abc123");
    }

    #[test]
    fn embeds_token_for_github() {
        let mut m = msg();
        m.forge = ForgeKind::GitHub;
        let creds = vec![("GITHUB_TOKEN".to_string(), "ghp_x".to_string())];
        let url = authenticated_clone_url(&m, &creds).unwrap();
        assert_eq!(url.username(), "x-access-token");
        assert_eq!(url.password(), Some("ghp_x"));
    }
}
