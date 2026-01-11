use once_cell::sync::Lazy;
use regex::Regex;
use std::time::Duration;
use tracing::info;

/// Redact common secret patterns from audit logs.
///
/// This is intentionally conservative: audit logs are an escape hatch for
/// debugging production issues, but should be safe to enable without leaking
/// credentials.
fn sanitize_text(text: &str) -> String {
    static OPENAI_KEY_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"sk-[A-Za-z0-9_-]{16,}").expect("valid regex"));
    static AWS_ACCESS_KEY_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("valid regex"));
    static GITHUB_TOKEN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\bghp_[A-Za-z0-9]{30,}\b").expect("valid regex"));
    static BEARER_TOKEN_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)(bearer\s+)[A-Za-z0-9\-._=+/]{16,}").expect("valid regex")
    });
    static HEADER_VALUE_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\b(authorization|x-api-key|api-key)\s*:\s*([^\r\n]+)")
            .expect("valid regex")
    });
    static QUERY_PARAM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)([?&](?:key|api_key|apikey|token)=)([^&\s]+)").expect("valid regex")
    });
    static LONG_HEX_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\b[0-9a-fA-F]{32,}\b").expect("valid regex"));
    static LONG_BASE64ISH_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{32,}").expect("valid regex"));

    let mut out = text.to_string();

    out = HEADER_VALUE_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}: [REDACTED]", &caps[1])
        })
        .into_owned();
    out = QUERY_PARAM_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}[REDACTED]", &caps[1])
        })
        .into_owned();
    out = BEARER_TOKEN_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| format!("{}[REDACTED]", &caps[1]))
        .into_owned();

    for re in [
        &OPENAI_KEY_RE,
        &AWS_ACCESS_KEY_RE,
        &GITHUB_TOKEN_RE,
        &LONG_HEX_RE,
        &LONG_BASE64ISH_RE,
    ] {
        out = re.replace_all(&out, "[REDACTED]").into_owned();
    }

    out
}

pub(crate) fn sanitize_url_for_log(url: &url::Url) -> String {
    let mut safe = url.clone();
    safe.set_query(None);
    safe.set_fragment(None);
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    sanitize_text(safe.as_str())
}

/// Format a chat-style prompt for audit logging.
pub(crate) fn format_chat_prompt(messages: &[crate::types::ChatMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            crate::types::ChatRole::System => "system",
            crate::types::ChatRole::User => "user",
            crate::types::ChatRole::Assistant => "assistant",
        };
        out.push_str(role);
        out.push_str(": ");
        out.push_str(&message.content);
        out.push('\n');
    }
    out
}

pub(crate) fn sanitize_prompt_for_audit(prompt: &str) -> String {
    sanitize_text(prompt)
}

pub(crate) fn sanitize_completion_for_audit(completion: &str) -> String {
    sanitize_text(completion)
}

pub(crate) fn log_llm_request(
    provider: &str,
    model: &str,
    prompt: &str,
    endpoint: Option<&str>,
    attempt: usize,
    stream: bool,
) {
    let prompt = sanitize_prompt_for_audit(prompt);
    info!(
        target: nova_config::AI_AUDIT_TARGET,
        event = "llm_request",
        provider = provider,
        model = model,
        endpoint = endpoint,
        attempt = attempt,
        stream = stream,
        prompt = %prompt,
    );
}

pub(crate) fn log_llm_response(
    provider: &str,
    model: &str,
    completion: &str,
    latency: Duration,
    retry_count: usize,
    stream: bool,
    chunk_count: Option<usize>,
) {
    let completion = sanitize_completion_for_audit(completion);
    info!(
        target: nova_config::AI_AUDIT_TARGET,
        event = "llm_response",
        provider = provider,
        model = model,
        latency_ms = latency.as_millis() as u64,
        retry_count = retry_count,
        stream = stream,
        chunk_count = chunk_count,
        completion = %completion,
    );
}

pub(crate) fn log_llm_error(
    provider: &str,
    model: &str,
    error: &str,
    latency: Duration,
    retry_count: usize,
    stream: bool,
) {
    let error = sanitize_text(error);
    info!(
        target: nova_config::AI_AUDIT_TARGET,
        event = "llm_error",
        provider = provider,
        model = model,
        latency_ms = latency.as_millis() as u64,
        retry_count = retry_count,
        stream = stream,
        error = %error,
    );
}
