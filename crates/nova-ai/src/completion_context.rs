use std::borrow::Cow;

/// Semantic context used to build prompts for multi-token completion generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiTokenCompletionContext {
    pub receiver_type: Option<String>,
    pub expected_type: Option<String>,
    pub surrounding_code: String,
    pub available_methods: Vec<String>,
    /// Fully qualified names that are valid to import in this file.
    pub importable_paths: Vec<String>,
}

impl MultiTokenCompletionContext {
    /// Maximum number of method names included in prompts (and typically in the IDE-side context).
    ///
    /// Keeping this bounded ensures prompts remain stable and fit within
    /// [`CompletionContextBuilder::max_prompt_chars`] without needing to hard-truncate the prompt
    /// mid-line.
    pub const MAX_AVAILABLE_METHODS: usize = 200;

    /// Maximum number of importable paths included in prompts (and typically in the IDE-side
    /// context).
    pub const MAX_IMPORTABLE_PATHS: usize = 50;
}

/// A deterministic prompt builder for multi-token completions.
#[derive(Clone, Debug, Default)]
pub struct CompletionContextBuilder {
    pub max_prompt_chars: usize,
}

/// Escape arbitrary user-provided content so it cannot terminate a Markdown fenced code block.
///
/// This guarantees the returned string contains **no literal `"```"` substring** by inserting a
/// backslash before any backtick that would otherwise form 3 consecutive backticks.
fn escape_markdown_fence_payload(text: &str) -> Cow<'_, str> {
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

impl CompletionContextBuilder {
    pub fn new(max_prompt_chars: usize) -> Self {
        Self { max_prompt_chars }
    }

    fn truncate_utf8_boundary(prompt: &mut String, max_bytes: usize) {
        if max_bytes >= prompt.len() {
            return;
        }

        let mut idx = max_bytes;
        while idx > 0 && !prompt.is_char_boundary(idx) {
            idx -= 1;
        }
        prompt.truncate(idx);
    }

    /// Build an instruction prompt for multi-token completions.
    pub fn build_completion_prompt(
        &self,
        ctx: &MultiTokenCompletionContext,
        max_items: usize,
    ) -> String {
        // Keep the formatting stable for tests and provider caching.
        let receiver_type =
            escape_markdown_fence_payload(ctx.receiver_type.as_deref().unwrap_or("<unknown>"));
        let expected_type =
            escape_markdown_fence_payload(ctx.expected_type.as_deref().unwrap_or("<unknown>"));
        let surrounding_code = escape_markdown_fence_payload(&ctx.surrounding_code);

        let mut prompt = String::new();
        prompt.push_str("You are Nova, a Java code completion engine.\n");
        prompt.push_str(
            "Generate multi-token completion suggestions (method chains or small templates).\n",
        );
        prompt.push_str(&format!("Return up to {max_items} suggestions.\n"));
        prompt.push_str("Rules:\n");
        prompt.push_str(
            "- Top-level method calls in insert_text must come from the Available methods list.\n",
        );
        prompt.push_str(
            "- Any additional_edits.add_import must be one of the Importable symbols list.\n",
        );
        prompt.push_str(
            "- insert_text should be the text to insert after the cursor (do not repeat the receiver expression).\n",
        );
        prompt.push_str("- Avoid suggesting file paths.\n");
        prompt.push_str("\n");
        prompt.push_str(&format!("Receiver type: {}\n", receiver_type.as_ref()));
        prompt.push_str(&format!("Expected type: {}\n", expected_type.as_ref()));
        prompt.push_str("\n");
        prompt.push_str("Available methods:\n");
        for method in ctx
            .available_methods
            .iter()
            .take(MultiTokenCompletionContext::MAX_AVAILABLE_METHODS)
        {
            prompt.push_str("- ");
            prompt.push_str(escape_markdown_fence_payload(method).as_ref());
            prompt.push('\n');
        }
        if !ctx.importable_paths.is_empty() {
            prompt.push('\n');
            prompt.push_str("Importable symbols:\n");
            for path in ctx
                .importable_paths
                .iter()
                .take(MultiTokenCompletionContext::MAX_IMPORTABLE_PATHS)
            {
                prompt.push_str("- ");
                prompt.push_str(escape_markdown_fence_payload(path).as_ref());
                prompt.push('\n');
            }
        }
        prompt.push('\n');
        prompt.push_str("Surrounding code:\n```java\n");
        prompt.push_str(surrounding_code.as_ref());
        if !ctx.surrounding_code.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("```\n");

        // Cap size defensively to avoid accidentally sending huge prompts.
        if self.max_prompt_chars > 0 && prompt.len() > self.max_prompt_chars {
            Self::truncate_utf8_boundary(&mut prompt, self.max_prompt_chars);
        }

        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_expected_sections() {
        let ctx = MultiTokenCompletionContext {
            receiver_type: Some("java.util.stream.Stream<Person>".into()),
            expected_type: Some("java.util.List<String>".into()),
            surrounding_code: "people.stream().".into(),
            available_methods: vec!["filter".into(), "map".into(), "collect".into()],
            importable_paths: vec!["java.util.stream.Collectors".into()],
        };
        let builder = CompletionContextBuilder::new(10_000);
        let prompt = builder.build_completion_prompt(&ctx, 3);

        assert!(prompt.contains("Rules:\n"));
        assert!(prompt.contains(
            "- Top-level method calls in insert_text must come from the Available methods list.\n"
        ));
        assert!(prompt.contains(
            "- Any additional_edits.add_import must be one of the Importable symbols list.\n"
        ));
        assert!(prompt.contains(
            "- insert_text should be the text to insert after the cursor (do not repeat the receiver expression).\n"
        ));
        assert!(prompt.contains("- Avoid suggesting file paths.\n"));
        assert!(prompt.contains("Receiver type: java.util.stream.Stream<Person>"));
        assert!(prompt.contains("Expected type: java.util.List<String>"));
        assert!(prompt.contains("- filter"));
        assert!(prompt.contains("- map"));
        assert!(prompt.contains("- collect"));
        assert!(prompt.contains("- java.util.stream.Collectors"));
        assert!(prompt.contains("```java\npeople.stream().\n```"));
    }

    #[test]
    fn prompt_escapes_triple_backticks_to_keep_fences_stable() {
        let ctx = MultiTokenCompletionContext {
            receiver_type: Some("A```B".into()),
            expected_type: Some("C````D".into()),
            surrounding_code: "a``````b".into(),
            available_methods: vec!["m```1".into(), "ok".into()],
            importable_paths: vec!["com.example.``````Foo".into()],
        };

        let prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx, 2);
        assert!(
            prompt.contains("```java\n"),
            "expected prompt to contain opening fence: {prompt}"
        );
        assert_eq!(
            prompt.match_indices("```").count(),
            2,
            "expected exactly one opening and one closing fence: {prompt}"
        );
    }

