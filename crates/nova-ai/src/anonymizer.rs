use std::collections::HashMap;

use crate::{AdditionalEdit, MultiTokenCompletion};

/// Options controlling how code is anonymized.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CodeAnonymizerOptions {
    /// Replace identifier tokens with deterministic anonymous names.
    pub anonymize_identifiers: bool,

    /// Redact string literals that look sensitive (tokens/keys/passwords).
    pub redact_sensitive_strings: bool,

    /// Redact suspiciously long numeric literals (IDs, hashes).
    pub redact_numeric_literals: bool,

    /// Strip comment bodies (line and block comments).
    ///
    /// When enabled, the anonymizer preserves the comment delimiters (`//`, `/* */`)
    /// but drops the comment content so secrets do not leak through comments.
    pub strip_or_redact_comments: bool,
}

impl Default for CodeAnonymizerOptions {
    fn default() -> Self {
        Self {
            anonymize_identifiers: true,
            redact_sensitive_strings: true,
            redact_numeric_literals: true,
            strip_or_redact_comments: false,
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

    /// Returns the identifier mapping collected so far (`original -> anonymized`).
    pub fn identifier_map(&self) -> &HashMap<String, String> {
        &self.name_map
    }

    /// Returns a reverse identifier mapping (`anonymized -> original`).
    ///
    /// This is useful for prompt pipelines that need to de-anonymize LLM output
    /// before returning it to the user.
    pub fn reverse_identifier_map(&self) -> HashMap<String, String> {
        let mut out = HashMap::with_capacity(self.name_map.len());
        for (original, anon) in &self.name_map {
            out.insert(anon.clone(), original.clone());
        }
        out
    }

    pub fn anonymize(&mut self, code: &str) -> String {
        let mut out = String::with_capacity(code.len());

        let mut chars = code.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                // Line comment
                '/' if chars.peek() == Some(&'/') => {
                    chars.next();
                    if self.options.strip_or_redact_comments {
                        out.push_str("// [REDACTED]");
                        while let Some(c) = chars.next() {
                            if c == '\n' {
                                out.push('\n');
                                break;
                            }
                        }
                    } else {
                        out.push('/');
                        out.push('/');
                        while let Some(c) = chars.next() {
                            out.push(c);
                            if c == '\n' {
                                break;
                            }
                        }
                    }
                }
                // Block comment
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    if self.options.strip_or_redact_comments {
                        // Preserve Nova's synthetic range marker comments used by code-edit prompts.
                        // These markers are not part of user code and are required for patch-based
                        // workflows to be reliable when comment stripping is enabled.
                        //
                        // We keep the detection very small and self-contained: only comments with
                        // bodies matching the exact marker strings are preserved.
                        const RANGE_MARKERS: [&str; 2] =
                            ["__NOVA_AI_RANGE_START__", "__NOVA_AI_RANGE_END__"];
                        const MAX_MARKER_LEN: usize = 64;

                        let mut marker_buf = String::new();
                        let mut marker_possible = true;
                        let mut closed = false;

                        while let Some(c) = chars.next() {
                            if c == '*' && chars.peek() == Some(&'/') {
                                chars.next();
                                closed = true;
                                break;
                            }

                            if marker_possible {
                                if marker_buf.len() < MAX_MARKER_LEN {
                                    marker_buf.push(c);
                                } else {
                                    marker_possible = false;
                                }
                            }
                        }

                        let marker = marker_buf.trim();
                        if closed && marker_possible && RANGE_MARKERS.contains(&marker) {
                            out.push_str("/*");
                            out.push_str(marker);
                            out.push_str("*/");
                        } else {
                            out.push_str("/* [REDACTED] */");
                        }
                    } else {
                        out.push('/');
                        out.push('*');
                        while let Some(c) = chars.next() {
                            out.push(c);
                            if c == '*' && chars.peek() == Some(&'/') {
                                out.push('/');
                                chars.next();
                                break;
                            }
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
                // Numeric literal (decimal)
                c if c.is_ascii_digit() => {
                    let mut raw = String::new();
                    raw.push(c);

                    let mut digits = 1usize;

                    // Hex literal: 0x...
                    if c == '0' && matches!(chars.peek(), Some(&'x') | Some(&'X')) {
                        raw.push(chars.next().expect("peeked above"));
                        let mut hex_digits = 0usize;
                        while let Some(&next) = chars.peek() {
                            if next.is_ascii_hexdigit() {
                                raw.push(next);
                                chars.next();
                                hex_digits += 1;
                            } else if next == '_' {
                                raw.push(next);
                                chars.next();
                            } else {
                                break;
                            }
                        }

                        let suffix = match chars.peek().copied() {
                            Some('l' | 'L' | 'f' | 'F' | 'd' | 'D') => chars.next(),
                            _ => None,
                        };

                        if self.options.redact_numeric_literals && hex_digits >= 16 {
                            out.push_str("0xREDACTED");
                            if let Some(suffix) = suffix {
                                out.push(suffix);
                            }
                        } else {
                            if let Some(suffix) = suffix {
                                raw.push(suffix);
                            }
                            out.push_str(&raw);
                        }
                        continue;
                    }

                    while let Some(&next) = chars.peek() {
                        if next.is_ascii_digit() {
                            raw.push(next);
                            chars.next();
                            digits += 1;
                        } else if next == '_' {
                            raw.push(next);
                            chars.next();
                        } else {
                            break;
                        }
                    }

                    if self.options.redact_numeric_literals && digits >= 16 {
                        out.push('0');
                        if let Some(&suffix) = chars.peek() {
                            if matches!(suffix, 'l' | 'L' | 'f' | 'F' | 'd' | 'D') {
                                out.push(suffix);
                                chars.next();
                            }
                        }
                    } else {
                        if let Some(&suffix) = chars.peek() {
                            if matches!(suffix, 'l' | 'L' | 'f' | 'F' | 'd' | 'D') {
                                raw.push(suffix);
                                chars.next();
                            }
                        }
                        out.push_str(&raw);
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

/// De-anonymize Java-like code emitted by an LLM by rewriting placeholder identifiers back to the
/// original names.
///
/// This is the inverse of identifier anonymization used by [`CodeAnonymizer`]. The function
/// performs a lightweight lexical scan (similar to the anonymizer) so it can safely replace
/// identifier *tokens* without corrupting:
/// - string literals (`"..."`)
/// - char literals (`'a'`)
/// - line comments (`// ...`)
/// - block comments (`/* ... */`)
///
/// The `reverse_identifiers` map must map from the anonymous identifier (e.g. `"id_0"`) to the
/// original identifier (e.g. `"UserService"`).
pub fn deanonymize_java_like_code(code: &str, reverse_identifiers: &HashMap<String, String>) -> String {
    if reverse_identifiers.is_empty() {
        return code.to_string();
    }

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
                out.push('"');
                let mut escaped = false;
                while let Some(c) = chars.next() {
                    out.push(c);
                    if escaped {
                        escaped = false;
                        continue;
                    }

                    match c {
                        '\\' => escaped = true,
                        '"' => break,
                        _ => {}
                    }
                }
            }
            // Char literal
            '\'' => {
                out.push('\'');
                let mut escaped = false;
                while let Some(c) = chars.next() {
                    out.push(c);
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
            }
            // Identifier token
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

                if let Some(original) = reverse_identifiers.get(&ident) {
                    out.push_str(original);
                } else {
                    out.push_str(&ident);
                }
            }
            other => out.push(other),
        }
    }

    out
}

/// Apply de-anonymization to all user-visible fields of a multi-token completion.
pub fn deanonymize_multi_token_completion(
    completion: &mut MultiTokenCompletion,
    reverse_identifiers: &HashMap<String, String>,
) {
    if reverse_identifiers.is_empty() {
        return;
    }

    completion.label = deanonymize_java_like_code(&completion.label, reverse_identifiers);
    completion.insert_text =
        deanonymize_java_like_code(&completion.insert_text, reverse_identifiers);
    for edit in &mut completion.additional_edits {
        deanonymize_additional_edit(edit, reverse_identifiers);
    }
}

/// Apply de-anonymization to a single [`AdditionalEdit`].
pub fn deanonymize_additional_edit(
    edit: &mut AdditionalEdit,
    reverse_identifiers: &HashMap<String, String>,
) {
    if reverse_identifiers.is_empty() {
        return;
    }

    match edit {
        AdditionalEdit::AddImport { path } => {
            *path = deanonymize_java_like_code(path, reverse_identifiers);
        }
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
            // Newer keywords / reserved identifiers.
            | "var"
            | "record"
            | "sealed"
            | "permits"
            | "yield"
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
            // Common JDK method identifiers that are ubiquitous in completion prompts.
            // Keeping these intact improves readability of anonymized cloud prompts.
            //
            // java.util.stream.Stream (method chaining)
            | "filter"
            | "map"
            | "flatMap"
            | "collect"
            | "sorted"
            | "forEach"
            | "findFirst"
            | "findAny"
            // java.util.stream.Collectors
            | "toList"
            | "toSet"
            | "joining"
            // java.lang.String
            | "length"
            | "substring"
            | "charAt"
            | "isEmpty"
            | "trim"
            | "toLowerCase"
            | "toUpperCase"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MultiTokenInsertTextFormat;
    use std::collections::HashMap;

    #[test]
    fn strips_comment_bodies_when_enabled() {
        let code = r#"
            class Foo {
                // apiKey=sk-verysecretstringthatislong
                /* password=hunter2 */
                void m() {}
            }
        "#;

        let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
            anonymize_identifiers: false,
            redact_sensitive_strings: false,
            redact_numeric_literals: false,
            strip_or_redact_comments: true,
        });

        let out = anonymizer.anonymize(code);
        assert!(out.contains("// [REDACTED]"));
        assert!(out.contains("/* [REDACTED] */"));
        assert!(!out.contains("sk-verysecret"), "{out}");
        assert!(!out.contains("hunter2"), "{out}");
    }

    #[test]
    fn preserves_nova_ai_range_markers_when_comment_stripping_enabled() {
        let code = r#"
            /*__NOVA_AI_RANGE_START__*/
            class Foo {
                /* secret */
                void m() {}
            }
            /*__NOVA_AI_RANGE_END__*/
        "#;

        let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
            anonymize_identifiers: false,
            redact_sensitive_strings: false,
            redact_numeric_literals: false,
            strip_or_redact_comments: true,
        });

        let out = anonymizer.anonymize(code);
        assert!(out.contains("/*__NOVA_AI_RANGE_START__*/"), "{out}");
        assert!(out.contains("/*__NOVA_AI_RANGE_END__*/"), "{out}");
        assert!(out.contains("/* [REDACTED] */"), "{out}");
        assert!(!out.contains("secret"), "{out}");
    }

    #[test]
    fn preserves_common_jdk_method_identifiers_when_anonymizing_identifiers() {
        let code = r#"
class Example {
    void run(List<String> items) {
        items.stream().filter(s -> !s.isEmpty()).map(String::trim).flatMap(s -> s.stream()).sorted().collect(Collectors.toList());
        items.stream().forEach(System.out::println);
        items.stream().findFirst();
        items.stream().findAny();
        items.stream().collect(Collectors.toSet());
        items.stream().collect(Collectors.joining(","));

        int len = "abc".length();
        String sub = "abc".substring(1);
        char ch = "abc".charAt(0);
        String norm = " Abc ".trim().toLowerCase().toUpperCase();
    }
}
"#;

        let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
            anonymize_identifiers: true,
            redact_sensitive_strings: false,
            redact_numeric_literals: false,
            strip_or_redact_comments: false,
        });

        let out = anonymizer.anonymize(code);

        // Sanity check: anonymization should still happen for user-defined identifiers.
        assert!(out.contains("class id_0"), "{out}");

        // Stream methods.
        assert!(out.contains(".filter("), "{out}");
        assert!(out.contains(".map("), "{out}");
        assert!(out.contains(".flatMap("), "{out}");
        assert!(out.contains(".collect("), "{out}");
        assert!(out.contains(".sorted("), "{out}");
        assert!(out.contains(".forEach("), "{out}");
        assert!(out.contains(".findFirst("), "{out}");
        assert!(out.contains(".findAny("), "{out}");

        // Collectors.
        assert!(out.contains("Collectors.toList"), "{out}");
        assert!(out.contains("Collectors.toSet"), "{out}");
        assert!(out.contains("Collectors.joining"), "{out}");

        // String methods.
        assert!(out.contains(".length("), "{out}");
        assert!(out.contains(".substring("), "{out}");
        assert!(out.contains(".charAt("), "{out}");
        assert!(out.contains(".isEmpty("), "{out}");
        assert!(out.contains(".trim("), "{out}");
        assert!(out.contains(".toLowerCase("), "{out}");
        assert!(out.contains(".toUpperCase("), "{out}");
    }

