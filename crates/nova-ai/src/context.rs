use crate::privacy::{redact_suspicious_literals, CodeAnonymizer, PrivacyMode};

#[derive(Debug, Clone)]
pub struct ContextBuilder;

impl ContextBuilder {
    pub fn new() -> Self {
        Self
    }

    pub fn build(&self, req: ContextRequest) -> BuiltContext {
        let mut remaining = req.token_budget;
        let mut out = String::new();
        let mut truncated = false;

        let mut anonymizer = if req.privacy.anonymize_identifiers {
            Some(CodeAnonymizer::new())
        } else {
            None
        };

        // Focal code is always included, even if it needs truncation.
        let (section, section_truncated, used) = build_section(
            "Focal code",
            &req.focal_code,
            remaining,
            &req.privacy,
            anonymizer.as_mut(),
            /*always_include=*/ true,
        );
        out.push_str(&section);
        truncated |= section_truncated;
        remaining = remaining.saturating_sub(used);

        // Enclosing context is next most useful.
        if let Some(enclosing) = req.enclosing_context.as_deref() {
            let (section, section_truncated, used) = build_section(
                "Enclosing context",
                enclosing,
                remaining,
                &req.privacy,
                anonymizer.as_mut(),
                /*always_include=*/ false,
            );
            out.push_str(&section);
            truncated |= section_truncated;
            remaining = remaining.saturating_sub(used);
        }

        // Related symbols in provided order (caller can pre-sort by relevance).
        if !req.related_symbols.is_empty() && remaining > 0 {
            for symbol in &req.related_symbols {
                if remaining == 0 {
                    break;
                }
                let title = if req.privacy.anonymize_identifiers {
                    format!("Related symbol ({})", symbol.kind)
                } else {
                    format!("Related symbol: {} ({})", symbol.name, symbol.kind)
                };
                let (section, section_truncated, used) = build_section(
                    &title,
                    &symbol.snippet,
                    remaining,
                    &req.privacy,
                    anonymizer.as_mut(),
                    /*always_include=*/ false,
                );
                out.push_str(&section);
                truncated |= section_truncated;
                remaining = remaining.saturating_sub(used);
            }
        }

        if req.include_doc_comments {
            if let Some(docs) = req.doc_comments.as_deref() {
                let (section, section_truncated, used) = build_section(
                    "Doc comments",
                    docs,
                    remaining,
                    &req.privacy,
                    anonymizer.as_mut(),
                    /*always_include=*/ false,
                );
                out.push_str(&section);
                truncated |= section_truncated;
                remaining = remaining.saturating_sub(used);
            }
        }

        // Optional path metadata (kept last so it doesn't crowd out code).
        if req.privacy.include_file_paths {
            if let Some(path) = req.file_path.as_deref() {
                let (section, section_truncated, _used) = build_section(
                    "File",
                    path,
                    remaining,
                    &req.privacy,
                    anonymizer.as_mut(),
                    /*always_include=*/ false,
                );
                out.push_str(&section);
                truncated |= section_truncated;
            }
        }

        let token_count = count_tokens(&out);
        // Hard budget enforcement: never exceed the requested budget.
        if token_count > req.token_budget {
            out = truncate_to_tokens(&out, req.token_budget);
            truncated = true;
        }

        let token_count = count_tokens(&out);
        BuiltContext {
            text: out,
            token_count,
            truncated,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub file_path: Option<String>,
    pub focal_code: String,
    pub enclosing_context: Option<String>,
    pub related_symbols: Vec<RelatedSymbol>,
    pub doc_comments: Option<String>,
    pub include_doc_comments: bool,
    pub token_budget: usize,
    pub privacy: PrivacyMode,
}

#[derive(Debug, Clone)]
pub struct RelatedSymbol {
    pub name: String,
    pub kind: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct BuiltContext {
    pub text: String,
    pub token_count: usize,
    pub truncated: bool,
}

fn build_section(
    title: &str,
    raw_content: &str,
    remaining: usize,
    privacy: &PrivacyMode,
    mut anonymizer: Option<&mut CodeAnonymizer>,
    always_include: bool,
) -> (String, bool, usize) {
    if remaining == 0 && !always_include {
        return (String::new(), false, 0);
    }

    let header = format!("## {title}\n");
    let header_tokens = count_tokens(&header);

    if !always_include && header_tokens >= remaining {
        return (String::new(), false, 0);
    }

    let mut content = raw_content.to_string();
    content = redact_suspicious_literals(&content, &privacy.redaction);
    if privacy.anonymize_identifiers {
        if let Some(anonymizer) = anonymizer.as_mut() {
            content = anonymizer.anonymize(&content);
        }
    }

    let allowed_tokens = remaining.saturating_sub(header_tokens);
    let current_tokens = count_tokens(&content);
    let truncated = current_tokens > allowed_tokens;
    let content = truncate_to_tokens(&content, allowed_tokens);
    let section = format!("{header}{content}\n\n");

    let used = count_tokens(&section);
    (section, truncated, used)
}

fn count_tokens(text: &str) -> usize {
    text.split_whitespace().count()
}

fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }

    let mut token_count = 0usize;
    let mut in_token = false;
    let mut last_good_end = 0usize;

    for (idx, ch) in text.char_indices() {
        if ch.is_whitespace() {
            in_token = false;
            continue;
        }

        if !in_token {
            token_count += 1;
            if token_count > max_tokens {
                break;
            }
            in_token = true;
        }

        last_good_end = idx + ch.len_utf8();
    }

    text[..last_good_end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_builder_enforces_budget_and_privacy() {
        let builder = ContextBuilder::new();
        let req = ContextRequest {
            file_path: Some("/home/user/project/Secret.java".to_string()),
            focal_code: r#"class Secret { String apiKey = "sk-verysecretstringthatislong"; }"#.to_string(),
            enclosing_context: Some("package com.example;\n".to_string()),
            related_symbols: vec![RelatedSymbol {
                name: "Secret".to_string(),
                kind: "class".to_string(),
                snippet: "class Secret {}".to_string(),
            }],
            doc_comments: Some("/** Javadoc mentioning Secret */".to_string()),
            include_doc_comments: true,
            token_budget: 20,
            privacy: PrivacyMode {
                anonymize_identifiers: true,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
        };

        let built = builder.build(req.clone());
        assert!(built.token_count <= 20);

        // Paths excluded.
        assert!(!built.text.contains("/home/user"));

        // Suspicious string redacted.
        assert!(built.text.contains("\"[REDACTED]\""));

        // Identifiers anonymized.
        assert!(!built.text.contains("Secret"));

        // Stability: same input yields same output.
        let built2 = builder.build(req);
        assert_eq!(built.text, built2.text);
    }
}
