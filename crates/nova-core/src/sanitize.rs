/// Best-effort sanitizer for `serde` / `serde_json` error display strings.
///
/// `serde_json::Error` messages sometimes include user-controlled scalar values, for example:
/// `invalid type: string "..."` or `unknown field `...`, expected ...`.
///
/// This helper conservatively redacts:
/// - all double-quoted substrings (handling escaped quotes), and
/// - backticked segments when they are known to contain user-controlled content (unknown
///   fields/variants or invalid type/value scalars),
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

    // `serde` uses backticks in a few different diagnostics:
    //
    // - `unknown field `secret`, expected ...` (user-controlled key → redact)
    // - `unknown variant `secret`, expected ...` (user-controlled variant → redact)
    // - `invalid type: integer `123`, expected ...` (user-controlled scalar → redact)
    // - `missing field `foo`` (schema field name → keep)
    // - `expected `,` or `}` at line ...` (parser expected tokens → keep)
    //
    // Redact only when the backticked segment is known to contain user-controlled content.
    let mut start = ["unknown field `", "unknown variant `"]
        .iter()
        .filter_map(|pattern| out.find(pattern).map(|pos| pos + pattern.len().saturating_sub(1)))
        .min();
    if start.is_none() && (out.contains("invalid type:") || out.contains("invalid value:")) {
        // `invalid type/value` errors include the unexpected scalar value before `, expected ...`.
        // Redact only backticked values in that prefix so we don't hide schema names or parser
        // expected-token diagnostics in the `expected` portion.
        let boundary = out.find(", expected").unwrap_or(out.len());
        start = out[..boundary].find('`');
        if start.is_none() && boundary == out.len() {
            // Some serde errors omit the `, expected ...` suffix. Fall back to the first backtick.
            start = out.find('`');
        }
    }
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

