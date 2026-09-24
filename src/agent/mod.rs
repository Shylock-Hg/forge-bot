//! Coding agent adapters.
//!
//! The gateway knows nothing about how an agent works: it builds an
//! [`AgentRequest`] from the webhook and hands it to an [`Agent`]. Adapters for
//! concrete CLIs (Codex, Pi, Claude Code, Kimi, ...) live in submodules.

pub mod claude;
pub mod codex;
pub mod command;
pub mod kimi;
pub mod pi;
pub mod pi_rpc;
pub mod registry;

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::Result;
use crate::location::ForgeKind;

pub use registry::AgentRegistry;

/// The only thing the gateway sends to an agent, exactly as in the design:
/// where the request came from and what was asked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    pub location: Url,
    pub message: String,
}

/// Additional, non-authoritative context handed to an adapter. It is kept
/// separate from [`AgentRequest`] because the request is the stable contract,
/// while this is convenience metadata.
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    pub workspace: PathBuf,
    pub forge: Option<ForgeKind>,
    pub repository: String,
    pub issue_number: Option<u64>,
    pub is_pull_request: bool,
    pub title: Option<String>,
    /// Environment variables carrying forge credentials.
    pub credentials: Vec<(String, String)>,
}

impl AgentContext {
    /// Variables exported to the agent process.
    pub fn environment(&self, request: &AgentRequest) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "FORGE_BOT_LOCATION".to_owned(),
                request.location.to_string(),
            ),
            ("FORGE_BOT_MESSAGE".to_owned(), request.message.clone()),
            (
                "FORGE_BOT_WORKSPACE".to_owned(),
                self.workspace.display().to_string(),
            ),
            ("FORGE_BOT_REPOSITORY".to_owned(), self.repository.clone()),
        ];

        if let Some(forge) = self.forge {
            env.push(("FORGE_BOT_FORGE".to_owned(), forge.as_str().to_owned()));
        }
        if let Some(number) = self.issue_number {
            env.push(("FORGE_BOT_ISSUE_NUMBER".to_owned(), number.to_string()));
        }
        env.push((
            "FORGE_BOT_IS_PULL_REQUEST".to_owned(),
            self.is_pull_request.to_string(),
        ));

        env.extend(self.credentials.iter().cloned());
        env
    }
}

/// What an agent reports back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutcome {
    pub success: bool,
    pub summary: String,
    pub duration: Duration,
}

impl AgentOutcome {
    pub fn success(summary: impl Into<String>, duration: Duration) -> Self {
        Self {
            success: true,
            summary: summary.into(),
            duration,
        }
    }

    pub fn failure(summary: impl Into<String>, duration: Duration) -> Self {
        Self {
            success: false,
            summary: summary.into(),
            duration,
        }
    }
}

/// A coding agent.
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    /// Adapter name, e.g. `codex`.
    fn name(&self) -> &str;

    /// Run the agent for one request. Implementations should be idempotent and
    /// must not panic on agent failure.
    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_contains_request_and_credentials() {
        let request = AgentRequest {
            location: Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "do it".into(),
        };
        let ctx = AgentContext {
            workspace: PathBuf::from("/tmp/ws"),
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(1),
            is_pull_request: false,
            title: None,
            credentials: vec![("FORGEJO_TOKEN".into(), "secret".into())],
        };
        let env = ctx.environment(&request);
        assert!(
            env.iter()
                .any(|(k, v)| k == "FORGEJO_TOKEN" && v == "secret")
        );
        assert!(env.iter().any(|(k, _)| k == "FORGE_BOT_LOCATION"));
        assert!(
            env.iter()
                .any(|(k, v)| k == "FORGE_BOT_ISSUE_NUMBER" && v == "1")
        );
    }
}
