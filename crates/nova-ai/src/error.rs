use thiserror::Error;

#[derive(Debug, Error)]
pub enum AiError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
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
