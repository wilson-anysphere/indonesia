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
    fn contains_escaped_snippet_block(message: &str) -> bool {
        // When `toml::de::Error` values are formatted via `Debug` (for example via `?err` in
        // `tracing`), embedded newlines are escaped as `\n`. If the `Debug` representation includes
        // the `Display` output (which contains the raw source snippet), we can end up with a
        // single-line string containing `\n1 | key = ...` style snippet markers.
        //
        // Detect those cases so we can treat the escaped newlines as real ones and strip snippet
        // lines without relying on callers to normalize formatting.
        if !message.contains("\\n") {
            return false;
        }

        // Snippet blocks always contain `|` lines (and often `-->`), so bail out early to avoid
        // work on unrelated strings that happen to contain `\n` escape sequences.
        if !message.contains('|') && !message.contains("-->") {
            return false;
        }

        let bytes = message.as_bytes();
        let mut i = 0usize;
        while i + 1 < bytes.len() {
            if bytes[i] == b'\\' && bytes[i + 1] == b'n' {
                let mut j = i + 2;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= bytes.len() {
                    break;
                }

                if bytes[j] == b'|' {
                    return true;
                }

                if j + 2 < bytes.len()
                    && bytes[j] == b'-'
                    && bytes[j + 1] == b'-'
                    && bytes[j + 2] == b'>'
                {
                    return true;
                }

                if bytes[j].is_ascii_digit() {
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'|' {
                        return true;
                    }
                }
            }
            i += 1;
        }

        false
    }

    fn sanitize_inner(message: &str) -> String {
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

    fn sanitize_toml_error_debug_output(message: &str) -> Option<String> {
        // `toml::de::Error`'s `Debug` output includes the full input TOML source as
        // `raw: Some("...")`, which can leak secrets when logged with `?err` (e.g. via `tracing`).
        //
        // Sanitize that debug representation by:
        // - sanitizing the `message: "..."` field (unescape, sanitize, then re-escape), and
        // - redacting the embedded `raw: Some("...")` text entirely.
        //
        // We keep this best-effort and intentionally narrow so we don't accidentally rewrite
        // unrelated debug output.
        if !message.contains("TomlError {") {
            return None;
        }
        if !message.contains("raw: Some(\"") {
            return None;
        }
        if !message.contains("message: \"") {
            return None;
        }

        fn find_unescaped_quote_end(text: &str) -> Option<usize> {
            let bytes = text.as_bytes();
            for (idx, &b) in bytes.iter().enumerate() {
                if b != b'"' {
                    continue;
                }

                let mut backslashes = 0usize;
                let mut k = idx;
                while k > 0 && bytes[k - 1] == b'\\' {
                    backslashes += 1;
                    k -= 1;
                }
                if backslashes % 2 == 0 {
                    return Some(idx);
                }
            }
            None
        }

        fn unescape_rust_debug_string(value: &str) -> String {
            let mut out = String::with_capacity(value.len());
            let mut chars = value.chars();
            while let Some(ch) = chars.next() {
                if ch != '\\' {
                    out.push(ch);
                    continue;
                }

                let Some(esc) = chars.next() else {
                    out.push('\\');
                    break;
                };

                match esc {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '\'' => out.push('\''),
                    '0' => out.push('\0'),
                    'x' => {
                        let hi = chars.next();
                        let lo = chars.next();
                        let Some((hi, lo)) = hi.zip(lo) else {
                            continue;
                        };
                        let Some(hi) = hi.to_digit(16) else {
                            continue;
                        };
                        let Some(lo) = lo.to_digit(16) else {
                            continue;
                        };
                        let byte = ((hi << 4) | lo) as u8;
                        out.push(byte as char);
                    }
                    'u' => {
                        if chars.next() != Some('{') {
                            continue;
                        }

                        let mut hex = String::new();
                        while let Some(ch) = chars.next() {
                            if ch == '}' {
                                break;
                            }
                            hex.push(ch);
                        }

                        if let Ok(codepoint) = u32::from_str_radix(hex.trim(), 16) {
                            if let Some(ch) = char::from_u32(codepoint) {
                                out.push(ch);
                            }
                        }
                    }
                    other => out.push(other),
                }
            }
            out
        }

        let mut out = message.to_string();

        // Sanitize `message: "..."` fields in-place.
        let mut search_start = 0usize;
        const MESSAGE_PREFIX: &str = "message: \"";
        while let Some(rel) = out[search_start..].find(MESSAGE_PREFIX) {
            let prefix_idx = search_start + rel;
            let start_quote = prefix_idx + MESSAGE_PREFIX.len().saturating_sub(1);
            let after_start = &out[start_quote + 1..];
            let Some(end_rel) = find_unescaped_quote_end(after_start) else {
                return Some(sanitize_inner(message));
            };
            let end_quote = start_quote + 1 + end_rel;
            let escaped = &out[start_quote + 1..end_quote];
            let unescaped = unescape_rust_debug_string(escaped);
            let sanitized = sanitize_inner(&unescaped);
            let escaped_sanitized = format!("{sanitized:?}");
            out.replace_range(start_quote..=end_quote, &escaped_sanitized);
            search_start = start_quote + escaped_sanitized.len();
        }

        // Redact `raw: Some("...")` source blocks entirely.
        search_start = 0;
        const RAW_PREFIX: &str = "raw: Some(\"";
        while let Some(rel) = out[search_start..].find(RAW_PREFIX) {
            let prefix_idx = search_start + rel;
            let start_quote = prefix_idx + RAW_PREFIX.len().saturating_sub(1);
            let after_start = &out[start_quote + 1..];
            let Some(end_rel) = find_unescaped_quote_end(after_start) else {
                return Some(sanitize_inner(message));
            };
            let end_quote = start_quote + 1 + end_rel;
            out.replace_range(start_quote + 1..end_quote, "<redacted>");
            search_start = start_quote + 1 + "<redacted>".len() + 1;
        }

        Some(out)
    }

    if let Some(sanitized) = sanitize_toml_error_debug_output(message) {
        return sanitized;
    }

    if !message.contains('\n') && contains_escaped_snippet_block(message) {
        // Preserve the original single-line formatting by re-escaping any newlines introduced by
        // unescaping and stripping snippet blocks.
        let unescaped = message.replace("\\r\\n", "\n").replace("\\n", "\n");
        let sanitized = sanitize_inner(&unescaped);
        return sanitized.replace('\n', "\\n");
    }

    sanitize_inner(message)
}

