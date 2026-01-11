use std::collections::HashMap;

use nova_types::Span;
use tree_sitter::{Node, Parser, Tree};

/// Parse Java source text with `tree-sitter-java`.
pub fn parse_java(source: &str) -> Result<Tree, String> {
    let mut parser = Parser::new();
    parser
        .set_language(tree_sitter_java::language())
        .map_err(|_| "tree-sitter-java language load failed".to_string())?;
    parser
        .parse(source, None)
        .ok_or_else(|| "tree-sitter failed to produce a syntax tree".to_string())
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
    pub text: String,
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
    parse_annotation_text(text, span)
}

fn parse_annotation_text(text: String, span: Span) -> Option<ParsedAnnotation> {
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
        text,
    })
}

fn extract_paren_contents<'a>(input: &'a str, open_paren_idx: usize) -> Option<&'a str> {
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
/// This helper keeps array suffixes (`[]`) intact.
pub fn simple_name(raw: &str) -> String {
    let raw = strip_generic_args(raw);
    raw.rsplit('.').next().unwrap_or(&raw).to_string()
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
    fn parses_positional_and_named_args() {
        let ann = parse_annotation_text("@X(\"foo\", name = \"bar\")".to_string(), Span::new(0, 0))
            .expect("annotation");
        assert_eq!(ann.simple_name, "X");
        assert_eq!(ann.args.get("value").map(String::as_str), Some("foo"));
        assert_eq!(ann.args.get("name").map(String::as_str), Some("bar"));
    }

    #[test]
    fn does_not_split_commas_inside_strings() {
        let ann = parse_annotation_text(
            "@X(value = \"a,b\", name=\"c\")".to_string(),
            Span::new(0, 0),
        )
        .expect("annotation");
        assert_eq!(ann.args.get("value").map(String::as_str), Some("a,b"));
        assert_eq!(ann.args.get("name").map(String::as_str), Some("c"));
    }

    #[test]
    fn handles_nested_parens_in_values() {
        let ann = parse_annotation_text(
            "@X(value = foo(\"a,b\", bar(1,2)), name = \"x\")".to_string(),
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
        let ann = parse_annotation_text(
            "@X(value = \"a,\\\\\\\"b\\\\\\\",c\")".to_string(),
            Span::new(0, 0),
        )
        .expect("annotation");
        assert_eq!(
            ann.args.get("value").map(String::as_str),
            Some("a,\\\\\\\"b\\\\\\\",c")
        );
    }

    #[test]
    fn preserves_class_literals() {
        let ann =
            parse_annotation_text("@X(targetEntity = Foo.class)".to_string(), Span::new(0, 0))
                .expect("annotation");
        assert_eq!(
            ann.args.get("targetEntity").map(String::as_str),
            Some("Foo.class")
        );
    }
}
