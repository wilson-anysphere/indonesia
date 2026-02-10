use once_cell::sync::Lazy;
use regex::Regex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::info;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

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
    static BEARER_TOKEN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)(bearer\s+)[A-Za-z0-9\-._=+/]{16,}").expect("valid regex"));
    static BASIC_TOKEN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)(basic\s+)[A-Za-z0-9\-._=+/]{16,}").expect("valid regex"));
    static HEADER_VALUE_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?i)(['"]?\b(?:authorization|x-[a-z0-9-]*api[-_]?key|api[-_]?key|access[_-]?token|token)\b['"]?)\s*:\s*([^\r\n]+)"#,
        )
        .expect("valid regex")
    });
    static ASSIGNMENT_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?i)\b(authorization|x-[a-z0-9-]*api[-_]?key|api[-_]?key|access[_-]?token|token)\b\s*=\s*([^\s\r\n]+)",
        )
        .expect("valid regex")
    });
    static QUERY_PARAM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)([?&](?:key|api[_-]?key|token|access[_-]?token|authorization)=)([^&\s]+)")
            .expect("valid regex")
    });
    static URL_USERINFO_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/@]+@").expect("valid regex"));
    static LONG_HEX_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\b[0-9a-fA-F]{32,}\b").expect("valid regex"));
    static LONG_BASE64ISH_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{32,}").expect("valid regex"));

    let mut out = text.to_string();

    out = URL_USERINFO_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}[REDACTED]@", &caps[1])
        })
        .into_owned();
    out = HEADER_VALUE_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}: [REDACTED]", &caps[1])
        })
        .into_owned();
    out = ASSIGNMENT_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}=[REDACTED]", &caps[1])
        })
        .into_owned();
    out = QUERY_PARAM_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}[REDACTED]", &caps[1])
        })
        .into_owned();
    out = BEARER_TOKEN_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}[REDACTED]", &caps[1])
        })
        .into_owned();
    out = BASIC_TOKEN_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}[REDACTED]", &caps[1])
        })
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

/// Sanitize an error string for non-audit tracing logs.
///
/// Some providers (or user-configured endpoints) embed credentials in request URLs (for example a
/// `?key=` query parameter).
/// `reqwest::Error`'s `Display` output can include the full URL, so emitting `%err` directly can
/// leak API keys into normal tracing logs.
///
/// This helper is intentionally conservative:
/// - Strips URL userinfo (`user:pass@`).
/// - Strips URL query + fragments (drops *all* query params, not just known keys).
/// - Redacts common secret/token patterns via the same rules as audit log sanitization.
pub(crate) fn sanitize_error_for_tracing(error: &str) -> String {
    sanitize_text(&sanitize_urls_for_tracing(error))
}

