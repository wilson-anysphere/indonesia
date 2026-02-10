use std::sync::Arc;
use std::{error::Error, fmt};

#[derive(Debug, Clone)]
pub enum AiError {
    Http(Arc<reqwest::Error>),
    Json(Arc<serde_json::Error>),
    Url(url::ParseError),
    InvalidConfig(String),
    Timeout,
    Cancelled,
    UnexpectedResponse(String),
}

impl From<reqwest::Error> for AiError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            Self::Timeout
        } else {
            Self::Http(Arc::new(err))
        }
    }
}

impl From<serde_json::Error> for AiError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(Arc::new(err))
    }
}

impl From<url::ParseError> for AiError {
    fn from(err: url::ParseError) -> Self {
        Self::Url(err)
    }
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn sanitize(text: &str) -> String {
            // `reqwest::Error` display strings frequently embed request URLs. Some providers encode
            // credentials as URL query parameters (e.g. Gemini's `?key=`). Use the same
            // best-effort URL stripping + token redaction logic we apply to tracing logs so
            // `AiError::to_string()` is safe to surface to end users.
            crate::audit::sanitize_error_for_tracing(text)
        }

        match self {
            AiError::Http(err) => {
                let sanitized = sanitize(&err.to_string());
                write!(f, "http error: {sanitized}")
            }
            AiError::Json(err) => {
                let sanitized = sanitize(&err.to_string());
                write!(f, "json error: {sanitized}")
            }
            AiError::Url(err) => write!(f, "url error: {err}"),
            AiError::InvalidConfig(msg) => {
                let sanitized = sanitize(msg);
                write!(f, "invalid config: {sanitized}")
            }
            AiError::Timeout => f.write_str("request timed out"),
            AiError::Cancelled => f.write_str("request cancelled"),
            AiError::UnexpectedResponse(msg) => {
                let sanitized = sanitize(msg);
                write!(f, "unexpected response: {sanitized}")
            }
        }
    }
}

impl Error for AiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AiError::Http(err) => Some(err.as_ref()),
            AiError::Json(err) => Some(err.as_ref()),
            AiError::Url(err) => Some(err),
            AiError::InvalidConfig(_) | AiError::Timeout | AiError::Cancelled | AiError::UnexpectedResponse(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn ai_error_display_sanitizes_reqwest_error_urls() {
        let secret = "sk-verysecret-012345678901234567890123456789";

        // Bind an ephemeral port, then close it. The resulting connection attempt should fail
        // quickly with ECONNREFUSED without requiring external network access.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let port = listener.local_addr().expect("listener addr").port();
        drop(listener);

        let url = format!("http://127.0.0.1:{port}/path?key={secret}&other=1");
        let client = reqwest::Client::new();
        let reqwest_err = client
            .get(url)
            .timeout(Duration::from_millis(200))
            .send()
            .await
            .expect_err("expected request to fail");

        let ai_err = AiError::from(reqwest_err);
        assert!(
            matches!(ai_err, AiError::Http(_)),
            "expected AiError::Http from connection failure, got {ai_err:?}"
        );
        let message = ai_err.to_string();

        assert!(
            !message.contains(secret),
            "AiError display leaked secret token: {message}"
        );
        assert!(
            !message.contains("?key="),
            "AiError display should strip URL query params: {message}"
        );
    }
}