    #[test]
    fn deanonymizes_identifier_tokens_in_code() {
        let mut reverse = HashMap::new();
        reverse.insert("id_0".to_string(), "Foo".to_string());
        reverse.insert("id_1".to_string(), "bar".to_string());

        let code = "class id_0 { void id_1() { id_1(); } }";
        let out = deanonymize_java_like_code(code, &reverse);

        assert_eq!(out, "class Foo { void bar() { bar(); } }");
    }

    #[test]
    fn does_not_replace_inside_string_or_char_literals() {
        let mut reverse = HashMap::new();
        reverse.insert("id_0".to_string(), "Foo".to_string());

        let code = "String s = \"id_0\"; char c = 'id_0'; id_0();";
        let out = deanonymize_java_like_code(code, &reverse);

        assert_eq!(out, "String s = \"id_0\"; char c = 'id_0'; Foo();");
    }

    #[test]
    fn does_not_replace_inside_comments() {
        let mut reverse = HashMap::new();
        reverse.insert("id_0".to_string(), "Foo".to_string());

        let code = "id_0(); // id_0\n/* id_0 */ id_0();";
        let out = deanonymize_java_like_code(code, &reverse);

        assert_eq!(out, "Foo(); // id_0\n/* id_0 */ Foo();");
    }

    #[test]
    fn replaces_dotted_import_paths() {
        let mut reverse = HashMap::new();
        reverse.insert("id_0".to_string(), "com".to_string());
        reverse.insert("id_1".to_string(), "example".to_string());
        reverse.insert("id_2".to_string(), "Foo".to_string());

        let code = "import id_0.id_1.id_2;";
        let out = deanonymize_java_like_code(code, &reverse);

        assert_eq!(out, "import com.example.Foo;");
    }

