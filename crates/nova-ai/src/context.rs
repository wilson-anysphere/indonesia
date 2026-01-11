use crate::anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};
use crate::privacy::PrivacyMode;
use std::ops::Range;

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

        let options = CodeAnonymizerOptions {
            anonymize_identifiers: req.privacy.anonymize_identifiers,
            redact_sensitive_strings: req.privacy.redaction.redact_string_literals,
            redact_numeric_literals: req.privacy.redaction.redact_numeric_literals,
            // When we're anonymizing identifiers, comment contents are likely to
            // contain project-specific identifiers and secrets; strip them.
            strip_or_redact_comments: req.privacy.anonymize_identifiers,
        };
        let mut anonymizer = CodeAnonymizer::new(options);

        // Focal code is always included, even if it needs truncation.
        let (section, section_truncated, used) = build_section(
            "Focal code",
            &req.focal_code,
            remaining,
            &mut anonymizer,
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
                &mut anonymizer,
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
                    &mut anonymizer,
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
                    &mut anonymizer,
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
                    &mut anonymizer,
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

impl ContextRequest {
    /// Build a context request from a Java source buffer + a byte-range selection.
    ///
    /// This is a best-effort extractor that uses Nova's Java syntax parser to find:
    /// - The focal code region (the given selection range).
    /// - The enclosing method (if any) and enclosing type declaration.
    /// - The nearest leading doc comment (optional).
    ///
    /// Callers can still populate `related_symbols` if they have richer semantic data.
    pub fn for_java_source_range(
        source: &str,
        selection: Range<usize>,
        token_budget: usize,
        privacy: PrivacyMode,
        include_doc_comments: bool,
    ) -> Self {
        let selection = clamp_range(selection, source.len());
        let focal_code = source[selection.clone()].to_string();

        let extracted = extract_java_context(source, selection.clone(), include_doc_comments);

        Self {
            file_path: None,
            focal_code,
            enclosing_context: extracted.enclosing_context,
            related_symbols: Vec::new(),
            doc_comments: extracted.doc_comment,
            include_doc_comments,
            token_budget,
            privacy,
        }
    }
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

#[derive(Debug, Clone)]
struct ExtractedJavaContext {
    enclosing_context: Option<String>,
    doc_comment: Option<String>,
}

fn clamp_range(range: Range<usize>, len: usize) -> Range<usize> {
    let start = range.start.min(len);
    let end = range.end.min(len).max(start);
    start..end
}

fn extract_java_context(
    source: &str,
    selection: Range<usize>,
    include_doc_comments: bool,
) -> ExtractedJavaContext {
    use nova_syntax::{parse_java, AstNode, CompilationUnit, SyntaxElement, SyntaxKind};

    if source.is_empty() {
        return ExtractedJavaContext {
            enclosing_context: None,
            doc_comment: None,
        };
    }

    let parse = parse_java(source);
    let range = nova_syntax::TextRange::new(selection.start, selection.end);
    let element = parse.covering_element(range);

    let node = match element {
        SyntaxElement::Node(n) => n,
        SyntaxElement::Token(t) => match t.parent() {
            Some(p) => p,
            None => {
                return ExtractedJavaContext {
                    enclosing_context: None,
                    doc_comment: None,
                }
            }
        },
    };

    let mut method = None;
    let mut ty = None;
    for anc in node.ancestors() {
        match anc.kind() {
            SyntaxKind::MethodDeclaration if method.is_none() => method = Some(anc.clone()),
            SyntaxKind::ClassDeclaration
            | SyntaxKind::InterfaceDeclaration
            | SyntaxKind::EnumDeclaration
            | SyntaxKind::RecordDeclaration
                if ty.is_none() =>
            {
                ty = Some(anc.clone())
            }
            _ => {}
        }
        if method.is_some() && ty.is_some() {
            break;
        }
    }

    let mut parts: Vec<String> = Vec::new();

    let root = parse.syntax();
    if let Some(unit) = CompilationUnit::cast(root) {
        if let Some(pkg) = unit.package() {
            parts.push(format!(
                "// Package\n{}",
                slice_node_without_leading_trivia(source, pkg.syntax(), true).trim()
            ));
        }

        let imports: Vec<_> = unit
            .imports()
            .map(|imp| {
                slice_node_without_leading_trivia(source, imp.syntax(), true)
                    .trim()
                    .to_string()
            })
            .collect();
        if !imports.is_empty() {
            parts.push(format!("// Imports\n{}", imports.join("\n")));
        }
    }

    if let Some(ty) = &ty {
        parts.push(format!(
            "// Enclosing type\n{}",
            slice_node_without_leading_trivia(source, ty, include_doc_comments).trim()
        ));
    }

    if let Some(method) = &method {
        parts.push(format!(
            "// Enclosing method\n{}",
            slice_node_without_leading_trivia(source, method, include_doc_comments).trim()
        ));
    }

    let doc_comment = if include_doc_comments {
        // Prefer method doc, else type doc.
        method
            .as_ref()
            .and_then(|n| find_doc_comment_before_node(source, n))
            .or_else(|| {
                ty.as_ref()
                    .and_then(|n| find_doc_comment_before_node(source, n))
            })
    } else {
        None
    };

    ExtractedJavaContext {
        enclosing_context: if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        },
        doc_comment,
    }
}

fn slice_node_without_leading_trivia<'a>(
    source: &'a str,
    node: &nova_syntax::SyntaxNode,
    include_doc_comments: bool,
) -> &'a str {
    let range = node.text_range();
    let mut start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;

    if !include_doc_comments {
        // Skip leading trivia (including doc comments) so doc inclusion is controlled
        // exclusively via `doc_comments`.
        if let Some(mut tok) = node.first_token() {
            let node_end = range.end();
            while tok.text_range().start() < node_end && tok.kind().is_trivia() {
                start = u32::from(tok.text_range().end()) as usize;
                if let Some(next) = tok.next_token() {
                    tok = next;
                } else {
                    break;
                }
            }
        }
    }

    start = start.min(source.len());
    let end = end.min(source.len()).max(start);
    &source[start..end]
}

