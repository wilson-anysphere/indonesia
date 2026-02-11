pub(crate) fn sanitize_serde_json_error(err: &serde_json::Error) -> String {
    sanitize_json_error_message(&err.to_string())
}

fn sanitize_json_error_message(message: &str) -> String {
    // `serde_json::Error` display strings can include user-provided scalar values (for example:
    // `invalid type: string "..."`). Conservatively redact all double-quoted substrings so secrets
    // are not echoed back through JSON-RPC error messages.
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let Some(end) = rest.find('"') else {
            // Unterminated quote: append the remainder and stop.
            out.push_str(rest);
            return out;
        };

        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Redact only the first backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        if let Some(end_rel) = out[start.saturating_add(1)..].find('`') {
            let end = start.saturating_add(1).saturating_add(end_rel);
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

