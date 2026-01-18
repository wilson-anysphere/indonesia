//! Spring Boot configuration support (Spring "externalized configuration").
//!
//! This module provides editor-facing configuration intelligence:
//! - Parse `application.properties` and `application.yml` / `application.yaml`
//! - Ingest Spring Boot `spring-configuration-metadata.json`
//! - Diagnostics for unknown/deprecated keys, duplicate keys (properties), and
//!   best-effort primitive type mismatches
//! - Completions + navigation for `@Value("${...}")` usages in Java source

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_config_metadata::{MetadataIndex, PropertyMeta};
use nova_core::TextRange;
use nova_types::{CompletionItem, Diagnostic, Span};

pub const SPRING_UNKNOWN_CONFIG_KEY: &str = "SPRING_UNKNOWN_CONFIG_KEY";
pub const SPRING_DEPRECATED_CONFIG_KEY: &str = "SPRING_DEPRECATED_CONFIG_KEY";
pub const SPRING_DUPLICATE_CONFIG_KEY: &str = "SPRING_DUPLICATE_CONFIG_KEY";
pub const SPRING_CONFIG_TYPE_MISMATCH: &str = "SPRING_CONFIG_TYPE_MISMATCH";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigLocation {
    pub path: PathBuf,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct SpringWorkspaceIndex {
    metadata: Arc<MetadataIndex>,
    definitions: HashMap<String, Vec<ConfigLocation>>,
    usages: HashMap<String, Vec<ConfigLocation>>,
    observed_keys: HashSet<String>,
    observed_prefixes: HashSet<String>,
}

impl SpringWorkspaceIndex {
    #[must_use]
    pub fn new(metadata: Arc<MetadataIndex>) -> Self {
        Self {
            metadata,
            definitions: HashMap::new(),
            usages: HashMap::new(),
            observed_keys: HashSet::new(),
            observed_prefixes: HashSet::new(),
        }
    }

    #[must_use]
    pub fn metadata(&self) -> &MetadataIndex {
        self.metadata.as_ref()
    }

    pub fn add_config_file(&mut self, path: impl Into<PathBuf>, text: &str) {
        let path = path.into();
        for entry in parse_config_entries(&path, text) {
            self.definitions
                .entry(entry.key.clone())
                .or_default()
                .push(ConfigLocation {
                    path: path.clone(),
                    span: entry.key_span,
                });
            self.observed_keys.insert(entry.key);
        }
    }

    pub fn add_java_file(&mut self, path: impl Into<PathBuf>, text: &str) {
        let path = path.into();
        for usage in scan_java_value_placeholders(text) {
            self.usages
                .entry(usage.key.clone())
                .or_default()
                .push(ConfigLocation {
                    path: path.clone(),
                    span: usage.span,
                });
            self.observed_keys.insert(usage.key);
        }

        // `@ConfigurationProperties(prefix="...")` implies that all keys with that
        // prefix are relevant to the project. As a best-effort heuristic, we
        // treat metadata keys under that prefix as observed.
        for prefix in scan_java_configuration_properties_prefixes(text) {
            self.observed_prefixes.insert(prefix.clone());
            if self.metadata.is_empty() {
                continue;
            }

            let prefix = if prefix.ends_with('.') {
                prefix
            } else {
                format!("{prefix}.")
            };
            for meta in self.metadata.known_properties(&prefix) {
                self.observed_keys.insert(meta.name.clone());
            }
        }
    }

    #[must_use]
    pub fn definitions_for(&self, key: &str) -> &[ConfigLocation] {
        self.definitions.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    #[must_use]
    pub fn usages_for(&self, key: &str) -> &[ConfigLocation] {
        self.usages.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    #[must_use]
    pub fn observed_keys(&self) -> impl Iterator<Item = &String> {
        self.observed_keys.iter()
    }

    #[must_use]
    pub fn observed_prefixes(&self) -> impl Iterator<Item = &String> {
        self.observed_prefixes.iter()
    }
}

#[derive(Clone, Debug)]
struct ConfigEntry {
    key: String,
    value: String,
    key_span: Span,
    value_span: Span,
}

fn parse_config_entries(path: &Path, text: &str) -> Vec<ConfigEntry> {
    let ext = path.extension().and_then(|e| e.to_str());
    if ext.is_some_and(|ext| ext.eq_ignore_ascii_case("properties")) {
        return nova_properties::parse(text)
            .entries
            .into_iter()
            .map(|e| ConfigEntry {
                key: e.key,
                value: e.value,
                key_span: text_range_to_span(e.key_range),
                value_span: text_range_to_span(e.value_range),
            })
            .collect();
    }

    if ext.is_some_and(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml")) {
        return nova_yaml::parse(text)
            .entries
            .into_iter()
            .map(|e| ConfigEntry {
                key: e.key,
                value: e.value,
                key_span: text_range_to_span(e.key_range),
                value_span: text_range_to_span(e.value_range),
            })
            .collect();
    }

    Vec::new()
}

fn text_range_to_span(range: TextRange) -> Span {
    Span::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

/// Produce Spring configuration diagnostics for a single config file.
#[must_use]
pub fn diagnostics_for_config_file(
    path: &Path,
    text: &str,
    metadata: &MetadataIndex,
) -> Vec<Diagnostic> {
    let entries = parse_config_entries(path, text);
    let mut diagnostics = Vec::new();

    // Duplicate detection is only meaningful for `.properties`, which Spring
    // resolves sequentially.
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("properties"))
    {
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for entry in &entries {
            if let Some(prev_span) = seen.insert(entry.key.as_str(), entry.key_span) {
                diagnostics.push(Diagnostic::warning(
                    SPRING_DUPLICATE_CONFIG_KEY,
                    format!("Duplicate configuration key '{}'", entry.key),
                    Some(entry.key_span),
                ));
                diagnostics.push(Diagnostic::warning(
                    SPRING_DUPLICATE_CONFIG_KEY,
                    format!("Duplicate configuration key '{}'", entry.key),
                    Some(prev_span),
                ));
            }
        }
    }

    if metadata.is_empty() {
        return diagnostics;
    }

    for entry in entries {
        let Some(prop) = metadata.property_meta(&entry.key) else {
            diagnostics.push(Diagnostic::warning(
                SPRING_UNKNOWN_CONFIG_KEY,
                format!("Unknown Spring configuration key '{}'", entry.key),
                Some(entry.key_span),
            ));
            continue;
        };

        if let Some(deprecation) = prop.deprecation.as_ref() {
            let mut message = format!("Deprecated Spring configuration key '{}'", entry.key);
            if let Some(replacement) = deprecation
                .replacement
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                message.push_str(&format!("; use '{replacement}'"));
            }
            diagnostics.push(Diagnostic::warning(
                SPRING_DEPRECATED_CONFIG_KEY,
                message,
                Some(entry.key_span),
            ));
        }

        if let Some(message) = validate_value_type(prop, &entry.value) {
            diagnostics.push(Diagnostic::warning(
                SPRING_CONFIG_TYPE_MISMATCH,
                message,
                Some(entry.value_span),
            ));
        }
    }

    diagnostics
}

fn validate_value_type(meta: &PropertyMeta, value: &str) -> Option<String> {
    let ty = meta.ty.as_deref()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if is_integer_type(ty) {
        if trimmed.parse::<i64>().is_err() {
            return Some(format!(
                "Expected an integer for '{}' but got '{}'",
                meta.name, value
            ));
        }
    } else if is_boolean_type(ty) {
        match trimmed {
            "true" | "false" => {}
            _ => {
                return Some(format!(
                    "Expected a boolean for '{}' but got '{}'",
                    meta.name, value
                ));
            }
        }
    }

    None
}

fn is_integer_type(ty: &str) -> bool {
    matches!(
        ty,
        "int" | "java.lang.Integer" | "long" | "java.lang.Long" | "java.lang.Short"
    )
}

fn is_boolean_type(ty: &str) -> bool {
    matches!(ty, "boolean" | "java.lang.Boolean")
}

#[derive(Clone, Debug)]
struct JavaUsage {
    key: String,
    span: Span,
}

fn scan_java_value_placeholders(text: &str) -> Vec<JavaUsage> {
    let mut usages = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = text[search..].find("@Value") {
        let start = search + rel;
        let Some(open_paren) = text[start..].find('(').map(|o| start + o) else {
            break;
        };
        let Some(open_quote) = find_next_unescaped_quote(text, open_paren) else {
            search = open_paren + 1;
            continue;
        };
        let Some(close_quote) = find_next_unescaped_quote(text, open_quote + 1) else {
            search = open_quote + 1;
            continue;
        };

        let content_start = open_quote + 1;
        let content_end = close_quote;
        let content = &text[content_start..content_end];

        for (key, span) in extract_placeholders_in_string(content, content_start) {
            usages.push(JavaUsage { key, span });
        }

        search = close_quote + 1;
    }
    usages
}

fn scan_java_configuration_properties_prefixes(text: &str) -> Vec<String> {
    let mut prefixes = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = text[search..].find("@ConfigurationProperties") {
        let start = search + rel;
        let Some(open_paren) = text[start..].find('(').map(|o| start + o) else {
            break;
        };
        let Some(close_paren) = text[open_paren..].find(')').map(|c| open_paren + c) else {
            break;
        };

        let args = &text[open_paren + 1..close_paren];
        if let Some(prefix) = extract_named_string_arg(args, "prefix") {
            prefixes.push(prefix);
        }

        search = close_paren + 1;
    }
    prefixes
}

fn extract_named_string_arg(args: &str, name: &str) -> Option<String> {
    let mut search = 0usize;
    while let Some(rel) = args[search..].find(name) {
        let start = search + rel;
        let after = &args[start + name.len()..];
        let after = after.trim_start();
        if !after.starts_with('=') {
            search = start + name.len();
            continue;
        }
        let after = after[1..].trim_start();
        let Some(open_quote) = after.find('"') else {
            return None;
        };
        let after_quote = &after[open_quote + 1..];
        let Some(close_quote) = after_quote.find('"') else {
            return None;
        };
        return Some(after_quote[..close_quote].to_string());
    }
    None
}

fn find_next_unescaped_quote(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let mut backslashes = 0usize;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn extract_placeholders_in_string(content: &str, absolute_start: usize) -> Vec<(String, Span)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = content[i..].find("${") {
        let open = i + rel;
        let key_start = open + 2;
        let rest = &content[key_start..];
        let close = rest.find('}').unwrap_or(rest.len());
        let placeholder = &rest[..close];
        let key = placeholder.split(':').next().unwrap_or("").trim();
        if !key.is_empty() {
            let start = absolute_start + key_start;
            let end = start + key.len();
            out.push((key.to_string(), Span::new(start, end)));
        }
        i = key_start + close + 1;
        if i > content.len() {
            break;
        }
    }
    out
}

#[derive(Clone, Debug)]
struct PlaceholderContext {
    /// Currently typed prefix within the placeholder.
    prefix: String,
    /// Best-effort full key in the placeholder (may be incomplete).
    key: String,
    /// Absolute byte offset of the key start (first char after `${`).
    key_start: usize,
}

fn placeholder_context_at(text: &str, offset: usize) -> Option<PlaceholderContext> {
    // Find the enclosing @Value string literal.
    let mut search = 0usize;
    while let Some(rel) = text[search..].find("@Value") {
        let start = search + rel;
        let Some(open_paren) = text[start..].find('(').map(|o| start + o) else {
            break;
        };
        let Some(open_quote) = find_next_unescaped_quote(text, open_paren) else {
            search = open_paren + 1;
            continue;
        };
        let Some(close_quote) = find_next_unescaped_quote(text, open_quote + 1) else {
            search = open_quote + 1;
            continue;
        };

        let content_start = open_quote + 1;
        let content_end = close_quote;
        if offset < content_start || offset > content_end {
            search = close_quote + 1;
            continue;
        }

        let content = &text[content_start..content_end];
        let rel_offset = offset - content_start;
        let Some(open_rel) = content[..rel_offset].rfind("${") else {
            return None;
        };
        let mut key_start_rel = open_rel + 2;
        while key_start_rel < content.len()
            && content.as_bytes()[key_start_rel].is_ascii_whitespace()
        {
            key_start_rel += 1;
        }
        if rel_offset < key_start_rel {
            return None;
        }

        let after_key = &content[key_start_rel..];
        let key_end_rel = after_key
            .find(|c| c == '}' || c == ':')
            .unwrap_or(after_key.len())
            + key_start_rel;
        if rel_offset > key_end_rel {
            return None;
        }

        let key = content[key_start_rel..key_end_rel].trim().to_string();
        let prefix = content[key_start_rel..rel_offset].trim().to_string();
        let key_start = content_start + key_start_rel;
        return Some(PlaceholderContext {
            prefix,
            key,
            key_start,
        });
    }

    None
}

/// Compute the replacement span for completions inside `@Value("${...}")`.
///
/// The returned span covers the placeholder key prefix (start of key to cursor),
/// so inserting a full key like `server.port` replaces `server.p` instead of
/// only the last identifier segment (`p`).
#[must_use]
pub fn completion_span_for_value_placeholder(java_source: &str, offset: usize) -> Option<Span> {
    let ctx = placeholder_context_at(java_source, offset)?;
    Some(Span::new(ctx.key_start, offset))
}

/// Compute the replacement span for completions inside `.properties` files.
///
/// This uses the range-preserving parser to locate the surrounding key/value
/// token and returns the span from its start to the cursor.
#[must_use]
pub fn completion_span_for_properties_file(path: &Path, text: &str, offset: usize) -> Option<Span> {
    let entries = parse_config_entries(path, text);
    for entry in entries {
        if entry.key_span.start <= offset && offset <= entry.key_span.end {
            return Some(Span::new(entry.key_span.start, offset));
        }
        if entry.value_span.start <= offset && offset <= entry.value_span.end {
            return Some(Span::new(entry.value_span.start, offset));
        }
    }
    None
}

/// Compute the replacement span for completions inside `application.yml` / `application.yaml`.
///
/// For key completions, the span covers the current mapping key token (start of
/// key to cursor). For value completions (`key: <cursor>`), the span covers the
/// scalar value token.
#[must_use]
pub fn completion_span_for_yaml_file(text: &str, offset: usize) -> Option<Span> {
    if let Some(value_start) = yaml_value_completion_start(text, offset) {
        return Some(Span::new(value_start, offset));
    }
    yaml_key_completion_span(text, offset)
}

/// Provide Spring configuration key completions inside `@Value("${...}")`.
#[must_use]
pub fn completions_for_value_placeholder(
    java_source: &str,
    offset: usize,
    index: &SpringWorkspaceIndex,
) -> Vec<CompletionItem> {
    let Some(ctx) = placeholder_context_at(java_source, offset) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut items = Vec::new();

    for meta in index.metadata.known_properties(&ctx.prefix) {
        if seen.insert(meta.name.clone()) {
            items.push(CompletionItem {
                label: meta.name.clone(),
                detail: property_completion_detail(meta),
                replace_span: None,
            });
        }
    }

    // Fall back to observed keys from the workspace.
    let mut observed: Vec<_> = index
        .observed_keys()
        .filter(|k| k.starts_with(&ctx.prefix) && *k != &ctx.key)
        .cloned()
        .collect();
    observed.sort();
    for key in observed {
        if seen.insert(key.clone()) {
            items.push(CompletionItem {
                label: key,
                detail: None,
                replace_span: None,
            });
        }
    }

    items
}

/// Best-effort "go to definition" for `@Value("${foo.bar}")`.
#[must_use]
pub fn goto_definition_for_value_placeholder(
    java_source: &str,
    offset: usize,
    index: &SpringWorkspaceIndex,
) -> Vec<ConfigLocation> {
    let Some(ctx) = placeholder_context_at(java_source, offset) else {
        return Vec::new();
    };

    index.definitions_for(&ctx.key).to_vec()
}

/// Best-effort "find references" for a config key inside `@Value("${...}")`.
///
/// When `include_definitions` is `true`, this returns config-file definitions
/// (e.g. from `application.properties`) in addition to all observed Java usages.
#[must_use]
pub fn find_references_for_value_placeholder(
    java_source: &str,
    offset: usize,
    index: &SpringWorkspaceIndex,
    include_definitions: bool,
) -> Vec<ConfigLocation> {
    let Some(ctx) = placeholder_context_at(java_source, offset) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if include_definitions {
        out.extend(index.definitions_for(&ctx.key).iter().cloned());
    }
    out.extend(index.usages_for(&ctx.key).iter().cloned());
    out
}

/// Completion inside `application.properties` (key and value positions).
#[must_use]
pub fn completions_for_properties_file(
    path: &Path,
    text: &str,
    offset: usize,
    index: &SpringWorkspaceIndex,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = HashSet::<String>::new();

    let entries = parse_config_entries(path, text);
    let Some((is_value, key, prefix)) = properties_completion_context(text, offset, &entries)
    else {
        return Vec::new();
    };

    if is_value {
        if let Some(meta) = index.metadata.property_meta(&key) {
            let mut candidates: Vec<String> = meta.allowed_values.clone();
            if candidates.is_empty() && is_boolean_type(meta.ty.as_deref().unwrap_or("")) {
                candidates.extend(["true".into(), "false".into()]);
            }

            candidates.sort();
            candidates.dedup();
            for value in candidates {
                if !value.starts_with(&prefix) {
                    continue;
                }
                if seen.insert(value.clone()) {
                    items.push(CompletionItem {
                        label: value,
                        detail: None,
                        replace_span: None,
                    });
                }
            }
        }

        return items;
    }

    // Key completion.
    for meta in index.metadata.known_properties(&prefix) {
        if seen.insert(meta.name.clone()) {
            items.push(CompletionItem {
                label: meta.name.clone(),
                detail: property_completion_detail(meta),
                replace_span: None,
            });
        }
    }

    let mut observed: Vec<_> = index
        .observed_keys()
        .filter(|k| k.starts_with(&prefix) && *k != &prefix)
        .cloned()
        .collect();
    observed.sort();
    observed.dedup();
    for key in observed {
        if seen.insert(key.clone()) {
            items.push(CompletionItem {
                label: key,
                detail: None,
                replace_span: None,
            });
        }
    }

    items
}

/// Completion inside `application.yml` / `application.yaml` for mapping keys.
///
/// This is best-effort: it suggests the next YAML segment based on indentation
/// context and Spring's dotted property keys.
#[must_use]
pub fn completions_for_yaml_file(
    _path: &Path,
    text: &str,
    offset: usize,
    index: &SpringWorkspaceIndex,
) -> Vec<CompletionItem> {
    // Value completion: `foo:\n  bar: <cursor>` -> suggest allowed values for `foo.bar`.
    if let Some((key, prefix)) = yaml_value_completion_context(text, offset) {
        if let Some(meta) = index.metadata.property_meta(&key) {
            let mut candidates: Vec<String> = meta.allowed_values.clone();
            if candidates.is_empty() && is_boolean_type(meta.ty.as_deref().unwrap_or("")) {
                candidates.extend(["true".into(), "false".into()]);
            }
            candidates.sort();
            candidates.dedup();

            let prefix = prefix.trim_matches('"');
            return candidates
                .into_iter()
                .filter(|value| value.starts_with(prefix))
                .map(|value| CompletionItem {
                    label: value,
                    detail: None,
                    replace_span: None,
                })
                .collect();
        }
        return Vec::new();
    }

    let Some((parent_prefix, typed_prefix)) = yaml_completion_context(text, offset) else {
        return Vec::new();
    };
    let full_prefix = format!("{parent_prefix}{typed_prefix}");

    let mut items = Vec::new();
    let mut seen = HashSet::<String>::new();

    for meta in index.metadata.known_properties(&full_prefix) {
        if let Some(segment) = next_yaml_segment(&meta.name, &parent_prefix) {
            if segment.starts_with(&typed_prefix) && seen.insert(segment.clone()) {
                let is_leaf = meta
                    .name
                    .strip_prefix(&parent_prefix)
                    .is_some_and(|remainder| remainder == segment);
                let detail = if is_leaf {
                    property_completion_detail(meta)
                } else {
                    None
                };
                items.push(CompletionItem {
                    label: segment,
                    detail,
                    replace_span: None,
                });
            }
        }
    }

    let mut observed: Vec<_> = index
        .observed_keys()
        .filter(|k| k.starts_with(&full_prefix))
        .cloned()
        .collect();
    observed.sort();
    observed.dedup();
    for key in observed {
        if let Some(segment) = next_yaml_segment(&key, &parent_prefix) {
            if segment.starts_with(&typed_prefix) && seen.insert(segment.clone()) {
                items.push(CompletionItem {
                    label: segment,
                    detail: None,
                    replace_span: None,
                });
            }
        }
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

fn property_completion_detail(meta: &PropertyMeta) -> Option<String> {
    let mut detail = String::new();

    if let Some(ty) = meta.ty.as_deref().filter(|s| !s.is_empty()) {
        detail.push_str(ty);
    }

    if let Some(default) = meta
        .default_value
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        if !detail.is_empty() {
            detail.push(' ');
        }
        detail.push_str(&format!("(default: {})", default.trim()));
    }

    if let Some(deprecation) = meta.deprecation.as_ref() {
        if !detail.is_empty() {
            detail.push(' ');
        }
        detail.push_str("[deprecated");
        if let Some(replacement) = deprecation.replacement.as_deref().filter(|s| !s.is_empty()) {
            detail.push_str(&format!(" → {}", replacement));
        }
        detail.push(']');
    }

    if let Some(desc) = meta.description.as_deref() {
        let desc = desc.lines().next().unwrap_or(desc).trim();
        if !desc.is_empty() {
            if !detail.is_empty() {
                detail.push_str(" — ");
            }
            const MAX_CHARS: usize = 80;
            let mut truncated: String = desc.chars().take(MAX_CHARS).collect();
            if desc.chars().skip(MAX_CHARS).next().is_some() {
                truncated.push('…');
            }
            detail.push_str(&truncated);
        }
    }

    if detail.is_empty() {
        None
    } else {
        Some(detail)
    }
}

/// Best-effort "find usages" from a config file key to Java `@Value` usages.
#[must_use]
pub fn goto_usages_for_config_key(
    path: &Path,
    text: &str,
    offset: usize,
    index: &SpringWorkspaceIndex,
) -> Vec<ConfigLocation> {
    let Some(key) = config_key_at_offset(path, text, offset) else {
        return Vec::new();
    };
    index.usages_for(&key).to_vec()
}

fn config_key_at_offset(path: &Path, text: &str, offset: usize) -> Option<String> {
    let entries = parse_config_entries(path, text);
    entries
        .into_iter()
        .find(|e| e.key_span.start <= offset && offset <= e.key_span.end)
        .map(|e| e.key)
}

fn properties_completion_context(
    text: &str,
    offset: usize,
    entries: &[ConfigEntry],
) -> Option<(bool, String, String)> {
    // First: use parsed entries (handles continuations/escapes/ranges).
    for entry in entries {
        if entry.key_span.start <= offset && offset <= entry.key_span.end {
            let prefix = text[entry.key_span.start..offset].trim().to_string();
            return Some((false, entry.key.clone(), prefix));
        }
        if entry.value_span.start <= offset && offset <= entry.value_span.end {
            let prefix = text[entry.value_span.start..offset].trim().to_string();
            return Some((true, entry.key.clone(), prefix));
        }
    }

    // Fallback: guess based on the current line.
    let (line_start, line_end) = line_bounds(text, offset);
    let line = text.get(line_start..line_end)?;
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
        return None;
    }

    let sep_idx = trimmed.find('=').or_else(|| trimmed.find(':'));
    let sep_idx = sep_idx.map(|idx| line.len() - trimmed.len() + idx);

    let key_start = line_start + (line.len() - trimmed.len());
    let key_end = line_start + sep_idx.unwrap_or(line.len());
    let is_value = sep_idx.is_some() && offset >= line_start + sep_idx.unwrap() + 1;

    let key = text
        .get(key_start..key_end)
        .unwrap_or("")
        .trim()
        .to_string();

    if key.is_empty() {
        return None;
    }

    if is_value {
        let value_start = (line_start + sep_idx.unwrap() + 1).min(text.len());
        let prefix = text[value_start..offset].trim().to_string();
        Some((true, key, prefix))
    } else {
        let prefix = text[key_start..offset].trim().to_string();
        Some((false, key, prefix))
    }
}

fn yaml_completion_context(text: &str, offset: usize) -> Option<(String, String)> {
    let (line_start, _line_end) = line_bounds(text, offset);
    let current_line = text.get(line_start..)?;
    let current_line = current_line
        .split_once('\n')
        .map(|(l, _)| l)
        .unwrap_or(current_line);
    let current_line = current_line.strip_suffix('\r').unwrap_or(current_line);

    let current_indent = current_line.bytes().take_while(|b| *b == b' ').count();

    let mut stack: Vec<(usize, String)> = Vec::new();
    for raw in text[..line_start].lines() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let indent = raw.bytes().take_while(|b| *b == b' ').count();
        let trimmed = raw[indent..].trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Only track mapping keys that open a nested block (`key:` with no value).
        let Some((key, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim_end();
        if key.is_empty() {
            continue;
        }

        while stack
            .last()
            .is_some_and(|(prev_indent, _)| *prev_indent >= indent)
        {
            stack.pop();
        }

        if rest.trim().is_empty() {
            stack.push((indent, key.to_string()));
        }
    }

    while stack
        .last()
        .is_some_and(|(prev_indent, _)| *prev_indent >= current_indent)
    {
        stack.pop();
    }

    let mut parent_prefix = stack
        .iter()
        .map(|(_, key)| key.as_str())
        .collect::<Vec<_>>()
        .join(".");
    if !parent_prefix.is_empty() {
        parent_prefix.push('.');
    }

    let after_indent = &current_line[current_indent..];
    let after_indent = if let Some(rest) = after_indent.strip_prefix("-") {
        rest.strip_prefix(' ').unwrap_or(rest)
    } else {
        after_indent
    };

    // Determine the token prefix on the current line.
    let token_end = offset.saturating_sub(line_start + current_indent);
    let typed_slice = &after_indent[..token_end.min(after_indent.len())];
    let typed = typed_slice
        .split(|c: char| c == ':' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    Some((parent_prefix, typed))
}

fn yaml_value_completion_context(text: &str, offset: usize) -> Option<(String, String)> {
    let (line_start, _line_end) = line_bounds(text, offset);
    let current_line = text.get(line_start..)?;
    let current_line = current_line
        .split_once('\n')
        .map(|(l, _)| l)
        .unwrap_or(current_line);
    let current_line = current_line.strip_suffix('\r').unwrap_or(current_line);

    let current_indent = current_line.bytes().take_while(|b| *b == b' ').count();

    let mut stack: Vec<(usize, String)> = Vec::new();
    for raw in text[..line_start].lines() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let indent = raw.bytes().take_while(|b| *b == b' ').count();
        let trimmed = raw[indent..].trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Only track mapping keys that open a nested block (`key:` with no value).
        let Some((key, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim_end();
        if key.is_empty() {
            continue;
        }

        while stack
            .last()
            .is_some_and(|(prev_indent, _)| *prev_indent >= indent)
        {
            stack.pop();
        }

        if rest.trim().is_empty() {
            stack.push((indent, key.to_string()));
        }
    }

    while stack
        .last()
        .is_some_and(|(prev_indent, _)| *prev_indent >= current_indent)
    {
        stack.pop();
    }

    let mut parent_prefix = stack
        .iter()
        .map(|(_, key)| key.as_str())
        .collect::<Vec<_>>()
        .join(".");
    if !parent_prefix.is_empty() {
        parent_prefix.push('.');
    }

    // Adjust for sequences (`- key: value`).
    let mut base_offset = line_start + current_indent;
    let mut after_indent = &current_line[current_indent..];
    if let Some(rest) = after_indent.strip_prefix('-') {
        base_offset += 1;
        after_indent = rest;
        if let Some(rest) = after_indent.strip_prefix(' ') {
            base_offset += 1;
            after_indent = rest;
        }
    }

    let rel_cursor = offset.saturating_sub(base_offset);
    let colon_rel = after_indent.find(':')?;
    if rel_cursor <= colon_rel {
        return None;
    }

    let key = after_indent[..colon_rel].trim_end();
    if key.is_empty() {
        return None;
    }

    let mut value_rel_start = colon_rel + 1;
    while value_rel_start < after_indent.len() && after_indent.as_bytes()[value_rel_start] == b' ' {
        value_rel_start += 1;
    }

    let value_abs_start = base_offset + value_rel_start;
    let prefix = if offset >= value_abs_start {
        text[value_abs_start..offset].trim().to_string()
    } else {
        String::new()
    };

    Some((format!("{parent_prefix}{key}"), prefix))
}

fn yaml_value_completion_start(text: &str, offset: usize) -> Option<usize> {
    let (line_start, _line_end) = line_bounds(text, offset);
    let current_line = text.get(line_start..)?;
    let current_line = current_line
        .split_once('\n')
        .map(|(l, _)| l)
        .unwrap_or(current_line);
    let current_line = current_line.strip_suffix('\r').unwrap_or(current_line);

    let current_indent = current_line.bytes().take_while(|b| *b == b' ').count();

    // Adjust for sequences (`- key: value`).
    let mut base_offset = line_start + current_indent;
    let mut after_indent = &current_line[current_indent..];
    if let Some(rest) = after_indent.strip_prefix('-') {
        base_offset += 1;
        after_indent = rest;
        if let Some(rest) = after_indent.strip_prefix(' ') {
            base_offset += 1;
            after_indent = rest;
        }
    }

    let rel_cursor = offset.saturating_sub(base_offset);
    let colon_rel = after_indent.find(':')?;
    if rel_cursor <= colon_rel {
        return None;
    }

    let mut value_rel_start = colon_rel + 1;
    while value_rel_start < after_indent.len() && after_indent.as_bytes()[value_rel_start] == b' ' {
        value_rel_start += 1;
    }
    let value_abs_start = base_offset + value_rel_start;
    (offset >= value_abs_start).then_some(value_abs_start)
}

fn yaml_key_completion_span(text: &str, offset: usize) -> Option<Span> {
    let (line_start, _line_end) = line_bounds(text, offset);
    let current_line = text.get(line_start..)?;
    let current_line = current_line
        .split_once('\n')
        .map(|(l, _)| l)
        .unwrap_or(current_line);
    let current_line = current_line.strip_suffix('\r').unwrap_or(current_line);

    let indent = current_line.bytes().take_while(|b| *b == b' ').count();
    let mut base_offset = line_start + indent;
    let mut after_indent = &current_line[indent..];
    if let Some(rest) = after_indent.strip_prefix('-') {
        base_offset += 1;
        after_indent = rest;
        if let Some(rest) = after_indent.strip_prefix(' ') {
            base_offset += 1;
            after_indent = rest;
        }
    }

    if offset < base_offset || base_offset > line_start + current_line.len() {
        return None;
    }

    let rel_cursor = offset - base_offset;
    let token_end = after_indent
        .find(|c: char| c == ':' || c.is_whitespace())
        .unwrap_or(after_indent.len());
    if rel_cursor > token_end {
        return None;
    }

    Some(Span::new(base_offset, offset))
}

fn next_yaml_segment(full_key: &str, parent_prefix: &str) -> Option<String> {
    let remainder = full_key.strip_prefix(parent_prefix).unwrap_or(full_key);
    let end = remainder
        .find(|c| c == '.' || c == '[')
        .unwrap_or(remainder.len());
    let seg = remainder[..end].trim();
    if seg.is_empty() {
        None
    } else {
        Some(seg.to_string())
    }
}

fn line_bounds(text: &str, offset: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    fn test_metadata() -> MetadataIndex {
        let mut index = MetadataIndex::new();
        index
            .ingest_json_bytes(
                br#"{
                "properties": [
                  { "name": "server.port", "type": "java.lang.Integer" },
                  { "name": "spring.main.banner-mode", "type": "java.lang.String",
                    "deprecation": { "level": "warning" }
                  }
                ],
                "hints": [
                  { "name": "spring.main.banner-mode",
                    "values": [ { "value": "off" }, { "value": "console" } ]
                  }
                ]
              }"#,
            )
            .unwrap();
        index
    }

    #[test]
    fn completes_spring_properties_in_value_annotation() {
        let mut workspace = SpringWorkspaceIndex::new(Arc::new(test_metadata()));
        workspace.add_config_file("application.properties", "server.port=8080\n");

        let java = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${ser}")
  String port;
}
"#;

        let offset = java.find("${ser}").unwrap() + "${ser".len();
        let items = completions_for_value_placeholder(java, offset, &workspace);
        assert!(items.iter().any(|i| i.label == "server.port"));
    }

    #[test]
    fn reports_unknown_keys_in_properties_file() {
        let metadata = test_metadata();
        let text = "server.port=8080\nunknown.key=foo\n";
        let diags =
            diagnostics_for_config_file(Path::new("application.properties"), text, &metadata);

        assert!(diags.iter().any(|d| d.message.contains("unknown.key")));
        assert!(!diags
            .iter()
            .any(|d| d.message.contains("server.port") && d.message.contains("Unknown")));
    }

    #[test]
    fn navigates_from_value_to_config_definition() {
        let mut workspace = SpringWorkspaceIndex::new(Arc::new(test_metadata()));
        let config = "server.port=8080\n";
        workspace.add_config_file("application.properties", config);

        let java = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${server.port}")
  String port;
}
"#;

        let offset = java.find("server.port").unwrap() + "server.".len();
        let targets = goto_definition_for_value_placeholder(java, offset, &workspace);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path, PathBuf::from("application.properties"));
        assert_eq!(
            &config[targets[0].span.start..targets[0].span.end],
            "server.port"
        );
    }

    #[test]
    fn completes_properties_keys_and_values() {
        let mut workspace = SpringWorkspaceIndex::new(Arc::new(test_metadata()));
        workspace.add_config_file("application.properties", "server.port=8080\n");

        let text = "spr";
        let offset = text.len();
        let items = completions_for_properties_file(
            Path::new("application.properties"),
            text,
            offset,
            &workspace,
        );
        assert!(items.iter().any(|i| i.label == "spring.main.banner-mode"));

        let text = "spring.main.banner-mode=o";
        let offset = text.len();
        let items = completions_for_properties_file(
            Path::new("application.properties"),
            text,
            offset,
            &workspace,
        );
        assert!(items.iter().any(|i| i.label == "off"));
    }

    #[test]
    fn properties_key_completion_does_not_suggest_current_incomplete_key() {
        let mut workspace = SpringWorkspaceIndex::new(Arc::new(test_metadata()));
        let text = "server.port=8080\nserver.p";
        workspace.add_config_file("application.properties", text);

        let offset = text.len();
        let items = completions_for_properties_file(
            Path::new("application.properties"),
            text,
            offset,
            &workspace,
        );
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"server.port"));
        assert!(!labels.contains(&"server.p"));
    }

    #[test]
    fn completes_yaml_keys_by_segment() {
        let workspace = SpringWorkspaceIndex::new(Arc::new(test_metadata()));
        let text = "server:\n  p";
        let offset = text.len();
        let items =
            completions_for_yaml_file(Path::new("application.yml"), text, offset, &workspace);
        let port = items.iter().find(|i| i.label == "port").expect("port item");
        assert_eq!(port.detail.as_deref(), Some("java.lang.Integer"));
    }

    #[test]
    fn completes_yaml_values_from_metadata_hints() {
        let workspace = SpringWorkspaceIndex::new(Arc::new(test_metadata()));
        let text = "spring:\n  main:\n    banner-mode: c";
        let offset = text.len();
        let items =
            completions_for_yaml_file(Path::new("application.yml"), text, offset, &workspace);
        assert!(items.iter().any(|i| i.label == "console"));
    }

    #[test]
    fn navigates_from_config_key_to_java_usage() {
        let mut workspace = SpringWorkspaceIndex::new(Arc::new(test_metadata()));
        let config = "server.port=8080\n";
        workspace.add_config_file("application.properties", config);
        let java = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${server.port}")
  String port;
}
"#;
        workspace.add_java_file("src/main/java/C.java", java);

        let offset = config.find("server.port").unwrap() + "server.".len();
        let usages = goto_usages_for_config_key(
            Path::new("application.properties"),
            config,
            offset,
            &workspace,
        );
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].path, PathBuf::from("src/main/java/C.java"));
    }
}