    #[test]
    fn avoids_substring_collisions() {
        let mut reverse = HashMap::new();
        reverse.insert("id_1".to_string(), "Foo".to_string());

        let code = "id_10 id_1";
        let out = deanonymize_java_like_code(code, &reverse);

        assert_eq!(out, "id_10 Foo");
    }

    #[test]
    fn preserves_vscode_snippet_placeholder_syntax() {
        let mut reverse = HashMap::new();
        reverse.insert("id_0".to_string(), "value".to_string());

        let code = "${1:id_0}";
        let out = deanonymize_java_like_code(code, &reverse);

        assert_eq!(out, "${1:value}");
    }

    #[test]
    fn deanonymize_helpers_apply_to_completion_fields() {
        let mut reverse = HashMap::new();
        reverse.insert("id_0".to_string(), "com".to_string());
        reverse.insert("id_1".to_string(), "example".to_string());
        reverse.insert("id_2".to_string(), "Foo".to_string());

        let mut completion = MultiTokenCompletion {
            label: "new id_2".to_string(),
            insert_text: "new ${1:id_2}()".to_string(),
            format: MultiTokenInsertTextFormat::Snippet,
            additional_edits: vec![AdditionalEdit::AddImport {
                path: "id_0.id_1.id_2".to_string(),
            }],
            confidence: 0.5,
        };

        deanonymize_multi_token_completion(&mut completion, &reverse);

        assert_eq!(completion.label, "new Foo");
        assert_eq!(completion.insert_text, "new ${1:Foo}()");
        assert_eq!(
            completion.additional_edits,
            vec![AdditionalEdit::AddImport {
                path: "com.example.Foo".to_string()
            }]
        );
    }
}