fn looks_like_serde_json_error_message(message: &str) -> bool {
    message.contains("invalid type:")
        || message.contains("invalid value:")
        || message.contains("unknown field")
        || message.contains("unknown variant")
}

fn looks_like_toml_error_message(message: &str) -> bool {
    if message.contains("TOML parse error") {
        return true;
    }

    if message.contains("TomlError {") && message.contains("raw: Some(") {
        return true;
    }

    if message.contains("invalid semver version") || message.contains("unknown capability") {
        return true;
    }

    if message.contains('|') || message.contains("-->") {
        for line in message.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("-->") || trimmed.starts_with('|') {
                return true;
            }

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
        }
    }

    // Best-effort: escaped newline snippet blocks (e.g. from debug output).
    if message.contains("\\n") && (message.contains("\\n|") || message.contains("\\n1 |")) {
        return true;
    }

    false
}

/// Best-effort sanitizer for stringified error messages that may include user-controlled scalar
/// values.
///
/// This is designed for user-facing diagnostics printed to stderr or returned over protocol error
/// channels. Callers can optionally pass whether the underlying error chain contains a typed
/// `serde_json::Error` to force JSON sanitization even when the final message doesn't match common
/// serde-json patterns.
#[must_use]
pub fn sanitize_error_message_text(message: &str, contains_serde_json: bool) -> String {
    if looks_like_toml_error_message(message) {
        sanitize_toml_error_message(message)
    } else if contains_serde_json || looks_like_serde_json_error_message(message) {
        sanitize_json_error_message(message)
    } else {
        message.to_owned()
    }
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
    fn sanitize_error_message_text_prefers_toml_sanitization_for_snippet_blocks() {
        let secret_suffix = "nova-core-error-text-snippet-secret";
        let secret_number = 42_424_242u64;
        let message = format!(
            "TOML parse error at line 1, column 10\n1 | api_key = \"{secret_suffix}\"\n2 | enabled = {secret_number}\n  |          ^\ninvalid type: string \"{secret_suffix}\", expected boolean"
        );
        assert!(
            message.contains(secret_suffix),
            "expected raw message to include secret so this test catches leaks: {message}"
        );
        assert!(
            message.contains(&secret_number.to_string()),
            "expected raw message to include numeric value so this test catches leaks: {message}"
        );

        let sanitized = sanitize_error_message_text(&message, false);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected combined sanitizer to omit TOML snippet contents: {sanitized}"
        );
        assert!(
            !sanitized.contains(&secret_number.to_string()),
            "expected combined sanitizer to omit numeric values from TOML snippet blocks: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected combined sanitizer to include redaction marker: {sanitized}"
        );
    }

    #[test]
    fn sanitize_error_message_text_sanitizes_serde_json_like_messages() {
        let secret_suffix = "nova-core-error-text-json-secret";
        let message = format!(
            r#"invalid type: string "prefix\"{secret_suffix}", expected boolean"#,
        );
        assert!(
            message.contains(secret_suffix),
            "expected raw message to include secret so this test catches leaks: {message}"
        );

        let sanitized = sanitize_error_message_text(&message, false);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected combined sanitizer to omit json scalar values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected combined sanitizer to include redaction marker: {sanitized}"
        );
    }

    #[test]
    fn sanitize_error_message_text_sanitizes_toml_single_quoted_values() {
        let secret_suffix = "nova-core-error-text-toml-single-quote-secret";
        let message = format!("invalid semver version 'prefix\\'{secret_suffix}', expected 1.2.3");
        assert!(
            message.contains(secret_suffix),
            "expected raw message to include secret so this test catches leaks: {message}"
        );

        let sanitized = sanitize_error_message_text(&message, false);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected combined sanitizer to omit single-quoted toml scalar values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected combined sanitizer to include redaction marker: {sanitized}"
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

    #[test]
    fn sanitize_toml_error_message_strips_snippet_blocks_when_newlines_are_escaped() {
        let secret_suffix = "nova-core-toml-escaped-snippet-secret";
        let secret_number = 42_424_242u64;
        let secret_number_text = secret_number.to_string();
        let message = format!(
            "TOML parse error at line 1, column 10\\n1 | api_key = \"{secret_suffix}\"\\n2 | enabled = {secret_number}\\n  |          ^\\ninvalid type: string \"{secret_suffix}\", expected boolean"
        );
        assert!(
            message.contains(secret_suffix),
            "expected raw message to include the secret so this test catches leaks: {message}"
        );
        assert!(
            message.contains(&secret_number_text),
            "expected raw message to include the numeric value so this test catches leaks: {message}"
        );

        let sanitized = sanitize_toml_error_message(&message);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected TOML sanitizer to omit escaped snippet contents and scalar values: {sanitized}"
        );
        assert!(
            !sanitized.contains(&secret_number_text),
            "expected TOML sanitizer to omit numeric values from escaped snippet blocks: {sanitized}"
        );
        assert!(
            !sanitized.contains("api_key ="),
            "expected TOML sanitizer to strip snippet source lines from escaped snippet blocks: {sanitized}"
        );
        assert!(
            sanitized.contains("invalid type:"),
            "expected TOML sanitizer to preserve trailing diagnostics after snippet blocks: {sanitized}"
        );
        assert!(
            sanitized.contains("\\n"),
            "expected TOML sanitizer to keep escaped newlines in escaped-snippet inputs: {sanitized}"
        );
        assert!(
            !sanitized.contains('\n'),
            "expected TOML sanitizer not to inject actual newlines when input used escapes: {sanitized:?}"
        );
    }

    #[test]
    fn sanitize_toml_error_message_redacts_raw_toml_text_in_debug_output() {
        let secret_suffix = "nova-core-toml-debug-secret";
        let message = format!(
            "TomlError {{ message: \"invalid array\\nexpected `]`\", raw: Some(\"flag = [1,\\napi_key = \\\\\\\"{secret_suffix}\\\\\\\"\\n\"), keys: [], span: Some(11..12) }}"
        );
        assert!(
            message.contains(secret_suffix),
            "expected raw TomlError debug output to contain secret so this test catches leaks: {message}"
        );

        let sanitized = sanitize_toml_error_message(&message);
        assert!(
            !sanitized.contains(secret_suffix),
            "expected TOML sanitizer to omit raw TOML source from debug output: {sanitized}"
        );
        assert!(
            sanitized.contains("invalid array"),
            "expected TOML sanitizer to preserve debug output message field: {sanitized}"
        );
        assert!(
            sanitized.contains("raw: Some(\"<redacted>\")"),
            "expected TOML sanitizer to redact the embedded raw TOML source in debug output: {sanitized}"
        );
    }
}
