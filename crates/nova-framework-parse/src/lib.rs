use std::cell::RefCell;
use std::collections::HashMap;

use nova_types::Span;
use tree_sitter::{Node, Parser, Tree};

thread_local! {
    static JAVA_PARSER: RefCell<Result<Parser, String>> = RefCell::new({
        let mut parser = Parser::new();
        match parser.set_language(tree_sitter_java::language()) {
            Ok(()) => Ok(parser),
            Err(_) => Err("tree-sitter-java language load failed".to_string()),
        }
    });
}

/// Parse Java source text with `tree-sitter-java`.
pub fn parse_java(source: &str) -> Result<Tree, String> {
    JAVA_PARSER.with(|parser_cell| {
        let mut parser = parser_cell
            .try_borrow_mut()
            .map_err(|_| "tree-sitter parser is already in use".to_string())?;
        let parser = match parser.as_mut() {
            Ok(parser) => parser,
            Err(err) => return Err(err.clone()),
        };

        parser
            .parse(source, None)
            .ok_or_else(|| "tree-sitter failed to produce a syntax tree".to_string())
    })
}

/// Visit a node and all its descendants in pre-order.
pub fn visit_nodes<'a, F: FnMut(Node<'a>)>(node: Node<'a>, f: &mut F) {
    f(node);
    if node.child_count() == 0 {
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_nodes(child, f);
    }
}

/// Find the first named child with the given kind.
pub fn find_named_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    result
}

/// Best-effort helper to fetch a node's `modifiers` field, falling back to a named child.
pub fn modifier_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("modifiers")
        .or_else(|| find_named_child(node, "modifiers"))
}

/// Return the byte slice for `node` within `source`.
pub fn node_text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

/// A best-effort parsed Java annotation.
///
/// ## Argument parsing semantics
/// - String and char literals have their surrounding quotes stripped, but escape sequences are
///   preserved (no unescaping).
/// - Class literals such as `Foo.class` are returned *with* the `.class` suffix intact. Consumers
///   that need the class name should strip the suffix themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAnnotation {
    pub simple_name: String,
    pub args: HashMap<String, String>,
    pub span: Span,
    /// The exact annotation source text (byte-for-byte) covered by `span`.
    ///
    /// Some framework analyzers don't need this; keeping it optional avoids forcing
    /// all callers to clone annotation text when they only care about `args`.
    pub text: Option<String>,
}

/// Collect all annotation nodes under a modifiers node.
pub fn collect_annotations(modifiers: Node<'_>, source: &str) -> Vec<ParsedAnnotation> {
    let mut anns = Vec::new();
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if child.kind().ends_with("annotation") {
            if let Some(ann) = parse_annotation(child, source) {
                anns.push(ann);
            }
        }
    }
    anns
}

fn parse_annotation(node: Node<'_>, source: &str) -> Option<ParsedAnnotation> {
    let text = node_text(source, node).to_string();
    let span = Span::new(node.start_byte(), node.end_byte());
    parse_annotation_text_owned(text, span)
}

/// Parse an annotation from raw source text and a span.
///
/// This is useful for analyzers built on top of parsers other than tree-sitter
/// (e.g. `nova_syntax`) that still want consistent annotation argument parsing.
pub fn parse_annotation_text(text: &str, span: Span) -> Option<ParsedAnnotation> {
    parse_annotation_text_owned(text.to_string(), span)
}

fn parse_annotation_text_owned(text: String, span: Span) -> Option<ParsedAnnotation> {
    let trimmed = text.trim();
    if !trimmed.starts_with('@') {
        return None;
    }

    let rest = &trimmed[1..];
    let open_paren_idx = rest.find('(');
    let (name_part, args_part) = match open_paren_idx {
        Some(idx) => {
            let name = rest[..idx].trim();
            let args = extract_paren_contents(rest, idx)?;
            (name, Some(args))
        }
        None => (rest.trim(), None),
    };

    let simple_name = name_part
        .rsplit('.')
        .next()
        .unwrap_or(name_part)
        .trim()
        .to_string();

    let mut args = HashMap::new();
    if let Some(args_part) = args_part {
        parse_annotation_args(args_part, &mut args);
    }

    Some(ParsedAnnotation {
        simple_name,
        args,
        span,
        text: Some(text),
    })
}