/// Best-effort sanitizer for `toml` error messages.
///
/// `toml::de::Error::message()` avoids the default `Display` output (which includes an offending
/// source snippet) but can still embed user-controlled scalar values, for example:
/// - `invalid type: string "..."` or
/// - `invalid semver version '...'`.
///
/// This helper conservatively redacts:
/// - all double-quoted substrings (via [`sanitize_json_error_message`]),
/// - all single-quoted substrings (handling escaped quotes), and
/// - backticked segments when they are known to contain user-controlled content (unknown
///   fields/variants or invalid type/value scalars) (via [`sanitize_json_error_message`]),
///
/// while preserving the rest of the message so it remains actionable (line/column info, expected
/// field lists, etc).
///
/// This is intentionally string-based so callers can use it without depending on `toml`.
#[must_use]
pub fn sanitize_toml_error_message(message: &str) -> String {
    fn looks_like_snippet_line(line: &str) -> bool {
        let trimmed = line.trim_start();
        if trimmed.starts_with('|') {
            return true;
        }

        // `toml::de::Error` snippet lines often look like:
        // `1 | key = "value"` (line number, pipe, raw source).
        let mut chars = trimmed.chars();
        let mut saw_digit = false;
        while let Some(ch) = chars.next() {
            if ch.is_ascii_digit() {
                saw_digit = true;
                continue;
            }

            if saw_digit && ch.is_whitespace() {
                continue;
            }

            if saw_digit && ch == '|' {
                return true;
            }

            break;
        }

        false
    }

    fn strip_snippet_block(message: &str) -> String {
        if !message.contains('\n') {
            return message.to_string();
        }

        // `toml::de::Error` `Display` output often includes a multi-line source snippet block like:
        //
        // ```text
        // TOML parse error at line 1, column 10
        //   |
        // 1 | api_key = "secret"
        //   |          ^
        // invalid type: string "secret", expected boolean
        // ```
        //
        // Strip the snippet *lines* (which can contain raw config/manifest text) while preserving
        // any non-snippet context lines after the snippet (the final diagnostic is often useful).
        let mut out = String::with_capacity(message.len());
        for chunk in message.split_inclusive('\n') {
            let line = chunk.strip_suffix('\n').unwrap_or(chunk);
            let line = line.trim_end_matches('\r');
            let trimmed = line.trim_start();
            if trimmed.starts_with("-->") || looks_like_snippet_line(line) {
                continue;
            }
            out.push_str(chunk);
        }

        out.trim_end_matches(&['\n', '\r'][..]).to_string()
    }

    fn redact_single_snippet_line(line: &str) -> Option<String> {
        let line = line.trim_end_matches('\r');
        if !looks_like_snippet_line(line) {
            return None;
        }

        let pipe = line.find('|')?;
        let after_pipe = &line[pipe + 1..];
        if after_pipe.trim().is_empty() {
            return Some(line.to_string());
        }

        let mut out = String::with_capacity(line.len());
        out.push_str(&line[..pipe + 1]);
        out.push(' ');
        out.push_str("<redacted>");
        Some(out)
    }

    // `toml::de::Error::message()` is snippet-free, but callers occasionally stringify full
    // `toml::de::Error` values (including the source snippet) when logging or panicking.
    // Best-effort: strip the snippet block so we don't leak raw config/manifest contents.
    let mut message = strip_snippet_block(message);
    if let Some(redacted) = redact_single_snippet_line(&message) {
        message = redacted;
    }

    let out = sanitize_json_error_message(&message);

    // Some TOML diagnostics (notably semver parsing) quote scalar values using single quotes.
    // Redact those as well, using the same backslash/escape handling we apply to double quotes.
    let mut sanitized = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(start) = rest.find('\'') {
        // Include the opening quote.
        sanitized.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let mut end = None;
        let bytes = rest.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if b != b'\'' {
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
            sanitized.push_str("<redacted>");
            rest = "";
            break;
        };

        sanitized.push_str("<redacted>'");
        rest = &rest[end + 1..];
    }
    sanitized.push_str(rest);
    sanitized
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

    #[test]
    fn sanitize_json_error_message_redacts_backticked_numeric_values() {
        let message = "invalid type: integer `123`, expected a boolean";
        let sanitized = sanitize_json_error_message(message);
        assert!(
            !sanitized.contains("123"),
            "expected sanitized message to omit backticked scalar values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected sanitized message to include redaction marker: {sanitized}"
        );
    }

    #[test]
    fn sanitize_json_error_message_preserves_expected_token_backticks() {
        // Parser errors can include expected tokens in backticks; keep these intact.
        let message = "expected `,` or `}` at line 1 column 8";
        let sanitized = sanitize_json_error_message(message);
        assert_eq!(sanitized, message);
    }

    #[test]
    fn sanitize_json_error_message_preserves_backticks_in_expected_portion_for_invalid_type_errors()
    {
        // Some wrapper/custom errors include backticked schema names in the `expected` portion of
        // an `invalid type/value` diagnostic. Only redact backticks that refer to the unexpected
        // scalar value, not the expected list.
        let message = "invalid type: map, expected one of `foo`, `bar`";
        let sanitized = sanitize_json_error_message(message);
        assert_eq!(sanitized, message);
    }

    #[test]
    fn sanitize_toml_error_message_redacts_single_quoted_values_with_escaped_quotes() {
        let secret_suffix = "nova-core-toml-single-quote-secret";
        let message = format!("invalid semver version 'prefix\\'{secret_suffix}', expected 1.2.3");
        let sanitized = sanitize_toml_error_message(&message);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected TOML sanitizer to omit single-quoted values (even when escaped): {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected TOML sanitizer to include redaction marker: {sanitized}"
        );
    }

    #[test]
    fn sanitize_toml_error_message_redacts_backticked_numeric_values() {
        let message = "invalid type: integer `123`, expected a boolean";
        let sanitized = sanitize_toml_error_message(message);
        assert!(
            !sanitized.contains("123"),
            "expected TOML sanitizer to omit backticked scalar values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected TOML sanitizer to include redaction marker: {sanitized}"
        );
    }

    #[test]
    fn sanitize_toml_error_message_strips_source_snippet_blocks() {
        let secret_suffix = "nova-core-toml-snippet-secret";
        let message = format!(
            "TOML parse error at line 1, column 10\n1 | api_key = \"prefix{secret_suffix}\"\n  |          ^\n"
        );
        assert!(
            message.contains(secret_suffix),
            "expected raw TOML error display to include the secret so this test catches leaks: {message}"
        );

        let sanitized = sanitize_toml_error_message(&message);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected TOML sanitizer to omit snippet contents: {sanitized}"
        );
        assert!(
            !sanitized.contains("api_key ="),
            "expected TOML sanitizer to strip snippet lines entirely: {sanitized}"
        );
    }

    #[test]
    fn sanitize_toml_error_message_preserves_trailing_diagnostics_after_snippet_blocks() {
        let secret_suffix = "nova-core-toml-trailing-diagnostic-secret";
        let message = format!(
            "TOML parse error at line 1, column 10\n1 | api_key = \"{secret_suffix}\"\n  |          ^\ninvalid type: string \"{secret_suffix}\", expected boolean\n"
        );
        assert!(
            message.contains(secret_suffix),
            "expected raw TOML error display to include the secret so this test catches leaks: {message}"
        );

        let sanitized = sanitize_toml_error_message(&message);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected TOML sanitizer to omit snippet contents and scalar values: {sanitized}"
        );
        assert!(
            !sanitized.contains("api_key ="),
            "expected TOML sanitizer to strip snippet source lines: {sanitized}"
        );
        assert!(
            sanitized.contains("invalid type:"),
            "expected TOML sanitizer to preserve trailing diagnostics after snippet blocks: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected TOML sanitizer to include redaction marker: {sanitized}"
        );
    }

    #[test]
    fn sanitize_toml_error_message_redacts_single_snippet_lines_with_unquoted_numbers() {
        let secret_number = "9876543210";
        let line = format!("1 | enabled = {secret_number}");
        assert!(
            line.contains(secret_number),
            "expected raw snippet line to include the numeric value so this test catches leaks: {line}"
        );

        let sanitized = sanitize_toml_error_message(&line);
        assert!(
            !sanitized.contains(secret_number),
            "expected TOML sanitizer to omit numeric values from snippet lines: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected TOML sanitizer to include redaction marker: {sanitized}"
        );
    }
}
