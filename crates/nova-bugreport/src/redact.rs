use regex::Regex;
use std::sync::OnceLock;

const REDACTION: &str = "<redacted>";

pub(crate) fn redact_string(input: &str) -> String {
    let mut out = input.to_owned();
    out = redact_urls(&out);
    out = redact_bearer_tokens(&out);
    out = redact_sensitive_header_values(&out);
    out = redact_api_keys(&out);
    out = redact_sensitive_kv_pairs(&out);
    out
}

fn redact_urls(input: &str) -> String {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    let re = URL_RE.get_or_init(|| {
        // Use a raw string literal with `#` so the pattern can include `"` in the
        // character class without escaping.
        Regex::new(r#"(?i)\b[a-z][a-z0-9+.-]*://[^\s"'<>]+"#).expect("URL regex should compile")
    });

    re.replace_all(input, |caps: &regex::Captures<'_>| {
        sanitize_url(caps.get(0).unwrap().as_str())
    })
    .into_owned()
}

fn redact_bearer_tokens(input: &str) -> String {
    static BEARER_RE: OnceLock<Regex> = OnceLock::new();
    let re = BEARER_RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(['"]?\bauthorization\b['"]?\s*[:=]\s*['"]?\s*(?:bearer|basic)\s+)([^\s"']+)"#,
        )
            .expect("bearer regex should compile")
    });

    re.replace_all(input, format!("$1{REDACTION}")).into_owned()
}

fn redact_sensitive_header_values(input: &str) -> String {
    static HEADER_RE: OnceLock<Regex> = OnceLock::new();
    let re = HEADER_RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(['"]?\b(?:x-[a-z0-9-]*api[-_]?key|api[-_]?key|access[_-]?token|x-[a-z0-9-]*token|token)\b['"]?)\s*:\s*([^,}\r\n]+)"#,
        )
        .expect("header regex should compile")
    });

    re.replace_all(input, format!("$1: {REDACTION}")).into_owned()
}

fn redact_api_keys(input: &str) -> String {
    static SK_RE: OnceLock<Regex> = OnceLock::new();
    static AIZA_RE: OnceLock<Regex> = OnceLock::new();
    let sk_re =
        SK_RE.get_or_init(|| Regex::new(r"sk-[A-Za-z0-9]{10,}").expect("sk regex should compile"));
    let aiza_re = AIZA_RE
        .get_or_init(|| Regex::new(r"AIza[0-9A-Za-z\-_]{10,}").expect("aiza regex should compile"));

    let out = sk_re.replace_all(input, REDACTION);
    aiza_re.replace_all(&out, REDACTION).into_owned()
}

fn redact_sensitive_kv_pairs(input: &str) -> String {
    static KV_RE: OnceLock<Regex> = OnceLock::new();
    let re = KV_RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b((?:key|token|access_token|api_key|apikey|authorization)\s*=\s*)([^\s&;]+)",
        )
        .expect("kv regex should compile")
    });

    re.replace_all(input, format!("$1{REDACTION}")).into_owned()
}

fn sanitize_url(url: &str) -> String {
    let Some(scheme_idx) = url.find("://") else {
        return url.to_owned();
    };

    let (scheme, rest) = url.split_at(scheme_idx + 3);

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);

    let authority = if let Some(at_pos) = authority.rfind('@') {
        let host = &authority[at_pos + 1..];
        format!("{REDACTION}@{host}")
    } else {
        authority.to_owned()
    };

    let tail = sanitize_url_tail(tail);
    format!("{scheme}{authority}{tail}")
}

fn sanitize_url_tail(tail: &str) -> String {
    let (before_fragment, has_fragment) = match tail.find('#') {
        Some(pos) => (&tail[..pos], true),
        None => (tail, false),
    };

    let sanitized = match before_fragment.find('?') {
        Some(q_pos) => {
            let (before_q, after_q) = before_fragment.split_at(q_pos + 1);
            let query = &after_q;
            let sanitized_query = sanitize_query(query);
            format!("{before_q}{sanitized_query}")
        }
        None => before_fragment.to_owned(),
    };

    if has_fragment {
        format!("{sanitized}#{REDACTION}")
    } else {
        sanitized
    }
}

fn sanitize_query(query: &str) -> String {
    let mut out = String::new();
    for (idx, part) in query.split('&').enumerate() {
        if idx > 0 {
            out.push('&');
        }
        if part.is_empty() {
            continue;
        }

        match part.split_once('=') {
            Some((key, _value)) => {
                out.push_str(key);
                out.push('=');
                // Be conservative: query parameters often contain secrets under arbitrary keys.
                out.push_str(REDACTION);
            }
            None => {
                out.push_str(part);
                out.push('=');
                out.push_str(REDACTION);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_urls_and_sensitive_query_params() {
        let input = "GET https://user:pass@example.com/path?token=abc&other=1";
        let out = redact_string(input);

        assert!(out.contains(
            "https://<redacted>@example.com/path?token=<redacted>&other=<redacted>"
        ));
        assert!(!out.contains("user:pass"));
        assert!(!out.contains("token=abc"));
    }

    #[test]
    fn redacts_url_fragments() {
        let input = "GET https://example.com/path#access_token=abc123";
        let out = redact_string(input);

        assert!(out.contains("https://example.com/path#<redacted>"));
        assert!(!out.contains("abc123"));
    }

    #[test]
    fn redacts_bearer_tokens() {
        let input = "Authorization: Bearer abc.def.ghi";
        let out = redact_string(input);

        assert_eq!(out, "Authorization: Bearer <redacted>");
    }

    #[test]
    fn redacts_basic_tokens() {
        let input = "Authorization: Basic dXNlcjpwYXNz";
        let out = redact_string(input);

        assert_eq!(out, "Authorization: Basic <redacted>");
    }

    #[test]
    fn redacts_api_key_headers() {
        let secret = "super-secret-api-key";
        let input = format!("x-goog-api-key: {secret}\napi-key: {secret}\ntoken: {secret}\n");
        let out = redact_string(&input);

        assert!(!out.contains(secret));
        assert!(out.contains("x-goog-api-key: <redacted>"));
        assert!(out.contains("api-key: <redacted>"));
        assert!(out.contains("token: <redacted>"));
    }

    #[test]
    fn redacts_json_style_authorization_headers() {
        let secret = "SUPERSECRET-TOKEN";
        let input = format!(r#"{{"authorization": "Bearer {secret}"}}"#);
        let out = redact_string(&input);

        assert!(!out.contains(secret));
        assert!(out.contains(r#""authorization": "Bearer <redacted>""#));
    }

    #[test]
    fn redacts_json_style_key_value_pairs() {
        let secret = "super-secret-api-key";
        let input = format!(r#"{{"api_key": "{secret}", "other": 1}}"#);
        let out = redact_string(&input);

        assert!(!out.contains(secret));
        assert!(out.contains(r#""api_key": <redacted>"#));
        assert!(out.contains(r#""other": 1"#));
    }
}
