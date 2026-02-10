use std::sync::Arc;
use std::{error::Error, fmt};

#[derive(Clone)]
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

impl fmt::Debug for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn sanitize(text: &str) -> String {
            crate::audit::sanitize_error_for_tracing(text)
        }

        match self {
            AiError::Http(err) => f
                .debug_struct("AiError::Http")
                .field("message", &sanitize(&err.to_string()))
                .field("status", &err.status().map(|s| s.as_u16()))
                .field("is_timeout", &err.is_timeout())
                .field("is_connect", &err.is_connect())
                .finish(),
            AiError::Json(err) => f
                .debug_struct("AiError::Json")
                .field("message", &sanitize(&err.to_string()))
                .finish(),
            AiError::Url(err) => f.debug_tuple("AiError::Url").field(err).finish(),
            AiError::InvalidConfig(msg) => f
                .debug_struct("AiError::InvalidConfig")
                .field("message", &sanitize(msg))
                .finish(),
            AiError::Timeout => f.write_str("AiError::Timeout"),
            AiError::Cancelled => f.write_str("AiError::Cancelled"),
            AiError::UnexpectedResponse(msg) => f
                .debug_struct("AiError::UnexpectedResponse")
                .field("message", &sanitize(msg))
                .finish(),
        }
    }
}

impl Error for AiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AiError::Url(err) => Some(err),
            // Do not expose reqwest/serde_json errors via `source()`: their `Display`/`Debug`
            // output can include unsanitized content (notably request URLs with embedded
            // credentials), and many error reporters include source chains in user-facing output.
            AiError::Http(_)
            | AiError::Json(_)
            | AiError::InvalidConfig(_)
            | AiError::Timeout
            | AiError::Cancelled
            | AiError::UnexpectedResponse(_) => None,
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

        let debug = format!("{ai_err:?}");
        assert!(
            !debug.contains(secret),
            "AiError debug leaked secret token: {debug}"
        );
        assert!(
            !debug.contains("?key="),
            "AiError debug should strip URL query params: {debug}"
        );
        assert!(
            ai_err.source().is_none(),
            "AiError::Http should not expose reqwest::Error via source()"
        );
    }
}
