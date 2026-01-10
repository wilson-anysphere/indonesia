use crate::{types::CodeSnippet, AiError};
use globset::{Glob, GlobSet, GlobSetBuilder};
use nova_config::AiPrivacyConfig;
use regex::Regex;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::Path,
};

/// Privacy filtering for LLM backends configured via `nova-config`.
///
/// This sits alongside (and intentionally separate from) `nova_ai::privacy`,
/// which focuses on prompt-building and token redaction/anonymization heuristics.
pub struct PrivacyFilter {
    excluded_paths: GlobSet,
    redact_patterns: Vec<Regex>,
    anonymize_code: bool,
}

impl PrivacyFilter {
    pub fn new(config: &AiPrivacyConfig) -> Result<Self, AiError> {
        let mut excluded_builder = GlobSetBuilder::new();
        for pattern in &config.excluded_paths {
            let glob = Glob::new(pattern).map_err(|err| {
                AiError::InvalidConfig(format!("invalid excluded_paths glob {pattern:?}: {err}"))
            })?;
            excluded_builder.add(glob);
        }

        let excluded_paths = excluded_builder.build().map_err(|err| {
            AiError::InvalidConfig(format!("failed to build excluded_paths globset: {err}"))
        })?;

        let mut redact_patterns = Vec::new();
        for pattern in &config.redact_patterns {
            let re = Regex::new(pattern).map_err(|err| {
                AiError::InvalidConfig(format!("invalid redact_patterns regex {pattern:?}: {err}"))
            })?;
            redact_patterns.push(re);
        }

        Ok(Self {
            excluded_paths,
            redact_patterns,
            anonymize_code: config.effective_anonymize(),
        })
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        self.excluded_paths.is_match(path)
    }

    /// Apply redaction patterns to arbitrary prompt text.
    pub fn sanitize_prompt_text(&self, text: &str) -> String {
        self.apply_redaction(text)
    }

    /// Apply redaction and (optionally) anonymization to code before sending it to an LLM.
    pub fn sanitize_code_text(&self, code: &str) -> String {
        let redacted = self.apply_redaction(code);
        if self.anonymize_code {
            anonymize_java_like_identifiers(&redacted)
        } else {
            redacted
        }
    }

    pub fn sanitize_snippet(&self, snippet: &CodeSnippet) -> Option<String> {
        if let Some(path) = snippet.path.as_deref() {
            if self.is_excluded(path) {
                return None;
            }
        }

        Some(self.sanitize_code_text(&snippet.content))
    }

    fn apply_redaction(&self, text: &str) -> String {
        let mut output = text.to_string();
        for re in &self.redact_patterns {
            output = re.replace_all(&output, "[REDACTED]").into_owned();
        }
        output
    }
}

fn anonymize_java_like_identifiers(text: &str) -> String {
    let identifier_re =
        Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]*\b").expect("static identifier regex is valid");
    let counter = Cell::new(0usize);
    let map = RefCell::new(HashMap::<String, String>::new());

    identifier_re
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let ident = &caps[0];
            if is_java_keyword(ident) {
                return ident.to_string();
            }

            let mut map = map.borrow_mut();
            if let Some(existing) = map.get(ident) {
                return existing.clone();
            }

            let replacement = format!("id_{}", counter.get());
            counter.set(counter.get() + 1);
            map.insert(ident.to_string(), replacement.clone());
            replacement
        })
        .into_owned()
}

fn is_java_keyword(ident: &str) -> bool {
    matches!(
        ident,
        // Literals
        "true"
            | "false"
            | "null"
            // Java keywords
            | "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            // Newer keywords
            | "var"
            | "record"
            | "sealed"
            | "permits"
            | "yield"
            | "non"
    )
}

