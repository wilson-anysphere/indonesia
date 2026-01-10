use std::collections::HashSet;

use nova_format::format_member_insertion;
use nova_index::TextRange;
use nova_types::TypeRef;
use thiserror::Error;
use tree_sitter::{Node, Parser, Tree};

use crate::TextEdit;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExtractError {
    #[error("failed to parse Java source")]
    ParseError,
    #[error("selection does not resolve to an expression")]
    InvalidSelection,
    #[error("expression has side effects and cannot be extracted safely")]
    SideEffectfulExpression,
    #[error("expression is not contained in a class body")]
    NotInClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractKind {
    Constant,
    Field,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOptions {
    pub name: Option<String>,
    pub replace_all: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            name: None,
            replace_all: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOutcome {
    pub edits: Vec<TextEdit>,
    pub name: String,
}

pub fn extract_constant(
    file: &str,
    source: &str,
    selection: TextRange,
    options: ExtractOptions,
) -> Result<ExtractOutcome, ExtractError> {
    extract_impl(file, source, selection, ExtractKind::Constant, options)
}

/// Extracts a selected expression into an instance field.
///
/// Current implementation policy:
/// - Generates an inline-initialized final field:
///   `private final <Type> <name> = <expr>;`
/// - Rejects expressions with potential side effects (method calls, `new`, etc.)
/// - Performs "replace all" using best-effort structural matching (normalized
///   expression text).
pub fn extract_field(
    file: &str,
    source: &str,
    selection: TextRange,
    options: ExtractOptions,
) -> Result<ExtractOutcome, ExtractError> {
    extract_impl(file, source, selection, ExtractKind::Field, options)
}

fn extract_impl(
    file: &str,
    source: &str,
    selection: TextRange,
    kind: ExtractKind,
    options: ExtractOptions,
) -> Result<ExtractOutcome, ExtractError> {
    let tree = parse_java(source)?;
    let root = tree.root_node();

    let expr = find_expression(root, selection).ok_or(ExtractError::InvalidSelection)?;
    if has_side_effects(expr) {
        return Err(ExtractError::SideEffectfulExpression);
    }

    let class_body = enclosing_class_body(expr).ok_or(ExtractError::NotInClass)?;

    let expr_range = TextRange::new(expr.start_byte(), expr.end_byte());
    let expr_text = source[expr_range.start..expr_range.end].to_string();
    let expr_type = infer_expr_type(source, expr).unwrap_or_else(|| TypeRef::new("Object"));

    let existing_names = collect_field_names(source, class_body);
    let suggested = suggest_name(source, expr, kind, &existing_names);
    let name = options
        .name
        .as_deref()
        .map(|n| sanitize_identifier(n, kind))
        .filter(|n| !n.is_empty())
        .unwrap_or(suggested);
    let name = make_unique(name, &existing_names, kind);

    let occurrences = if options.replace_all {
        find_equivalent_expressions(source, class_body, expr, &expr_text)
    } else {
        vec![expr_range]
    };

    let (type_spelling, import_edit) = compute_type_and_import(file, source, &expr_type);

    let (insert_offset, indent, needs_blank_line_after) = insertion_point(source, class_body, kind);

    let declaration = match kind {
        ExtractKind::Constant => format!("private static final {} {} = {};", type_spelling, name, expr_text),
        ExtractKind::Field => format!("private final {} {} = {};", type_spelling, name, expr_text),
    };

    let insert_text = format_member_insertion(&indent, &declaration, needs_blank_line_after);

    let mut edits = Vec::new();
    if let Some(import_edit) = import_edit {
        edits.push(import_edit);
    }

    edits.push(TextEdit {
        file: file.to_string(),
        range: TextRange::new(insert_offset, insert_offset),
        replacement: insert_text,
    });

    for range in occurrences {
        edits.push(TextEdit {
            file: file.to_string(),
            range,
            replacement: name.clone(),
        });
    }

    Ok(ExtractOutcome { edits, name })
}

fn parse_java(source: &str) -> Result<Tree, ExtractError> {
    let mut parser = Parser::new();
    parser
        .set_language(tree_sitter_java::language())
        .map_err(|_| ExtractError::ParseError)?;
    parser.parse(source, None).ok_or(ExtractError::ParseError)
}

fn find_expression(root: Node, selection: TextRange) -> Option<Node> {
    let mut node = root.descendant_for_byte_range(selection.start, selection.end)?;
    loop {
        if is_expression_node(node) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn is_expression_node(node: Node) -> bool {
    let kind = node.kind();
    kind.ends_with("_expression")
        || matches!(
            kind,
            "identifier"
                | "field_access"
                | "array_access"
                | "method_invocation"
                | "class_instance_creation_expression"
                | "array_creation_expression"
                | "string_literal"
                | "character_literal"
                | "decimal_integer_literal"
                | "hex_integer_literal"
                | "octal_integer_literal"
                | "binary_integer_literal"
                | "decimal_floating_point_literal"
                | "hex_floating_point_literal"
                | "true"
                | "false"
                | "null_literal"
                | "this"
                | "super"
        )
}

fn enclosing_class_body(mut node: Node) -> Option<Node> {
    loop {
        if node.kind() == "class_body" {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn has_side_effects(expr: Node) -> bool {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "assignment_expression"
            | "update_expression"
            | "method_invocation"
            | "class_instance_creation_expression"
            | "array_creation_expression" => return true,
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_named() {
                stack.push(child);
            }
        }
    }
    false
}

fn infer_expr_type(source: &str, expr: Node) -> Option<TypeRef> {
    match expr.kind() {
        "string_literal" => Some(TypeRef::new("String")),
        "character_literal" => Some(TypeRef::new("char")),
        "true" | "false" | "boolean_literal" => Some(TypeRef::new("boolean")),
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => {
            let text = &source[expr.start_byte()..expr.end_byte()];
            if text.ends_with(['L', 'l']) {
                Some(TypeRef::new("long"))
            } else {
                Some(TypeRef::new("int"))
            }
        }
        "decimal_floating_point_literal" | "hex_floating_point_literal" => {
            let text = &source[expr.start_byte()..expr.end_byte()];
            if text.ends_with(['F', 'f']) {
                Some(TypeRef::new("float"))
            } else {
                Some(TypeRef::new("double"))
            }
        }
        "parenthesized_expression" => infer_expr_type(source, expr.named_child(0)?),
        "binary_expression" => {
            let left = expr.child_by_field_name("left")?;
            let right = expr.child_by_field_name("right")?;
            let op = expr.child_by_field_name("operator")?;
            let op_text = source[op.start_byte()..op.end_byte()].trim();
            if op_text == "+" {
                let lt = infer_expr_type(source, left);
                let rt = infer_expr_type(source, right);
                if lt.as_ref().map(|t| t.text()) == Some("String")
                    || rt.as_ref().map(|t| t.text()) == Some("String")
                {
                    return Some(TypeRef::new("String"));
                }
            }
            infer_expr_type(source, left).or_else(|| infer_expr_type(source, right))
        }
        _ => infer_type_from_declaration_initializer(source, expr),
    }
}

fn infer_type_from_declaration_initializer(source: &str, expr: Node) -> Option<TypeRef> {
    let parent = expr.parent()?;
    if parent.kind() != "variable_declarator" {
        return None;
    }
    let initializer = parent.child_by_field_name("value")?;
    if initializer.id() != expr.id() {
        return None;
    }

    // Walk up to a local variable or field declaration and grab the `type` child.
    let mut cur = parent.parent()?;
    while matches!(cur.kind(), "variable_declarator" | "variable_declarator_id") {
        cur = cur.parent()?;
    }

    let type_node = cur.child_by_field_name("type")?;
    let type_text = source[type_node.start_byte()..type_node.end_byte()].trim();
    (!type_text.is_empty()).then(|| TypeRef::new(type_text))
}

fn collect_field_names(source: &str, class_body: Node) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut cursor = class_body.walk();
    for child in class_body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let mut field_cursor = child.walk();
        for node in child.named_children(&mut field_cursor) {
            if node.kind() != "variable_declarator" {
                continue;
            }
            if let Some(id) = node.child_by_field_name("name") {
                let name = source[id.start_byte()..id.end_byte()].to_string();
                names.insert(name);
            }
        }
    }
    names
}

fn is_constant_field(field: Node, source: &str) -> bool {
    let mut cursor = field.walk();
    for child in field.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let modifiers_text = &source[child.start_byte()..child.end_byte()];
        return modifiers_text.contains("static") && modifiers_text.contains("final");
    }
    false
}

fn insertion_point(source: &str, class_body: Node, kind: ExtractKind) -> (usize, String, bool) {
    let members = class_body_members(class_body);

    let mut constant_fields = Vec::new();
    let mut all_fields = Vec::new();
    for member in &members {
        if member.kind() == "field_declaration" {
            all_fields.push(*member);
            if is_constant_field(*member, source) {
                constant_fields.push(*member);
            }
        }
    }

    let member_indent = if let Some(first) = members.first() {
        indentation_at(source, first.start_byte())
    } else {
        let class_indent = indentation_at(source, class_body.start_byte());
        format!("{class_indent}    ")
    };

    match kind {
        ExtractKind::Constant => {
            if let Some(last_const) = constant_fields.last() {
                let insert_offset = line_start_after(source, last_const.end_byte());
                let next_member = member_after(&members, last_const.end_byte());
                let needs_blank = next_member
                    .map(|m| !(m.kind() == "field_declaration" && is_constant_field(m, source)))
                    .unwrap_or(false);
                (insert_offset, member_indent, needs_blank)
            } else if let Some(first_member) = members.first() {
                let insert_offset = line_start(source, first_member.start_byte());
                (insert_offset, member_indent, true)
            } else {
                let insert_offset = class_body_inner_start(source, class_body);
                (insert_offset, member_indent, false)
            }
        }
        ExtractKind::Field => {
            if let Some(last_field) = all_fields.last() {
                let insert_offset = line_start_after(source, last_field.end_byte());
                let next_member = member_after(&members, last_field.end_byte());
                let needs_blank = next_member
                    .map(|m| m.kind() != "field_declaration")
                    .unwrap_or(false);
                (insert_offset, member_indent, needs_blank)
            } else if let Some(last_const) = constant_fields.last() {
                let insert_offset = line_start_after(source, last_const.end_byte());
                let next_member = member_after(&members, last_const.end_byte());
                let needs_blank = next_member
                    .map(|m| m.kind() != "field_declaration")
                    .unwrap_or(false);
                (insert_offset, member_indent, needs_blank)
            } else if let Some(first_member) = members.first() {
                let insert_offset = line_start(source, first_member.start_byte());
                (insert_offset, member_indent, true)
            } else {
                let insert_offset = class_body_inner_start(source, class_body);
                (insert_offset, member_indent, false)
            }
        }
    }
}

fn class_body_members(class_body: Node) -> Vec<Node> {
    let mut members = Vec::new();
    let mut cursor = class_body.walk();
    for child in class_body.named_children(&mut cursor) {
        if child.is_named() {
            members.push(child);
        }
    }
    members
}

fn member_after<'a>(members: &'a [Node], byte: usize) -> Option<Node<'a>> {
    members.iter().copied().find(|m| m.start_byte() >= byte)
}

fn class_body_inner_start(source: &str, class_body: Node) -> usize {
    let mut i = class_body.start_byte();
    let bytes = source.as_bytes();
    while i < bytes.len() && bytes[i] != b'{' {
        i += 1;
    }
    i = (i + 1).min(bytes.len());
    if bytes.get(i) == Some(&b'\n') {
        i += 1;
    }
    i
}

fn line_start(source: &str, byte: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = byte;
    while i > 0 && bytes[i - 1] != b'\n' {
        i -= 1;
    }
    i
}

fn line_start_after(source: &str, byte: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = byte;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'\n' {
        i += 1;
    }
    i
}

fn indentation_at(source: &str, byte: usize) -> String {
    let start = line_start(source, byte);
    let line = &source[start..byte];
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

fn suggest_name(source: &str, expr: Node, kind: ExtractKind, existing: &HashSet<String>) -> String {
    let base = match expr.kind() {
        "identifier" => source[expr.start_byte()..expr.end_byte()].to_string(),
        "field_access" => {
            let text = &source[expr.start_byte()..expr.end_byte()];
            text.rsplit('.').next().unwrap_or("value").to_string()
        }
        "string_literal" => "text".to_string(),
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => "value".to_string(),
        "decimal_floating_point_literal" | "hex_floating_point_literal" => "value".to_string(),
        "character_literal" => "ch".to_string(),
        _ => "value".to_string(),
    };

    let sanitized = sanitize_identifier(&base, kind);
    make_unique(sanitized, existing, kind)
}

fn sanitize_identifier(name: &str, kind: ExtractKind) -> String {
    let name = name.trim();
    if name.is_empty() {
        return default_name(kind);
    }

    let candidate = match kind {
        ExtractKind::Constant => to_upper_snake(name),
        ExtractKind::Field => to_lower_camel(name),
    };

    let mut candidate = if is_java_keyword(&candidate) {
        format!("{}_", candidate)
    } else {
        candidate
    };

    if candidate
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(true)
    {
        candidate.insert(0, '_');
    }

    if candidate.is_empty() {
        default_name(kind)
    } else {
        candidate
    }
}

fn default_name(kind: ExtractKind) -> String {
    match kind {
        ExtractKind::Constant => "EXTRACTED_CONSTANT".to_string(),
        ExtractKind::Field => "extractedField".to_string(),
    }
}

fn to_upper_snake(name: &str) -> String {
    let mut out = String::new();
    let mut prev_was_sep = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if ch.is_ascii_uppercase() && !prev_was_sep && !out.ends_with('_') {
                out.push('_');
            }
            out.push(ch.to_ascii_uppercase());
            prev_was_sep = false;
        } else {
            if !prev_was_sep {
                out.push('_');
            }
            prev_was_sep = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn to_lower_camel(name: &str) -> String {
    let words = split_words(name);
    if words.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&words[0].to_ascii_lowercase());
    for w in words.iter().skip(1) {
        out.push_str(&capitalize(w));
    }
    out
}

fn split_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in name.chars() {
        if ch == '_' || ch == '-' || ch == ' ' {
            if !current.is_empty() {
                words.push(current.clone());
                current.clear();
            }
            continue;
        }
        if ch.is_ascii_uppercase() && !current.is_empty() {
            words.push(current.clone());
            current.clear();
        }
        if ch.is_ascii_alphanumeric() {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
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
    )
}

fn make_unique(mut name: String, existing: &HashSet<String>, kind: ExtractKind) -> String {
    if !existing.contains(&name) {
        return name;
    }
    let base = name.clone();
    let mut i = 2;
    loop {
        name = match kind {
            ExtractKind::Constant => format!("{}{}", base, i),
            ExtractKind::Field => format!("{}{}", base, i),
        };
        if !existing.contains(&name) {
            return name;
        }
        i += 1;
    }
}

fn normalize_expr_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn find_equivalent_expressions(
    source: &str,
    class_body: Node,
    selected_expr: Node,
    selected_text: &str,
) -> Vec<TextRange> {
    let norm = normalize_expr_text(selected_text);
    let mut ranges = Vec::new();

    let mut stack = vec![class_body];
    while let Some(node) = stack.pop() {
        if is_expression_node(node) {
            let range = TextRange::new(node.start_byte(), node.end_byte());
            let text = &source[range.start..range.end];
            if normalize_expr_text(text) == norm {
                ranges.push(range);
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_named() {
                if child.id() == selected_expr.id() {
                    continue;
                }
                stack.push(child);
            }
        }
    }

    let selected_range = TextRange::new(selected_expr.start_byte(), selected_expr.end_byte());
    if !ranges.iter().any(|r| r == &selected_range) {
        ranges.push(selected_range);
    }

    ranges.sort_by_key(|r| (r.start, r.end));
    ranges.dedup();
    ranges
}

fn compute_type_and_import(
    file: &str,
    source: &str,
    ty: &TypeRef,
) -> (String, Option<TextEdit>) {
    if !ty.needs_import() {
        return (ty.text().to_string(), None);
    }
    let Some(fq) = ty.fully_qualified_base() else {
        return (ty.text().to_string(), None);
    };

    if source
        .lines()
        .any(|line| line.trim() == format!("import {};", fq))
    {
        return (ty.with_simple_base(), None);
    }

    let insert_pos = import_insertion_offset(source);
    let mut import_line = format!("import {};\n", fq);
    if !source[insert_pos..].starts_with('\n') {
        import_line.push('\n');
    }

    (
        ty.with_simple_base(),
        Some(TextEdit {
            file: file.to_string(),
            range: TextRange::new(insert_pos, insert_pos),
            replacement: import_line,
        }),
    )
}

fn import_insertion_offset(source: &str) -> usize {
    let mut last_import_end = None;
    let mut package_end = None;
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("package ") {
            package_end = Some(offset + line.len());
        }
        if trimmed.starts_with("import ") {
            last_import_end = Some(offset + line.len());
        }
        offset += line.len();
    }

    if let Some(end) = last_import_end {
        end
    } else if let Some(end) = package_end {
        end
    } else {
        0
    }
}

