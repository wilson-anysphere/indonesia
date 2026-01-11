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

/// A deterministic prompt builder for multi-token completions.
#[derive(Clone, Debug, Default)]
pub struct CompletionContextBuilder {
    pub max_prompt_chars: usize,
}

impl CompletionContextBuilder {
    pub fn new(max_prompt_chars: usize) -> Self {
        Self { max_prompt_chars }
    }

    /// Build an instruction prompt for multi-token completions.
    pub fn build_completion_prompt(
        &self,
        ctx: &MultiTokenCompletionContext,
        max_items: usize,
    ) -> String {
        // Keep the formatting stable for tests and provider caching.
        let receiver_type = ctx.receiver_type.as_deref().unwrap_or("<unknown>");
        let expected_type = ctx.expected_type.as_deref().unwrap_or("<unknown>");

        let mut prompt = String::new();
        prompt.push_str("You are Nova, a Java code completion engine.\n");
        prompt.push_str(
            "Generate multi-token completion suggestions (method chains or small templates).\n",
        );
        prompt.push_str(&format!("Return up to {max_items} suggestions.\n"));
        prompt.push_str("\n");
        prompt.push_str(&format!("Receiver type: {receiver_type}\n"));
        prompt.push_str(&format!("Expected type: {expected_type}\n"));
        prompt.push_str("\n");
        prompt.push_str("Available methods:\n");
        for method in &ctx.available_methods {
            prompt.push_str("- ");
            prompt.push_str(method);
            prompt.push('\n');
        }
        if !ctx.importable_paths.is_empty() {
            prompt.push('\n');
            prompt.push_str("Importable symbols:\n");
            for path in &ctx.importable_paths {
                prompt.push_str("- ");
                prompt.push_str(path);
                prompt.push('\n');
            }
        }
        prompt.push('\n');
        prompt.push_str("Surrounding code:\n```java\n");
        prompt.push_str(&ctx.surrounding_code);
        if !ctx.surrounding_code.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("```\n");

        // Cap size defensively to avoid accidentally sending huge prompts.
        if self.max_prompt_chars > 0 && prompt.len() > self.max_prompt_chars {
            prompt.truncate(self.max_prompt_chars);
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

        assert!(prompt.contains("Receiver type: java.util.stream.Stream<Person>"));
        assert!(prompt.contains("Expected type: java.util.List<String>"));
        assert!(prompt.contains("- filter"));
        assert!(prompt.contains("- map"));
        assert!(prompt.contains("- collect"));
        assert!(prompt.contains("- java.util.stream.Collectors"));
        assert!(prompt.contains("```java\npeople.stream().\n```"));
    }
}
