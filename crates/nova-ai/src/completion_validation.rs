use crate::{AdditionalEdit, MultiTokenCompletion, MultiTokenCompletionContext};

/// Validate an AI-generated multi-token completion against best-effort semantic constraints.
///
/// This function is intentionally heuristic-based and must remain deterministic so we can
/// regression-test prompt/output handling without making live model calls.
pub fn validate_multi_token_completion(
    ctx: &MultiTokenCompletionContext,
    completion: &MultiTokenCompletion,
    max_additional_edits: usize,
    max_tokens: usize,
) -> bool {
    if completion.insert_text.trim().is_empty() {
        return false;
    }

    if completion.additional_edits.len() > max_additional_edits {
        return false;
    }

    if token_count(&completion.insert_text) > max_tokens {
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