fn find_doc_comment_before_node(source: &str, node: &nova_syntax::SyntaxNode) -> Option<String> {
    let offset = u32::from(node.text_range().start()) as usize;
    find_doc_comment_before_offset(source, offset)
}

fn find_doc_comment_before_offset(source: &str, offset: usize) -> Option<String> {
    use nova_syntax::SyntaxKind;

    let tokens = nova_syntax::lex(source);
    let mut idx = 0usize;
    while idx < tokens.len() {
        let end = tokens[idx].range.end as usize;
        if end > offset {
            break;
        }
        idx += 1;
    }

    while idx > 0 {
        idx -= 1;
        let tok = &tokens[idx];
        match tok.kind {
            SyntaxKind::Whitespace | SyntaxKind::LineComment | SyntaxKind::BlockComment => continue,
            SyntaxKind::DocComment => return Some(tok.text(source).to_string()),
            _ => break,
        }
    }

    None
}

fn build_section(
    title: &str,
    raw_content: &str,
    remaining: usize,
    anonymizer: &mut CodeAnonymizer,
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

    let content = anonymizer.anonymize(raw_content);

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
            focal_code: r#"class Secret { String apiKey = "sk-verysecretstringthatislong"; }"#
                .to_string(),
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

    #[test]
    fn java_source_range_extracts_enclosing_context_and_docs() {
        let source = r#"
package com.example;

/** Class docs */
public class Foo {
  /** Method docs */
  public void bar() {
    int x = 0;
  }
}
"#;

        let start = source.find("int x").unwrap();
        let end = start + "int x = 0;".len();

        let req = ContextRequest::for_java_source_range(
            source,
            start..end,
            200,
            PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
            /*include_doc_comments=*/ true,
        );

        let enclosing = req.enclosing_context.as_deref().unwrap();
        assert!(enclosing.contains("package com.example"));
        assert!(enclosing.contains("class Foo"));
        assert!(enclosing.contains("void bar"));

        let docs = req.doc_comments.as_deref().unwrap();
        assert!(docs.contains("Method docs"));
    }
}
