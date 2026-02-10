use crate::util::markdown::escape_markdown_fence_payload;

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
        // Always put the closing fence on its own line (preceded by a newline) so that a payload
        // ending in backticks cannot accidentally form a fence boundary by spanning the join.
        const FOOTER: &str = "\n```\n";

        // Keep the formatting stable for tests and provider caching.
        let receiver_type =
            escape_markdown_fence_payload(ctx.receiver_type.as_deref().unwrap_or("<unknown>"));
        let expected_type =
            escape_markdown_fence_payload(ctx.expected_type.as_deref().unwrap_or("<unknown>"));
        let surrounding_code = escape_markdown_fence_payload(&ctx.surrounding_code);

        // ---------------------------------------------------------------------
        // Fixed header (instructions, metadata, lists, opening fence)
        // ---------------------------------------------------------------------
        let mut header = String::new();
        header.push_str("You are Nova, a Java code completion engine.\n");
        header.push_str(
            "Generate multi-token completion suggestions (method chains or small templates).\n",
        );
        header.push_str(&format!("Return up to {max_items} suggestions.\n"));
        header.push_str("Rules:\n");
        header.push_str(
            "- Top-level method calls in insert_text must come from the Available methods list.\n",
        );
        header.push_str(
            "- Any additional_edits.add_import must be one of the Importable symbols list.\n",
        );
        header.push_str(
            "- insert_text should be the text to insert after the cursor (do not repeat the receiver expression).\n",
        );
        header.push_str("- Avoid suggesting file paths.\n");
        header.push_str("\n");
        header.push_str(&format!("Receiver type: {}\n", receiver_type.as_ref()));
        header.push_str(&format!("Expected type: {}\n", expected_type.as_ref()));
        header.push_str("\n");
        header.push_str("Available methods:\n");
        for method in ctx
            .available_methods
            .iter()
            .take(MultiTokenCompletionContext::MAX_AVAILABLE_METHODS)
        {
            header.push_str("- ");
            header.push_str(escape_markdown_fence_payload(method).as_ref());
            header.push('\n');
        }
        if !ctx.importable_paths.is_empty() {
            header.push('\n');
            header.push_str("Importable symbols:\n");
            for path in ctx
                .importable_paths
                .iter()
                .take(MultiTokenCompletionContext::MAX_IMPORTABLE_PATHS)
            {
                header.push_str("- ");
                header.push_str(escape_markdown_fence_payload(path).as_ref());
                header.push('\n');
            }
        }
        header.push('\n');
        header.push_str("Surrounding code:\n```java\n");

        // ---------------------------------------------------------------------
        // Fenced body (surrounding code)
        // ---------------------------------------------------------------------
        let mut body = surrounding_code.into_owned();

        // Cap size defensively to avoid accidentally sending huge prompts.
        if self.max_prompt_chars > 0 {
            let fixed_len = header.len() + FOOTER.len();
            let avail = self.max_prompt_chars.saturating_sub(fixed_len);
            if body.len() > avail {
                Self::truncate_utf8_boundary(&mut body, avail);
            }
        }

        if body.ends_with('\n') {
            // The footer always inserts a newline before the closing fence; trim one trailing LF so
            // we don't end up with an extra blank line between code and fence.
            body.pop();
        }

        // ---------------------------------------------------------------------
        // Assemble prompt (header + body + footer)
        // ---------------------------------------------------------------------
        let mut prompt = String::with_capacity(header.len() + body.len() + FOOTER.len());
        prompt.push_str(&header);
        prompt.push_str(&body);
        prompt.push_str(FOOTER);
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

        let emoji_idx = ctx
            .surrounding_code
            .find('😀')
            .expect("emoji should be present");

        let full_prompt = CompletionContextBuilder::new(10_000).build_completion_prompt(&ctx, 1);
        let fixed_len = full_prompt.len() - ctx.surrounding_code.len();

        // Force truncation in the middle of the emoji's UTF-8 byte sequence.
        let max_prompt_chars = fixed_len + emoji_idx + 1;
        let truncated =
            CompletionContextBuilder::new(max_prompt_chars).build_completion_prompt(&ctx, 1);

        assert!(
            truncated.len() <= max_prompt_chars,
            "prompt should respect max_prompt_chars (got {} > {max_prompt_chars})",
            truncated.len()
        );
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

    #[test]
    fn prompt_truncation_preserves_markdown_fences() {
        let ctx = MultiTokenCompletionContext {
            receiver_type: Some("String".into()),
            expected_type: Some("void".into()),
            surrounding_code: "x".repeat(10_000),
            available_methods: vec!["println".into()],
            importable_paths: vec![],
        };

        // Force truncation with no remaining budget for the fenced body. This ensures truncation is
        // applied only to the fenced body portion and that the closing fence is never dropped.
        let full_prompt = CompletionContextBuilder::new(0).build_completion_prompt(&ctx, 1);
        let fixed_len = full_prompt.len() - ctx.surrounding_code.len();
        let max_prompt_chars = fixed_len;
        let prompt =
            CompletionContextBuilder::new(max_prompt_chars).build_completion_prompt(&ctx, 1);

        assert!(
            prompt.len() <= max_prompt_chars,
            "prompt should respect max_prompt_chars (got {} > {max_prompt_chars})",
            prompt.len()
        );
        assert!(
            prompt.ends_with("```\n"),
            "prompt should always end with a closing fence: {prompt:?}"
        );
        assert_eq!(
            prompt.match_indices("```").count(),
            2,
            "expected exactly one opening and one closing fence: {prompt}"
        );
        assert_eq!(
            prompt.match_indices("```java\n").count(),
            1,
            "expected exactly one opening fence: {prompt}"
        );
    }
}
