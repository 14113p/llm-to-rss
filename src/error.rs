use thiserror::Error;

/// Top-level error type for `llm-to-rss`.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("feed '{0}' not found")]
    FeedNotFound(String),

    #[error("source fetch error for '{url}': {message}")]
    Fetch { url: String, message: String },

    #[error("LLM provider '{provider}' error: {message}")]
    Provider { provider: String, message: String },

    #[error("database error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(String),
}

pub type Result<T> = std::result::Result<T, AppError>;