fn extract_paren_contents(input: &str, open_paren_idx: usize) -> Option<&str> {
    // `input` starts at the annotation name and includes the opening paren.
    let start = open_paren_idx.checked_add(1)?;
    let mut depth: u32 = 1;
    let mut in_string = false;
    let mut in_char = false;
    let mut escape = false;

    for (idx, ch) in input[start..].char_indices() {
        let idx = start + idx;
        if in_string || in_char {
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' {
                escape = true;
                continue;
            }
            if in_string && ch == '"' {
                in_string = false;
            } else if in_char && ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '\'' => in_char = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(input[start..idx].trim());
                }
            }
            _ => {}
        }
    }

    // Unbalanced parens; best-effort: take the rest.
    Some(input[start..].trim())
}

fn parse_annotation_args(args_part: &str, out: &mut HashMap<String, String>) {
    for seg in split_top_level_commas(args_part) {
        if seg.is_empty() {
            continue;
        }

        if let Some((key, value)) = split_named_arg(seg) {
            if let Some(parsed) = parse_literal(value) {
                out.insert(key.to_string(), parsed);
            }
        } else if let Some(value) = parse_literal(seg) {
            // Single positional argument => `value`.
            out.insert("value".to_string(), value);
        }
    }
}

fn split_named_arg(segment: &str) -> Option<(&str, &str)> {
    let mut depth_paren = 0u32;
    let mut depth_brace = 0u32;
    let mut depth_bracket = 0u32;
    let mut in_string = false;
    let mut in_char = false;
    let mut escape = false;

    let bytes = segment.as_bytes();

    for (idx, ch) in segment.char_indices() {
        if in_string || in_char {
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' {
                escape = true;
                continue;
            }
            if in_string && ch == '"' {
                in_string = false;
            } else if in_char && ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '\'' => in_char = true,
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            '=' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                // Avoid treating equality/comparison operators as named arguments.
                let prev = idx.checked_sub(1).and_then(|p| bytes.get(p)).copied();
                let next = bytes.get(idx + 1).copied();
                if prev == Some(b'=') || next == Some(b'=') || prev == Some(b'!') {
                    continue;
                }
                if prev == Some(b'<') || prev == Some(b'>') {
                    continue;
                }

                let key = segment[..idx].trim();
                if !is_ident(key) {
                    continue;
                }
                let value = segment[idx + 1..].trim();
                return Some((key, value));
            }
            _ => {}
        }
    }

    None
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ident_start(first) {
        return false;
    }
    chars.all(is_ident_continue)
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
}

fn is_ident_continue(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth_paren = 0u32;
    let mut depth_brace = 0u32;
    let mut depth_bracket = 0u32;
    let mut in_string = false;
    let mut in_char = false;
    let mut escape = false;
    let mut last = 0usize;

    for (idx, ch) in input.char_indices() {
        if in_string || in_char {
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' {
                escape = true;
                continue;
            }
            if in_string && ch == '"' {
                in_string = false;
            } else if in_char && ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '\'' => in_char = true,
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            ',' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                out.push(input[last..idx].trim());
                last = idx + 1;
            }
            _ => {}
        }
    }

    out.push(input[last..].trim());
    out
}

fn parse_literal(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    if input.len() >= 2 {
        if input.starts_with('"') && input.ends_with('"') {
            return Some(input[1..input.len() - 1].to_string());
        }
        if input.starts_with('\'') && input.ends_with('\'') {
            return Some(input[1..input.len() - 1].to_string());
        }
    }

    Some(input.to_string())
}

/// Remove all whitespace from a type-like string.
pub fn clean_type(raw: &str) -> String {
    raw.split_whitespace().collect::<String>()
}

/// Return the simple (unqualified) name of a type-like string, stripping generic arguments.
///
/// This helper strips trailing array suffixes (`[]`).
pub fn simple_name(raw: &str) -> String {
    simplify_type(raw)
}

