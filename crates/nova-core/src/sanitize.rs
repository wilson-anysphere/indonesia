/// Best-effort sanitizer for `serde` / `serde_json` error display strings.
///
/// `serde_json::Error` messages sometimes include user-controlled scalar values, for example:
/// `invalid type: string "..."` or `unknown field `...`, expected ...`.
///
/// This helper conservatively redacts:
/// - all double-quoted substrings (handling escaped quotes), and
/// - the first backticked segment (typically the unknown field/variant),
///
/// while preserving the rest of the message so it remains actionable (line/column info, expected
/// field lists, etc).
///
/// This is intentionally string-based so callers can use it without depending on `serde_json`.
#[must_use]
pub fn sanitize_json_error_message(message: &str) -> String {
    // Conservatively redact all double-quoted substrings. This keeps the error actionable (it
    // retains the overall structure + line/column info) without echoing potentially-sensitive
    // content embedded in strings.
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let mut end = None;
        let bytes = rest.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if b != b'"' {
                continue;
            }

            // Treat quotes preceded by an odd number of backslashes as escaped.
            let mut backslashes = 0usize;
            let mut k = idx;
            while k > 0 && bytes[k - 1] == b'\\' {
                backslashes += 1;
                k -= 1;
            }
            if backslashes % 2 == 0 {
                end = Some(idx);
                break;
            }
        }

        let Some(end) = end else {
            // Unterminated quote: redact the remainder and stop.
            out.push_str("<redacted>");
            rest = "";
            break;
        };
        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Only redact the first backticked segment for unknown field/variant errors, because that
    // segment can contain user-controlled content (the unknown key/variant).
    //
    // Other serde diagnostics (e.g. `missing field `foo``) also use backticks, but those refer to
    // schema field names and are safe + useful to keep.
    let start = ["unknown field `", "unknown variant `"]
        .iter()
        .filter_map(|pattern| out.find(pattern).map(|pos| pos + pattern.len().saturating_sub(1)))
        .min();
    if let Some(start) = start {
        let after_start = &out[start.saturating_add(1)..];
        let end = if let Some(end_rel) = after_start.rfind("`, expected") {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else if let Some(end_rel) = after_start.rfind('`') {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else {
            None
        };
        if let Some(end) = end {
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_json_error_message_redacts_quoted_string_values() {
        let secret_suffix = "nova-core-secret-token";
        let message = format!(
            r#"invalid type: string "prefix\"{secret_suffix}", expected boolean"#,
        );

        let sanitized = sanitize_json_error_message(&message);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected sanitized message to omit string values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected sanitized message to include redaction marker: {sanitized}"
        );
    }

    #[test]
    fn sanitize_json_error_message_redacts_backticked_unknown_fields() {
        let secret_suffix = "nova-core-backtick-secret";
        let message = format!("unknown field `{secret_suffix}`, expected foo, bar");

        let sanitized = sanitize_json_error_message(&message);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected sanitized message to omit backticked values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected sanitized message to include redaction marker: {sanitized}"
        );
        assert!(
            sanitized.contains("expected foo, bar"),
            "expected sanitized message to preserve expected list: {sanitized}"
        );
    }

    #[test]
    fn sanitize_json_error_message_redacts_backticked_values_with_embedded_backticks() {
        // Some callers feed sanitized `anyhow` chains or wrapper errors into this helper, and the
        // offending field/variant name can itself contain backticks + `, expected` substrings.
        //
        // When that happens we still want to redact the *entire* offending segment without
        // accidentally stopping at the first internal backtick.
        let secret_suffix = "nova-core-embedded-backtick-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let message = format!("unknown field `{secret}`, expected foo, bar");

        let sanitized = sanitize_json_error_message(&message);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected sanitized message to omit embedded backticked values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected sanitized message to include redaction marker: {sanitized}"
        );
        assert!(
            sanitized.contains("expected foo, bar"),
            "expected sanitized message to preserve expected list: {sanitized}"
        );
    }

    #[test]
    fn sanitize_json_error_message_preserves_missing_field_names() {
        // `missing field` errors refer to schema field names (not user-controlled values). Keep
        // these intact so invalid-params errors remain actionable for clients.
        let message = "missing field `textDocument`";
        let sanitized = sanitize_json_error_message(message);
        assert_eq!(sanitized, message);
    }
}
