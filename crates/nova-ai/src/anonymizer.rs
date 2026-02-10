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
///
/// ## Identifier mapping (request-scoped)
///
/// When `options.anonymize_identifiers=true`, this anonymizer assigns stable placeholder names
/// (`id_0`, `id_1`, …) to each identifier it sees. The mapping is **request-scoped**: create a new
/// [`CodeAnonymizer`] (and therefore a new mapping) per request. Do **not** reuse the mapping (or
/// the anonymizer instance) across requests, otherwise callers may inadvertently correlate
/// identifiers across unrelated prompts.
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
    ///
    /// The mapping is **request-scoped**: create a new anonymizer per request and do not reuse the
    /// mapping across requests. Reusing mappings across requests can leak cross-request identity
    /// correlation (e.g., the same placeholder name referring to the same original identifier).
    pub(crate) fn identifier_map(&self) -> &HashMap<String, String> {
        &self.name_map
    }

    /// Returns a reverse identifier mapping (`anonymized -> original`).
    ///
    /// This is useful for prompt pipelines that need to de-anonymize LLM output before returning
    /// it to the user.
    ///
    /// Deterministic behavior: if multiple originals map to the same anonymized identifier (this
    /// should not happen for maps produced by [`CodeAnonymizer`]), the reverse map keeps the
    /// lexicographically smallest original.
    pub fn reverse_identifier_map(&self) -> HashMap<String, String> {
        build_reverse_identifier_map(self.identifier_map())
    }

    /// Consume the anonymizer and return its identifier mapping (original identifier → anonymized
    /// identifier).
    ///
    /// See [`Self::identifier_map`] for important privacy notes: the mapping is **request-scoped**
    /// and must not be reused across requests.
    #[cfg(test)]
    pub(crate) fn into_identifier_map(self) -> HashMap<String, String> {
        self.name_map
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

/// Build the reverse identifier mapping (anonymized identifier → original identifier).
///
/// This is primarily used to de-anonymize model outputs within the same request.
///
/// The mapping is **request-scoped** and must not be reused across requests (see
/// [`CodeAnonymizer`]).
///
/// ## Determinism
///
/// [`HashMap`] iteration order is intentionally not relied upon for conflict resolution. If
/// multiple original identifiers map to the same anonymized identifier (this should not happen
/// for maps produced by [`CodeAnonymizer`]), this helper deterministically keeps the
/// lexicographically smallest original identifier.
pub(crate) fn build_reverse_identifier_map(
    identifier_map: &HashMap<String, String>,
) -> HashMap<String, String> {
    use std::collections::hash_map::Entry;

    let mut out = HashMap::with_capacity(identifier_map.len());
    for (original, anonymized) in identifier_map {
        match out.entry(anonymized.to_owned()) {
            Entry::Vacant(v) => {
                v.insert(original.to_owned());
            }
            Entry::Occupied(mut o) => {
                // Deterministic conflict resolution (independent of HashMap iteration order).
                if original < o.get() {
                    o.insert(original.to_owned());
                }
            }
        }
    }
    out
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
pub fn deanonymize_java_like_code(
    code: &str,
    reverse_identifiers: &HashMap<String, String>,
) -> String {
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
    completion.insert_text = deanonymize_java_like_code(&completion.insert_text, reverse_identifiers);
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
            // Functional interfaces / helpers frequently referenced in method signatures.
            | "Function"
            | "Predicate"
            | "Consumer"
            | "Supplier"
            | "Comparator"
            | "Collector"
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
            | "distinct"
            | "sorted"
            | "limit"
            | "skip"
            | "forEach"
            | "forEachOrdered"
            | "findFirst"
            | "findAny"
            | "anyMatch"
            | "allMatch"
            | "noneMatch"
            | "count"
            | "peek"
            | "reduce"
            | "max"
            | "min"
            | "toArray"
            // java.util.stream.Collectors
            | "toList"
            | "toSet"
            | "toMap"
            | "joining"
            | "groupingBy"
            | "mapping"
            | "collectingAndThen"
            | "partitioningBy"
            // java.lang.String
            | "length"
            | "substring"
            | "charAt"
            | "isEmpty"
            | "trim"
            | "startsWith"
            | "endsWith"
            | "replace"
            | "split"
            | "isBlank"
            | "strip"
            | "toLowerCase"
            | "toUpperCase"
            // java.lang.Object
            | "toString"
            | "equals"
            | "hashCode"
            // java.util.Optional
            | "ofNullable"
            | "orElse"
            | "orElseGet"
            | "orElseThrow"
            | "isPresent"
            | "ifPresent"
            | "ifPresentOrElse"
            // Common collection helpers
            | "get"
            | "put"
            | "add"
            | "remove"
            | "contains"
            | "containsKey"
            | "getOrDefault"
            | "computeIfAbsent"
            | "size"
            | "asList"
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
        items.stream()
            .filter(s -> !s.isEmpty())
            .map(String::trim)
            .flatMap(s -> s.stream())
            .distinct()
            .sorted()
            .limit(10)
            .skip(1)
            .peek(System.out::println)
            .collect(Collectors.toList());
        items.stream().forEach(System.out::println);
        items.stream().forEachOrdered(System.out::println);
        items.stream().findFirst();
        items.stream().findAny();
        items.stream().anyMatch(s -> s.isEmpty());
        items.stream().allMatch(s -> !s.isEmpty());
        items.stream().noneMatch(s -> s.isEmpty());
        items.stream().count();
        items.stream().reduce((a, b) -> a);
        items.stream().max(String::compareTo);
        items.stream().min(String::compareTo);
        items.stream().toArray();
        items.stream().collect(Collectors.toSet());
        items.stream().collect(Collectors.toMap(x -> x, x -> x));
        items.stream().collect(Collectors.joining(","));
        items.stream().collect(Collectors.groupingBy(x -> x));
        items.stream().collect(Collectors.partitioningBy(x -> true));
        items.stream().collect(Collectors.mapping(x -> x, Collectors.toList()));
        items.stream().collect(Collectors.collectingAndThen(Collectors.toList(), x -> x));

        int len = "abc".length();
        String sub = "abc".substring(1);
        char ch = "abc".charAt(0);
        boolean hasPrefix = "abc".startsWith("a");
        boolean hasSuffix = "abc".endsWith("c");
        String norm = " Abc ".trim().toLowerCase().toUpperCase();
        boolean hasMid = "abc".contains("b");
        "abc".replace("a", "b");
        "a,b".split(",");
        "   ".isBlank();
        " abc ".strip();
        String s = items.toString();
        boolean eq = "abc".equals("def");
        int h = items.hashCode();

        int n = items.size();
        boolean has = items.contains("x");
        items.add("x");
        items.remove("x");
        String first = items.get(0);

        Map<String, String> m = new HashMap<>();
        m.put("a", "b");
        m.get("a");
        m.getOrDefault("a", "b");
        m.computeIfAbsent("a", k -> "b");
        m.containsKey("a");
        m.remove("a");

        Arrays.asList("a", "b");

        Optional<String> opt = Optional.ofNullable(first);
        opt.isPresent();
        opt.orElse("fallback");
        opt.orElseGet(() -> "fallback");
        opt.orElseThrow();
        opt.ifPresent(System.out::println);
        opt.ifPresentOrElse(System.out::println, () -> {});

        Function<String, String> fn1 = String::trim;
        Predicate<String> pred = s -> s.isEmpty();
        Consumer<String> cons = System.out::println;
        Supplier<String> sup = () -> "x";
        Comparator<String> cmp = String::compareTo;
        Collector<String, ?, List<String>> col = Collectors.toList();
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
        assert!(out.contains(".distinct("), "{out}");
        assert!(out.contains(".sorted("), "{out}");
        assert!(out.contains(".limit("), "{out}");
        assert!(out.contains(".skip("), "{out}");
        assert!(out.contains(".peek("), "{out}");
        assert!(out.contains(".forEach("), "{out}");
        assert!(out.contains(".forEachOrdered("), "{out}");
        assert!(out.contains(".findFirst("), "{out}");
        assert!(out.contains(".findAny("), "{out}");
        assert!(out.contains(".anyMatch("), "{out}");
        assert!(out.contains(".allMatch("), "{out}");
        assert!(out.contains(".noneMatch("), "{out}");
        assert!(out.contains(".count("), "{out}");
        assert!(out.contains(".reduce("), "{out}");
        assert!(out.contains(".max("), "{out}");
        assert!(out.contains(".min("), "{out}");
        assert!(out.contains(".toArray("), "{out}");

        // Collectors.
        assert!(out.contains("Collectors.toList"), "{out}");
        assert!(out.contains("Collectors.toSet"), "{out}");
        assert!(out.contains("Collectors.toMap"), "{out}");
        assert!(out.contains("Collectors.joining"), "{out}");
        assert!(out.contains("Collectors.groupingBy"), "{out}");
        assert!(out.contains("Collectors.partitioningBy"), "{out}");
        assert!(out.contains("Collectors.mapping"), "{out}");
        assert!(out.contains("Collectors.collectingAndThen"), "{out}");

        // String methods.
        assert!(out.contains(".length("), "{out}");
        assert!(out.contains(".substring("), "{out}");
        assert!(out.contains(".charAt("), "{out}");
        assert!(out.contains(".isEmpty("), "{out}");
        assert!(out.contains(".trim("), "{out}");
        assert!(out.contains(".startsWith("), "{out}");
        assert!(out.contains(".endsWith("), "{out}");
        assert!(out.contains(".contains("), "{out}");
        assert!(out.contains(".replace("), "{out}");
        assert!(out.contains(".split("), "{out}");
        assert!(out.contains(".isBlank("), "{out}");
        assert!(out.contains(".strip("), "{out}");
        assert!(out.contains(".toLowerCase("), "{out}");
        assert!(out.contains(".toUpperCase("), "{out}");

        // Object methods.
        assert!(out.contains(".toString("), "{out}");
        assert!(out.contains(".equals("), "{out}");
        assert!(out.contains(".hashCode("), "{out}");

        // Common collection methods/utilities.
        assert!(out.contains(".size("), "{out}");
        assert!(out.contains(".contains("), "{out}");
        assert!(out.contains(".add("), "{out}");
        assert!(out.contains(".remove("), "{out}");
        assert!(out.contains(".get("), "{out}");
        assert!(out.contains(".put("), "{out}");
        assert!(out.contains(".containsKey("), "{out}");
        assert!(out.contains(".getOrDefault("), "{out}");
        assert!(out.contains(".computeIfAbsent("), "{out}");
        assert!(out.contains("Arrays.asList"), "{out}");

        // Optional methods.
        assert!(out.contains(".ofNullable("), "{out}");
        assert!(out.contains(".isPresent("), "{out}");
        assert!(out.contains(".orElse("), "{out}");
        assert!(out.contains(".orElseGet("), "{out}");
        assert!(out.contains(".orElseThrow("), "{out}");
        assert!(out.contains(".ifPresent("), "{out}");
        assert!(out.contains(".ifPresentOrElse("), "{out}");

        // Common functional interface type names.
        assert!(out.contains("Function<"), "{out}");
        assert!(out.contains("Predicate<"), "{out}");
        assert!(out.contains("Consumer<"), "{out}");
        assert!(out.contains("Supplier<"), "{out}");
        assert!(out.contains("Comparator<"), "{out}");
        assert!(out.contains("Collector<"), "{out}");
    }

    #[test]
    fn identifier_mapping_contains_anonymized_identifiers_and_reverse_is_bijection() {
        let code = r#"
            class Foo {
                Foo foo;
                void bar(Foo baz) {
                    Foo qux = baz;
                    foo = qux;
                }
            }
        "#;

        let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
            anonymize_identifiers: true,
            redact_sensitive_strings: false,
            redact_numeric_literals: false,
            strip_or_redact_comments: false,
        });

        let _out = anonymizer.anonymize(code);

        let forward = anonymizer.identifier_map().clone();
        assert!(forward.contains_key("Foo"), "{forward:?}");
        assert!(forward.contains_key("foo"), "{forward:?}");
        assert!(forward.contains_key("bar"), "{forward:?}");
        assert!(forward.contains_key("baz"), "{forward:?}");
        assert!(forward.contains_key("qux"), "{forward:?}");

        let reverse = build_reverse_identifier_map(&forward);
        assert_eq!(
            reverse.len(),
            forward.len(),
            "reverse map should be a bijection for anonymized identifiers"
        );

        for (original, anonymized) in &forward {
            assert_eq!(
                reverse.get(anonymized),
                Some(original),
                "expected reverse[{anonymized}] = {original}"
            );
        }

        let moved = anonymizer.into_identifier_map();
        assert_eq!(moved, forward);
    }

    #[test]
    fn identifier_mapping_is_deterministic_for_repeated_anonymize_calls() {
        let code = r#"class Foo { int count; void inc() { count++; } }"#;

        let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
            anonymize_identifiers: true,
            redact_sensitive_strings: false,
            redact_numeric_literals: false,
            strip_or_redact_comments: false,
        });

        let out1 = anonymizer.anonymize(code);
        let map1 = anonymizer.identifier_map().clone();

        let out2 = anonymizer.anonymize(code);
        let map2 = anonymizer.identifier_map().clone();

        assert_eq!(out1, out2);
        assert_eq!(map1, map2);
    }

    #[test]
    fn reverse_identifier_map_conflicts_are_resolved_deterministically() {
        let mut map1 = HashMap::new();
        map1.insert("b".to_string(), "id_0".to_string());
        map1.insert("a".to_string(), "id_0".to_string());

        let mut map2 = HashMap::new();
        map2.insert("a".to_string(), "id_0".to_string());
        map2.insert("b".to_string(), "id_0".to_string());

        let reverse1 = build_reverse_identifier_map(&map1);
        let reverse2 = build_reverse_identifier_map(&map2);

        assert_eq!(reverse1, reverse2);
        assert_eq!(reverse1.get("id_0").map(String::as_str), Some("a"));
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