/// Simplify a type-like string down to its unqualified base type.
///
/// This strips whitespace, generic arguments, and trailing array suffixes.
pub fn simplify_type(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let compact = clean_type(raw);
    let no_generics = strip_generic_args(&compact);
    let no_array = no_generics.trim_end_matches("[]");
    no_array.rsplit('.').next().unwrap_or(no_array).to_string()
}

/// Parse a class literal like `Foo.class` into a simple class name (`Foo`).
///
/// This is intentionally best-effort and also accepts bare type names (without
/// the `.class` suffix).
pub fn parse_class_literal(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let value = value.strip_suffix(".class").unwrap_or(value).trim();
    if value.is_empty() {
        return None;
    }
    Some(value.rsplit('.').next().unwrap_or(value).to_string())
}

/// Extract a string literal argument's value *and* span (without quotes) within
/// the original source file.
///
/// This is a small convenience for analyzers that want to implement navigation
/// based on annotation string arguments (e.g. MapStruct's `@Mapping(target="...")`).
pub fn annotation_string_value_span(
    annotation: &ParsedAnnotation,
    key: &str,
) -> Option<(String, Span)> {
    let haystack = annotation.text.as_deref()?;
    let idx = find_assignment_key(haystack, key)?;
    let mut i = idx + key.len();

    // Skip whitespace between key and '='.
    i += haystack[i..]
        .bytes()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    if i >= haystack.len() || !haystack[i..].starts_with('=') {
        return None;
    }
    i += 1;

    // Skip whitespace between '=' and opening quote.
    i += haystack[i..]
        .bytes()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    if i >= haystack.len() || !haystack[i..].starts_with('"') {
        return None;
    }
    i += 1;

    let start_in_ann = i;
    let end_quote_rel = find_unescaped_quote(&haystack[start_in_ann..])?;
    let end_in_ann = start_in_ann + end_quote_rel;
    let value = haystack[start_in_ann..end_in_ann].to_string();

    Some((
        value,
        Span::new(
            annotation.span.start + start_in_ann,
            annotation.span.start + end_in_ann,
        ),
    ))
}

fn find_unescaped_quote(input: &str) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut escaped = false;
    for (idx, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' {
            escaped = true;
            continue;
        }
        if b == b'"' {
            return Some(idx);
        }
    }
    None
}

fn find_assignment_key(haystack: &str, key: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut search_start = 0usize;
    while let Some(rel) = haystack[search_start..].find(key) {
        let idx = search_start + rel;
        let before = bytes.get(..idx).and_then(|s| s.last()).copied();
        let after = bytes
            .get(idx + key.len()..)
            .and_then(|s| s.first())
            .copied();

        let before_ok = before
            .map(|b| !is_ident_continue(b as char))
            .unwrap_or(true);
        let after_ok = after.map(|b| !is_ident_continue(b as char)).unwrap_or(true);

        if before_ok && after_ok {
            return Some(idx);
        }

        search_start = idx + key.len();
    }
    None
}

