use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum AiError {
    #[error("http error: {0}")]
    Http(Arc<reqwest::Error>),
    #[error("json error: {0}")]
    Json(Arc<serde_json::Error>),
    #[error("url error: {0}")]
    Url(#[from] url::ParseError),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("request timed out")]
    Timeout,
    #[error("request cancelled")]
    Cancelled,
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
}

impl From<reqwest::Error> for AiError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(Arc::new(err))
    }
}

impl From<serde_json::Error> for AiError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(Arc::new(err))
    }
}
