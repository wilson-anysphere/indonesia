use crate::CompletionConfig;
use nova_ai::{AdditionalEdit, MultiTokenCompletion, MultiTokenCompletionContext};

/// Validate AI completions against best-effort semantic constraints.
pub fn validate_ai_completion(
    ctx: &MultiTokenCompletionContext,
    completion: &MultiTokenCompletion,
    config: &CompletionConfig,
) -> bool {
    if completion.insert_text.trim().is_empty() {
        return false;
    }

    if completion.additional_edits.len() > config.ai_max_additional_edits {
        return false;
    }

    if token_count(&completion.insert_text) > config.ai_max_tokens {
        return false;
    }

    if !additional_edits_allowed(ctx, completion) {
        return false;
    }

    // Best-effort: verify top-level method chain segments.
    let methods = extract_top_level_method_calls(&completion.insert_text);
    methods
        .iter()
        .all(|m| ctx.available_methods.iter().any(|available| available == m))
}

fn additional_edits_allowed(ctx: &MultiTokenCompletionContext, completion: &MultiTokenCompletion) -> bool {
    completion.additional_edits.iter().all(|edit| match edit {
        AdditionalEdit::AddImport { path } => ctx.importable_paths.iter().any(|p| p == path),
    })
}

fn token_count(text: &str) -> usize {
    let mut count = 0usize;
    let mut in_token = false;

    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            // Skip VS Code snippet placeholders: $1 or ${1:foo}
            if matches!(chars.peek(), Some('{')) {
                // Consume until matching '}'
                while let Some(c) = chars.next() {
                    if c == '}' {
                        break;
                    }
                }
            } else {
                while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
                    chars.next();
                }
            }
            in_token = false;
            continue;
        }

        let is_token_char = ch.is_alphanumeric() || ch == '_';
        if is_token_char {
            if !in_token {
                count += 1;
                in_token = true;
            }
        } else {
            in_token = false;
        }
    }

    count
}

fn extract_top_level_method_calls(text: &str) -> Vec<String> {
    let mut calls = Vec::new();
    let mut paren_depth = 0usize;

    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '(' => {
                paren_depth = paren_depth.saturating_add(1);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
            }
            '$' => {
                // Skip snippet placeholders at any depth.
                if matches!(chars.peek(), Some('{')) {
                    while let Some(c) = chars.next() {
                        if c == '}' {
                            break;
                        }
                    }
                } else {
                    while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
                        chars.next();
                    }
                }
            }
            _ => {
                if paren_depth != 0 {
                    continue;
                }

                if is_ident_start(ch) {
                    let mut ident = String::new();
                    ident.push(ch);
                    while matches!(chars.peek(), Some(c) if is_ident_continue(*c)) {
                        ident.push(chars.next().expect("peeked"));
                    }

                    // Skip whitespace.
                    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
                        chars.next();
                    }

                    if matches!(chars.peek(), Some('(')) {
                        calls.push(ident);
                    }
                }
            }
        }
    }

    calls
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_ai::{AdditionalEdit, MultiTokenCompletion, MultiTokenCompletionContext, MultiTokenInsertTextFormat};

    fn ctx() -> MultiTokenCompletionContext {
        MultiTokenCompletionContext {
            receiver_type: Some("Stream<Person>".into()),
            expected_type: Some("List<String>".into()),
            surrounding_code: "people.stream().".into(),
            available_methods: vec!["filter".into(), "map".into(), "collect".into()],
            importable_paths: vec!["java.util.stream.Collectors".into()],
        }
    }

    #[test]
    fn top_level_method_extraction_ignores_lambda_body() {
        let completion = MultiTokenCompletion {
            label: "chain".into(),
            insert_text: "filter(p -> p.isActive()).map(Person::getName).collect(Collectors.toList())"
                .into(),
            format: MultiTokenInsertTextFormat::PlainText,
            additional_edits: vec![AdditionalEdit::AddImport {
                path: "java.util.stream.Collectors".into(),
            }],
            confidence: 0.9,
        };

        let calls = extract_top_level_method_calls(&completion.insert_text);
        assert_eq!(calls, vec!["filter", "map", "collect"]);
        assert!(validate_ai_completion(&ctx(), &completion, &CompletionConfig::default()));
    }

    #[test]
    fn rejects_unknown_top_level_method() {
        let completion = MultiTokenCompletion {
            label: "bad".into(),
            insert_text: "unknown().map(x -> x)".into(),
            format: MultiTokenInsertTextFormat::PlainText,
            additional_edits: vec![],
            confidence: 0.1,
        };

        assert!(!validate_ai_completion(&ctx(), &completion, &CompletionConfig::default()));
    }

    #[test]
    fn rejects_unimportable_additional_edit() {
        let completion = MultiTokenCompletion {
            label: "bad import".into(),
            insert_text: "filter(x -> true)".into(),
            format: MultiTokenInsertTextFormat::PlainText,
            additional_edits: vec![AdditionalEdit::AddImport {
                path: "com.example.NotAllowed".into(),
            }],
            confidence: 0.5,
        };

        assert!(!validate_ai_completion(&ctx(), &completion, &CompletionConfig::default()));
    }

    #[test]
    fn token_count_skips_snippet_placeholders() {
        let tokens = token_count("filter(${1:p} -> ${1:p}.isActive())");
        assert!(tokens > 0);
        assert_eq!(token_count("filter(${123:foo})"), 1);
    }
}
