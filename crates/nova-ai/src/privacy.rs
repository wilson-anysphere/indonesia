use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};

/// Privacy controls that apply when building prompts for cloud models.
#[derive(Debug, Clone)]
pub struct PrivacyMode {
    /// Replace identifiers (class/method/variable names) with stable placeholders.
    pub anonymize_identifiers: bool,

    /// Redact suspicious literals (API keys, tokens, long IDs, etc).
    pub redaction: RedactionConfig,

    /// Whether the builder is allowed to include file system paths.
    pub include_file_paths: bool,
}

impl Default for PrivacyMode {
    fn default() -> Self {
        Self {
            anonymize_identifiers: false,
            redaction: RedactionConfig::default(),
            include_file_paths: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RedactionConfig {
    pub redact_string_literals: bool,
    pub redact_numeric_literals: bool,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            redact_string_literals: true,
            redact_numeric_literals: true,
        }
    }
}

static IDENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]*\b").expect("valid regex"));

static LONG_NUMBER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b\d{16,}\b").expect("valid regex"));

static LONG_HEX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b0x[0-9a-fA-F]{16,}\b").expect("valid regex"));

static BASE64ISH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[A-Za-z0-9+/=_-]{20,}$").expect("valid regex")
});

static HEXISH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9a-fA-F]{32,}$").expect("valid regex"));

static JAVA_KEYWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class",
        "const", "continue", "default", "do", "double", "else", "enum", "extends", "final",
        "finally", "float", "for", "goto", "if", "implements", "import", "instanceof", "int",
        "interface", "long", "native", "new", "package", "private", "protected", "public",
        "return", "short", "static", "strictfp", "super", "switch", "synchronized", "this",
        "throw", "throws", "transient", "try", "void", "volatile", "while", "null", "true",
        "false",
    ]
    .into_iter()
    .collect()
});

static COMMON_NON_PROJECT_IDENTIFIERS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    // A small allowlist to keep prompts readable while still anonymizing project-specific code.
    [
        "String",
        "Object",
        "System",
        "Math",
        "List",
        "Map",
        "Set",
        "Optional",
        "Arrays",
        "Collections",
    ]
    .into_iter()
    .collect()
});

/// Stateful anonymizer that produces stable placeholders within a single prompt build.
#[derive(Debug, Default, Clone)]
pub(crate) struct CodeAnonymizer {
    name_map: HashMap<String, String>,
    next_id: usize,
}

impl CodeAnonymizer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn anonymize(&mut self, code: &str) -> String {
        let mut out = String::with_capacity(code.len());

        // Walk the text and only run the identifier regex outside of string/char literals.
        let bytes = code.as_bytes();
        let mut i = 0;
        let mut segment_start = 0;

        while i < bytes.len() {
            let b = bytes[i];
            if b == b'"' || b == b'\'' {
                // Process preceding non-literal segment.
                if segment_start < i {
                    let segment = &code[segment_start..i];
                    out.push_str(&self.anonymize_segment(segment));
                }

                // Copy literal verbatim.
                let quote = b;
                let mut j = i + 1;
                while j < bytes.len() {
                    let bj = bytes[j];
                    if bj == b'\\' {
                        // Skip escaped byte.
                        j = (j + 2).min(bytes.len());
                        continue;
                    }
                    if bj == quote {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                out.push_str(&code[i..j.min(bytes.len())]);
                i = j;
                segment_start = i;
                continue;
            }
            i += 1;
        }

        if segment_start < code.len() {
            out.push_str(&self.anonymize_segment(&code[segment_start..]));
        }

        out
    }

    fn anonymize_segment(&mut self, segment: &str) -> String {
        IDENT_RE
            .replace_all(segment, |caps: &regex::Captures<'_>| {
                let ident = &caps[0];
                if should_anonymize_identifier(ident) {
                    self.get_or_create_placeholder(ident)
                } else {
                    ident.to_string()
                }
            })
            .into_owned()
    }

    fn get_or_create_placeholder(&mut self, original: &str) -> String {
        if let Some(existing) = self.name_map.get(original) {
            return existing.clone();
        }

        let placeholder = format!("ID_{}", self.next_id);
        self.next_id += 1;
        self.name_map
            .insert(original.to_string(), placeholder.clone());
        placeholder
    }
}

fn should_anonymize_identifier(ident: &str) -> bool {
    if JAVA_KEYWORDS.contains(ident) {
        return false;
    }
    if COMMON_NON_PROJECT_IDENTIFIERS.contains(ident) {
        return false;
    }
    true
}

pub fn redact_suspicious_literals(code: &str, cfg: &RedactionConfig) -> String {
    let mut redacted = code.to_string();

    if cfg.redact_numeric_literals {
        redacted = LONG_HEX_RE.replace_all(&redacted, "0xREDACTED").into_owned();
        redacted = LONG_NUMBER_RE.replace_all(&redacted, "0").into_owned();
    }

    if cfg.redact_string_literals {
        redacted = redact_string_literals(&redacted);
    }

    redacted
}

fn redact_string_literals(code: &str) -> String {
    let bytes = code.as_bytes();
    let mut out = String::with_capacity(code.len());

    let mut segment_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }

        // Copy the non-literal segment before the string.
        if segment_start < i {
            out.push_str(&code[segment_start..i]);
        }

        // Parse a Java string literal (including quotes).
        let start = i;
        i += 1;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if bytes[i] == b'"' {
                i += 1;
                break;
            }
            i += 1;
        }

        let literal = &code[start..i.min(bytes.len())];
        let inner = literal
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(literal);

        if looks_sensitive_string(inner) {
            out.push_str("\"[REDACTED]\"");
        } else {
            out.push_str(literal);
        }

        segment_start = i.min(bytes.len());
    }

    if segment_start < code.len() {
        out.push_str(&code[segment_start..]);
    }

    out
}

fn looks_sensitive_string(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if lower.contains("password")
        || lower.contains("secret")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("token")
    {
        return true;
    }

    if s.contains("-----BEGIN") || s.contains("PRIVATE KEY") {
        return true;
    }

    // Long strings that look like tokens (base64-ish or hex).
    if BASE64ISH_RE.is_match(s) || HEXISH_RE.is_match(s) {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymize_identifiers_is_stable() {
        let mut anon = CodeAnonymizer::new();
        let code = r#"
class MyService {
  int add(int left, int right) { return left + right; }
}
"#;

        let out = anon.anonymize(code);

        // Project identifiers are replaced.
        assert!(out.contains("class ID_0"));
        assert!(out.contains("ID_1(")); // method name
        // Java keyword and primitive types are kept.
        assert!(out.contains("int"));
        assert!(out.contains("return"));

        // `left`/`right` placeholders are consistent within the same run.
        assert!(out.matches("ID_2").count() >= 2);
        assert!(out.matches("ID_3").count() >= 2);
    }

    #[test]
    fn redacts_suspicious_string_literals() {
        let code = r#"String apiKey = "sk-verysecretstringthatislong";"#;
        let redacted = redact_suspicious_literals(code, &RedactionConfig::default());
        assert!(redacted.contains("\"[REDACTED]\""));
        assert!(!redacted.contains("sk-verysecret"));
    }
}
