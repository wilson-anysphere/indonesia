use std::collections::HashMap;

/// Options controlling how code is anonymized.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CodeAnonymizerOptions {
    /// Replace identifier tokens with deterministic anonymous names.
    pub anonymize_identifiers: bool,

    /// Redact string literals that look sensitive (tokens/keys/passwords).
    pub redact_sensitive_strings: bool,
}

impl Default for CodeAnonymizerOptions {
    fn default() -> Self {
        Self {
            anonymize_identifiers: true,
            redact_sensitive_strings: true,
        }
    }
}

/// Deterministic (per-session) anonymizer for Java-like source code.
///
/// The implementation intentionally avoids depending on a full Java parser.
/// Instead, it performs a light lexical pass that recognizes:
/// - identifiers
/// - string literals
/// - comments
///
/// This is sufficient for redacting/anonymizing snippets sent to external AI
/// services while keeping the code readable for debugging.
#[derive(Debug, Default)]
pub struct CodeAnonymizer {
    options: CodeAnonymizerOptions,
    name_map: HashMap<String, String>,
    next_id: usize,
}

impl CodeAnonymizer {
    pub fn new(options: CodeAnonymizerOptions) -> Self {
        Self {
            options,
            name_map: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn anonymize(&mut self, code: &str) -> String {
        let mut out = String::with_capacity(code.len());

        let mut chars = code.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                // Line comment
                '/' if chars.peek() == Some(&'/') => {
                    out.push('/');
                    out.push('/');
                    chars.next();
                    while let Some(c) = chars.next() {
                        out.push(c);
                        if c == '\n' {
                            break;
                        }
                    }
                }
                // Block comment
                '/' if chars.peek() == Some(&'*') => {
                    out.push('/');
                    out.push('*');
                    chars.next();
                    while let Some(c) = chars.next() {
                        out.push(c);
                        if c == '*' && chars.peek() == Some(&'/') {
                            out.push('/');
                            chars.next();
                            break;
                        }
                    }
                }
                // String literal
                '"' => {
                    let mut raw = String::new();
                    raw.push('"');

                    let mut literal = String::new();
                    let mut escaped = false;

                    while let Some(c) = chars.next() {
                        raw.push(c);

                        if escaped {
                            escaped = false;
                            literal.push(c);
                            continue;
                        }

                        match c {
                            '\\' => {
                                escaped = true;
                                // Don't include the escape itself in the logical literal content.
                            }
                            '"' => break,
                            _ => literal.push(c),
                        }
                    }

                    if self.options.redact_sensitive_strings && looks_sensitive(&literal) {
                        out.push_str("\"[REDACTED]\"");
                    } else {
                        out.push_str(&raw);
                    }
                }
                // Char literal
                '\'' => {
                    let mut raw = String::new();
                    raw.push('\'');
                    let mut escaped = false;
                    while let Some(c) = chars.next() {
                        raw.push(c);
                        if escaped {
                            escaped = false;
                            continue;
                        }
                        match c {
                            '\\' => escaped = true,
                            '\'' => break,
                            _ => {}
                        }
                    }

                    out.push_str(&raw);
                }
                // Identifier start
                c if is_ident_start(c) => {
                    let mut ident = String::new();
                    ident.push(c);
                    while let Some(&next) = chars.peek() {
                        if is_ident_continue(next) {
                            ident.push(next);
                            chars.next();
                        } else {
                            break;
                        }
                    }

                    if self.options.anonymize_identifiers && should_anonymize_identifier(&ident) {
                        let anon = self.get_or_create_anon_name(&ident);
                        out.push_str(&anon);
                    } else {
                        out.push_str(&ident);
                    }
                }
                other => out.push(other),
            }
        }

        out
    }

    fn get_or_create_anon_name(&mut self, original: &str) -> String {
        if let Some(existing) = self.name_map.get(original) {
            return existing.clone();
        }

        let anon = format!("id_{}", self.next_id);
        self.next_id += 1;
        self.name_map.insert(original.to_owned(), anon.clone());
        anon
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

fn should_anonymize_identifier(ident: &str) -> bool {
    if is_java_keyword(ident) {
        return false;
    }

    // Preserve common Java standard library types and ubiquitous names.
    if is_standard_library_identifier(ident) {
        return false;
    }

    true
}

fn is_java_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "abstract"
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
            // Literal values
            | "true"
            | "false"
            | "null"
    )
}

fn is_standard_library_identifier(ident: &str) -> bool {
    matches!(
        ident,
        // Common stdlib package segments (for fully-qualified names).
        "java"
            | "javax"
            | "jakarta"
            | "lang"
            | "util"
            | "io"
            | "net"
            | "nio"
            | "time"
            | "concurrent"
            | "function"
            | "stream"
        // java.lang
            | "String"
            | "Object"
            | "System"
            | "Math"
            | "Integer"
            | "Long"
            | "Double"
            | "Float"
            | "Boolean"
            | "Character"
            | "Short"
            | "Byte"
            | "Void"
            | "Exception"
            | "RuntimeException"
            | "Throwable"
            | "Override"
            | "SuppressWarnings"
            | "Deprecated"
            // Collections
            | "List"
            | "Map"
            | "Set"
            | "HashMap"
            | "HashSet"
            | "ArrayList"
            | "LinkedList"
            | "Optional"
            | "Stream"
            | "Collectors"
            | "Collections"
            | "Arrays"
            | "Objects"
            // Common fields/methods used in snippets.
            | "out"
            | "err"
            | "print"
            | "println"
            | "printf"
            | "format"
    )
}

fn looks_sensitive(literal: &str) -> bool {
    let trimmed = literal.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Common key/token prefixes.
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("auth")
    {
        return true;
    }

    if trimmed.starts_with("sk-") && trimmed.len() >= 20 {
        return true;
    }

    if trimmed.starts_with("AKIA") && trimmed.len() >= 16 {
        return true;
    }

    if trimmed.contains("-----BEGIN") {
        return true;
    }

    // Heuristic: long-ish base64/hex strings.
    if trimmed.len() >= 32 && is_mostly_alnum_or_symbols(trimmed) {
        return true;
    }

    false
}

fn is_mostly_alnum_or_symbols(s: &str) -> bool {
    let mut good = 0usize;
    let mut total = 0usize;

    for c in s.chars() {
        total += 1;
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=' | '+' | '/' | '.') {
            good += 1;
        }
    }

    // Avoid redacting natural language strings; require the vast majority to be "token-like".
    total > 0 && good * 100 / total >= 95
}