    #[test]
    fn prompt_truncation_is_utf8_safe() {
        let ctx = MultiTokenCompletionContext {
            receiver_type: Some("String".into()),
            expected_type: Some("void".into()),
            surrounding_code: "System.out.println(\"😀\");".into(),
            available_methods: vec!["println".into()],
            importable_paths: vec![],
        };

        let full_prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx, 1);
        let emoji_idx = full_prompt.find('😀').expect("emoji should be present");

        let truncated =
            CompletionContextBuilder::new(emoji_idx + 1).build_completion_prompt(&ctx, 1);

        assert_eq!(truncated.len(), emoji_idx);
        assert!(!truncated.contains('😀'));
    }

    #[test]
    fn prompt_truncates_available_methods_deterministically() {
        let max = MultiTokenCompletionContext::MAX_AVAILABLE_METHODS;
        let methods = (0..(max + 10))
            .map(|idx| format!("method{idx:04}"))
            .collect::<Vec<_>>();

        let ctx = MultiTokenCompletionContext {
            receiver_type: Some("Foo".into()),
            expected_type: Some("Bar".into()),
            surrounding_code: "foo.".into(),
            available_methods: methods.clone(),
            importable_paths: vec![],
        };

        let builder = CompletionContextBuilder::new(100_000);
        let prompt = builder.build_completion_prompt(&ctx, 1);

        let mut lines = prompt.lines();
        while let Some(line) = lines.next() {
            if line == "Available methods:" {
                break;
            }
        }

        let prompt_methods = lines
            .take_while(|line| !line.is_empty())
            .map(|line| line.strip_prefix("- ").expect("method bullet"))
            .collect::<Vec<_>>();

        let expected = methods.iter().take(max).map(String::as_str).collect::<Vec<_>>();
        assert_eq!(prompt_methods, expected);
        assert!(
            !prompt.contains(&format!("- {}\n", methods[max])),
            "expected methods past the cap to be omitted from the prompt"
        );
    }
}
