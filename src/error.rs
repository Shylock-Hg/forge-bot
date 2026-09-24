//! Error type shared across the bot.

use thiserror::Error;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, BotError>;

/// Every fallible operation in the bot returns this error.
#[derive(Debug, Error)]
pub enum BotError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("webhook verification failed: {0}")]
    Verification(String),

    #[error("unsupported forge: {0}")]
    UnsupportedForge(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("invalid payload: {0}")]
    InvalidPayload(String),

    #[error("could not parse location {location:?}: {reason}")]
    InvalidLocation { location: String, reason: String },

    #[error("agent `{0}` is not configured")]
    UnknownAgent(String),

    #[error("agent `{name}` failed: {reason}")]
    Agent { name: String, reason: String },

    #[error("forge api error: {0}")]
    ForgeApi(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