fn sanitize_urls_for_tracing(text: &str) -> String {
    static URL_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\b[a-z][a-z0-9+.-]*://[^\s]+").expect("valid regex"));

    URL_RE
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let matched = &caps[0];

            // `reqwest` error strings often wrap URLs in parentheses, e.g.
            // "... for url (https://...?...)".
            //
            // Our regex intentionally includes trailing punctuation like ')', which makes
            // `Url::parse` fail. Trim common trailing punctuation and re-add it after
            // sanitizing.
            let mut end = matched.len();
            while end > 0 {
                let byte = matched.as_bytes()[end - 1];
                match byte {
                    b')' | b']' | b'}' | b',' | b'.' | b';' | b':' | b'"' | b'\'' | b'>' => {
                        end -= 1;
                    }
                    _ => break,
                }
            }

            let (url_part, suffix) = matched.split_at(end);
            match url::Url::parse(url_part) {
                Ok(mut url) => {
                    url.set_query(None);
                    url.set_fragment(None);
                    let _ = url.set_username("");
                    let _ = url.set_password(None);
                    format!("{}{}", url.as_str(), suffix)
                }
                Err(_) => {
                    // Best-effort fallback for non-parseable URLs: strip userinfo and drop
                    // query/fragment so secrets can't be leaked.
                    //
                    // This intentionally trades fidelity for safety. We still run
                    // `sanitize_text` afterwards, but stripping the query here ensures we don't
                    // retain unknown query params in the output.
                    let mut sanitized = url_part.to_string();

                    if let Some(scheme_idx) = sanitized.find("://") {
                        let after_scheme = scheme_idx.saturating_add(3);
                        let rest = &sanitized[after_scheme..];
                        let authority_end_rel = rest
                            .find(|c| matches!(c, '/' | '?' | '#'))
                            .unwrap_or(rest.len());
                        if let Some(at_rel) = rest[..authority_end_rel].rfind('@') {
                            let at_abs = after_scheme.saturating_add(at_rel);
                            if at_abs < sanitized.len() {
                                sanitized.replace_range(after_scheme..=at_abs, "");
                            }
                        }
                    }

                    let cut = sanitized
                        .find(|c| matches!(c, '?' | '#'))
                        .unwrap_or(sanitized.len());
                    sanitized.truncate(cut);

                    format!("{}{}", sanitized, suffix)
                }
            }
        })
        .into_owned()
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
    request_id: u64,
    provider: &str,
    model: &str,
    prompt: &str,
    endpoint: Option<&str>,
    attempt: usize,
    stream: bool,
) {
    let prompt_len = prompt.len() as u64;
    let prompt = sanitize_prompt_for_audit(prompt);
    info!(
        target: nova_config::AI_AUDIT_TARGET,
        event = "llm_request",
        request_id = request_id,
        provider = provider,
        model = model,
        endpoint = endpoint,
        url = endpoint,
        prompt_len = prompt_len,
        attempt = attempt,
        stream = stream,
        prompt = %prompt,
    );
}

pub(crate) fn log_llm_response(
    request_id: u64,
    provider: &str,
    model: &str,
    endpoint: Option<&str>,
    completion: &str,
    latency: Duration,
    retry_count: usize,
    stream: bool,
    chunk_count: Option<usize>,
) {
    let completion_len = completion.len() as u64;
    let completion = sanitize_completion_for_audit(completion);
    info!(
        target: nova_config::AI_AUDIT_TARGET,
        event = "llm_response",
        request_id = request_id,
        provider = provider,
        model = model,
        endpoint = endpoint,
        url = endpoint,
        latency_ms = latency.as_millis() as u64,
        retry_count = retry_count,
        stream = stream,
        chunk_count = chunk_count,
        completion_len = completion_len,
        completion = %completion,
    );
}

pub(crate) fn log_llm_error(
    request_id: u64,
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
        request_id = request_id,
        provider = provider,
        model = model,
        latency_ms = latency.as_millis() as u64,
        retry_count = retry_count,
        stream = stream,
        error = %error,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_text_redacts_url_userinfo_and_tokens() {
        let input = r#"POST https://user:pass@example.com/path?access_token=abcd1234
GET https://example.com/path?authorization=abcd1234
Basic abcdefghijklmnop
{"api_key": "sh0rt"}
api_key=sh0rt2
api_key='sh0rt3'
sk-proj-012345678901234567890123456789"#;
        let out = sanitize_prompt_for_audit(input);
        assert!(!out.contains("user:pass@"));
        assert!(!out.contains("access_token=abcd1234"));
        assert!(!out.contains("authorization=abcd1234"));
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("sh0rt"));
        assert!(!out.contains("sh0rt2"));
        assert!(!out.contains("sh0rt3"));
        assert!(!out.contains("abcdefghijklmnop"));
        assert!(!out.contains("sk-proj-012345678901234567890123456789"));
    }

    #[test]
    fn sanitize_error_for_tracing_strips_query_even_when_url_parse_fails() {
        let secret = "sk-verysecret-012345678901234567890123456789";
        let input =
            format!("http error for url (https://user:pass@example.com/pa%ZZth?key={secret})");

        let out = sanitize_error_for_tracing(&input);
        assert!(!out.contains(secret));
        assert!(!out.contains("user:pass@"));
        assert!(!out.contains("?key="));
        assert!(out.contains("https://example.com/pa%ZZth"));
    }
}
