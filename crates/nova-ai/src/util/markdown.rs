use std::borrow::Cow;

/// Escape arbitrary user-provided content so it cannot terminate a Markdown fenced code block.
///
/// This guarantees the returned string contains **no literal `"```"` substring** by inserting a
/// backslash before any backtick that would otherwise form 3 consecutive backticks.
pub(crate) fn escape_markdown_fence_payload(text: &str) -> Cow<'_, str> {
    // Fast path: if there's no triple-backtick substring, we don't need to allocate.
    if !text.contains("```") {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len() + text.len() / 2);
    let mut backticks = 0usize;

    for ch in text.chars() {
        if ch == '`' {
            if backticks == 2 {
                // Break the run before we would emit a third consecutive '`'.
                out.push('\\');
                backticks = 0;
            }
            out.push('`');
            backticks += 1;
        } else {
            out.push(ch);
            backticks = 0;
        }
    }

    debug_assert!(
        !out.contains("```"),
        "escape_markdown_fence_payload must remove all triple backticks"
    );

    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_triple_backticks_without_allocating_when_unnecessary() {
        let plain = escape_markdown_fence_payload("no fences here");
        assert!(matches!(plain, Cow::Borrowed(_)));

        for input in ["```", "````", "``````", "a```b", "a````b", "a``````b"] {
            let escaped = escape_markdown_fence_payload(input);
            assert!(
                !escaped.contains("```"),
                "escaped payload should not contain triple backticks: input={input:?} escaped={escaped:?}"
            );
        }
    }
}