fn strip_generic_args(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0u32;
    for ch in raw.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_java_sources() {
        let src1 = "class A {}";
        let src2 = "class A {} class B {}";

        let tree1 = parse_java(src1).expect("parse src1");
        let tree2 = parse_java(src2).expect("parse src2");

        assert!(!tree1.root_node().has_error());
        assert!(!tree2.root_node().has_error());
        assert_ne!(
            tree1.root_node().named_child_count(),
            tree2.root_node().named_child_count()
        );
    }

    #[test]
    fn parse_java_is_safe_across_threads() {
        let t1 = std::thread::spawn(|| {
            let tree = parse_java("class A {}").expect("parse thread 1");
            (
                tree.root_node().has_error(),
                tree.root_node().named_child_count(),
            )
        });
        let t2 = std::thread::spawn(|| {
            let tree = parse_java("class B {}").expect("parse thread 2");
            (
                tree.root_node().has_error(),
                tree.root_node().named_child_count(),
            )
        });

        let (err1, count1) = t1.join().expect("thread 1 join");
        let (err2, count2) = t2.join().expect("thread 2 join");

        assert!(!err1);
        assert!(!err2);
        assert!(count1 > 0);
        assert!(count2 > 0);
    }

    #[test]
    fn parse_java_returns_error_if_parser_is_reentered_on_same_thread() {
        JAVA_PARSER.with(|cell| {
            // Hold a mutable borrow of the thread-local parser to simulate a re-entrant call.
            let _borrow = cell.borrow_mut();
            let err = parse_java("class A {}").expect_err("expected re-entrancy error");
            assert_eq!(err, "tree-sitter parser is already in use");
        });
    }

    #[test]
    fn parse_java_reuses_parser_instance_within_thread() {
        fn parser_ptr() -> usize {
            JAVA_PARSER.with(|cell| {
                let borrowed = cell.borrow();
                let parser = borrowed
                    .as_ref()
                    .expect("thread-local parser should initialize");
                parser as *const Parser as usize
            })
        }

        let ptr_before = parser_ptr();
        parse_java("class A {}").expect("parse");
        let ptr_after = parser_ptr();
        assert_eq!(ptr_before, ptr_after);
    }

    #[test]
    fn parse_java_does_not_carry_error_state_between_parses() {
        let bad = parse_java("class A {").expect("parse bad source");
        assert!(bad.root_node().has_error());

        let good = parse_java("class B {}").expect("parse good source");
        assert!(!good.root_node().has_error());
    }

    #[test]
    fn parses_positional_and_named_args() {
        let ann = parse_annotation_text("@X(\"foo\", name = \"bar\")", Span::new(0, 0))
            .expect("annotation");
        assert_eq!(ann.simple_name, "X");
        assert_eq!(ann.args.get("value").map(String::as_str), Some("foo"));
        assert_eq!(ann.args.get("name").map(String::as_str), Some("bar"));
    }

    #[test]
    fn does_not_split_commas_inside_strings() {
        let ann = parse_annotation_text("@X(value = \"a,b\", name=\"c\")", Span::new(0, 0))
            .expect("annotation");
        assert_eq!(ann.args.get("value").map(String::as_str), Some("a,b"));
        assert_eq!(ann.args.get("name").map(String::as_str), Some("c"));
    }

    #[test]
    fn handles_nested_parens_in_values() {
        let ann = parse_annotation_text(
            "@X(value = foo(\"a,b\", bar(1,2)), name = \"x\")",
            Span::new(0, 0),
        )
        .expect("annotation");
        assert_eq!(
            ann.args.get("value").map(String::as_str),
            Some("foo(\"a,b\", bar(1,2))")
        );
        assert_eq!(ann.args.get("name").map(String::as_str), Some("x"));
    }

    #[test]
    fn handles_escaped_quotes_in_strings() {
        let ann = parse_annotation_text("@X(value = \"a,\\\\\\\"b\\\\\\\",c\")", Span::new(0, 0))
            .expect("annotation");
        assert_eq!(
            ann.args.get("value").map(String::as_str),
            Some("a,\\\\\\\"b\\\\\\\",c")
        );
    }

    #[test]
    fn preserves_class_literals() {
        let ann = parse_annotation_text("@X(targetEntity = Foo.class)", Span::new(0, 0))
            .expect("annotation");
        assert_eq!(
            ann.args.get("targetEntity").map(String::as_str),
            Some("Foo.class")
        );
    }

    #[test]
    fn parses_class_literal_simple_name() {
        assert_eq!(parse_class_literal("Foo.class").as_deref(), Some("Foo"));
        assert_eq!(
            parse_class_literal("com.example.Foo.class").as_deref(),
            Some("Foo")
        );
        assert_eq!(parse_class_literal("Foo").as_deref(), Some("Foo"));
    }

    #[test]
    fn finds_string_value_span() {
        let text = r#"@Mapping(target = "seatCount", source = "numberOfSeats")"#;
        let span = Span::new(100, 100 + text.len());
        let ann = parse_annotation_text(text, span).expect("annotation");
        let (value, value_span) =
            annotation_string_value_span(&ann, "target").expect("target span");
        assert_eq!(value, "seatCount");
        assert_eq!(
            &text[(value_span.start - span.start)..(value_span.end - span.start)],
            "seatCount"
        );
    }
}
