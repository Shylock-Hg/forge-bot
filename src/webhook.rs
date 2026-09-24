//! HTTP webhook receiver.
//!
//! One route per forge accepts the raw webhook, delegates verification and
//! parsing to the matching [`ForgeAdapter`], extracts `@agent` mentions, and
//! hands actionable jobs to the [`Dispatcher`].

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::agent::AgentRegistry;
use crate::config::Config;
use crate::error::BotError;
use crate::forge::ForgeAdapter;
use crate::mention::extract_mention;
use crate::session::Dispatcher;

/// Shared state for the webhook server.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub adapters: Arc<std::collections::HashMap<String, Arc<dyn ForgeAdapter>>>,
    pub agents: Arc<AgentRegistry>,
    pub dispatcher: Arc<Dispatcher>,
    dedupe: Arc<Mutex<RecentComments>>,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        adapters: std::collections::HashMap<String, Arc<dyn ForgeAdapter>>,
        agents: AgentRegistry,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            config,
            adapters: Arc::new(adapters),
            agents: Arc::new(agents),
            dispatcher,
            dedupe: Arc::new(Mutex::new(RecentComments::new(1024))),
        }
    }

    fn seen(&self, key: &str) -> bool {
        self.dedupe
            .lock()
            .expect("dedupe mutex poisoned")
            .insert(key)
    }
}

/// A small bounded set remembering recently handled comments.
struct RecentComments {
    order: VecDeque<String>,
    set: HashSet<String>,
    capacity: usize,
}

impl RecentComments {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
            capacity,
        }
    }

    /// Returns true when the key was already present.
    fn insert(&mut self, key: &str) -> bool {
        if self.set.contains(key) {
            return true;
        }
        self.set.insert(key.to_owned());
        self.order.push_back(key.to_owned());
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        false
    }
}

/// Build the axum router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/webhooks/{forge}", post(receive))
        .route("/webhook/{forge}", post(receive))
        .with_state(state)
}

async fn root(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "name": "forge-bot",
        "version": env!("CARGO_PKG_VERSION"),
        "forges": state.adapters.keys().collect::<Vec<_>>(),
        "agents": state.agents.names(),
        "mention": state.config.trigger(),
    }))
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// Handle a webhook delivery.
async fn receive(
    State(state): State<AppState>,
    Path(forge): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(adapter) = state.adapters.get(&forge).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown forge `{forge}`") })),
        );
    };

    // Verify and parse.
    let messages = match adapter.handle(&headers, &body) {
        Ok(messages) => messages,
        Err(BotError::Verification(reason)) => {
            tracing::warn!(forge = %forge, %reason, "rejected webhook");
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": reason })));
        }
        Err(error) => {
            tracing::warn!(forge = %forge, %error, "failed to handle webhook");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.to_string() })),
            );
        }
    };

    let mut accepted = 0usize;
    for message in messages {
        // Never react to our own comments.
        if state.dispatcher.policy().is_ignored(&message.author) {
            continue;
        }

        let Some(mention) = extract_mention(&message.body, state.config.trigger()) else {
            continue;
        };

        if let Some(comment_id) = message.comment_id {
            let dedupe_key = format!("{}:{}:{comment_id}", message.forge, message.repository);
            if state.seen(&dedupe_key) {
                tracing::debug!(%dedupe_key, "ignoring duplicate webhook delivery");
                continue;
            }
        }

        let agent_name = mention
            .agent
            .clone()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| state.config.default_agent.clone());

        match state.dispatcher.submit(message, mention, &agent_name).await {
            Ok(job_id) => {
                accepted += 1;
                tracing::info!(%job_id, agent = %agent_name, "accepted trigger");
            }
            Err(BotError::Unauthorized(reason)) => {
                tracing::info!(%reason, "ignored unauthorized trigger");
            }
            Err(BotError::UnknownAgent(name)) => {
                tracing::warn!(agent = %name, "trigger referenced an unknown agent");
            }
            Err(error) => {
                tracing::error!(%error, "failed to enqueue job");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": error.to_string() })),
                );
            }
        }
    }

    (StatusCode::ACCEPTED, Json(json!({ "accepted": accepted })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_remembers_and_evicts() {
        let mut recent = RecentComments::new(2);
        assert!(!recent.insert("a"));
        assert!(recent.insert("a"));
        assert!(!recent.insert("b"));
        assert!(!recent.insert("c"));
        assert!(!recent.insert("a")); // evicted
    }
}
