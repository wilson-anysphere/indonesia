use crate::TextEdit;
use nova_index::TextRange;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineMethodOptions {
    pub inline_all: bool,
}

impl Default for InlineMethodOptions {
    fn default() -> Self {
        Self { inline_all: false }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InlineMethodError {
    #[error("cursor is not on a method invocation")]
    NotOnInvocation,
    #[error("invocation receiver is not supported for inlining")]
    UnsupportedReceiver,
    #[error("could not resolve invoked method declaration")]
    MethodNotFound,
    #[error("only private methods are supported for inlining")]
    MethodNotPrivate,
    #[error("method has unsupported modifiers (abstract/native/synchronized)")]
    UnsupportedModifiers,
    #[error("generic methods are not supported for inlining yet")]
    UnsupportedTypeParameters,
    #[error("recursive methods are not supported for inlining yet")]
    RecursiveMethod,
    #[error("methods with multiple return statements are not supported for inlining yet")]
    MultipleReturns,
    #[error("method has unsupported control flow for inlining yet")]
    UnsupportedControlFlow,
    #[error("call site is not a supported statement form for inlining yet")]
    UnsupportedCallSite,
    #[error("argument count mismatch")]
    ArgumentCountMismatch,
    #[error("void methods are not supported for inlining yet")]
    VoidMethodNotSupported,
}

#[derive(Debug, Clone)]
struct ParameterInfo {
    name: String,
    ty_text: String,
}

#[derive(Debug, Clone)]
struct MethodInfo<'tree> {
    name: String,
    is_private: bool,
    has_bad_modifiers: bool,
    has_type_params: bool,
    return_type_text: String,
    parameters: Vec<ParameterInfo>,
    body: Node<'tree>,
    decl_range: TextRange,
}

#[derive(Debug, Clone)]
struct InvocationInfo<'tree> {
    node: Node<'tree>,
    name: String,
    args: Vec<Node<'tree>>,
    receiver: Option<Node<'tree>>,
}

pub fn inline_method(
    file: &str,
    source: &str,
    cursor_byte_offset: usize,
    options: InlineMethodOptions,
) -> Result<Vec<TextEdit>, InlineMethodError> {
    let mut parser = Parser::new();
    parser
        .set_language(tree_sitter_java::language())
        .expect("tree-sitter-java language should load");

    let tree = parser
        .parse(source, None)
        .ok_or(InlineMethodError::NotOnInvocation)?;

    let root = tree.root_node();
    let invocation_node = find_invocation_at_offset(root, cursor_byte_offset)
        .ok_or(InlineMethodError::NotOnInvocation)?;
    let invocation_info =
        parse_invocation(invocation_node, source).ok_or(InlineMethodError::NotOnInvocation)?;

    let invocation_name = invocation_info.name.clone();
    let invocation_arg_count = invocation_info.args.len();

    let method_node = find_method_declaration(root, source, &invocation_info)
        .ok_or(InlineMethodError::MethodNotFound)?;
    let method_info = analyze_method(method_node, source)?;

    if !method_info.is_private {
        return Err(InlineMethodError::MethodNotPrivate);
    }
    if method_info.has_bad_modifiers {
        return Err(InlineMethodError::UnsupportedModifiers);
    }
    if method_info.has_type_params {
        return Err(InlineMethodError::UnsupportedTypeParameters);
    }

    if method_info.return_type_text.trim() == "void" {
        return Err(InlineMethodError::VoidMethodNotSupported);
    }

    if is_recursive(&method_info, source) {
        return Err(InlineMethodError::RecursiveMethod);
    }

    if has_unsupported_control_flow(&method_info, source) {
        return Err(InlineMethodError::UnsupportedControlFlow);
    }

    let return_stmt = extract_single_return_statement(&method_info, source)?;
    let return_expr = return_stmt
        .child_by_field_name("value")
        .or_else(|| return_stmt.named_child(0))
        .ok_or(InlineMethodError::MultipleReturns)?;

    // Call sites: either the single invocation at cursor or all invocations in the file.
    let call_sites = if options.inline_all {
        find_all_invocations(root, source, &invocation_name, invocation_arg_count)
    } else {
        vec![invocation_info]
    };

    // Receiver restrictions for the initial implementation: no receiver / this / super only.
    for site in &call_sites {
        if let Some(receiver) = site.receiver {
            let recv_text = source[receiver.byte_range()].trim();
            if recv_text != "this" && recv_text != "super" {
                return Err(InlineMethodError::UnsupportedReceiver);
            }
        }
    }

    let mut edits = Vec::new();

    for call_site in call_sites {
        let stmt =
            enclosing_statement(call_site.node).ok_or(InlineMethodError::UnsupportedCallSite)?;
        let stmt_range = node_replacement_range(source, stmt);

        let replacement =
            inline_into_statement(source, &method_info, return_expr, &call_site, stmt)?;

        edits.push(TextEdit {
            file: file.to_string(),
            range: stmt_range,
            replacement,
        });
    }

    if options.inline_all {
        edits.push(TextEdit {
            file: file.to_string(),
            range: method_info.decl_range,
            replacement: String::new(),
        });
    }

    Ok(edits)
}

fn find_invocation_at_offset<'tree>(root: Node<'tree>, offset: usize) -> Option<Node<'tree>> {
    let node = root.descendant_for_byte_range(offset, offset)?;
    let mut cur = node;
    loop {
        if cur.kind() == "method_invocation" {
            return Some(cur);
        }
        cur = cur.parent()?;
    }
}

fn find_named_child_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

fn parse_invocation<'tree>(invocation: Node<'tree>, source: &str) -> Option<InvocationInfo<'tree>> {
    if invocation.kind() != "method_invocation" {
        return None;
    }

    let receiver = invocation.child_by_field_name("object");

    let name_node = invocation
        .child_by_field_name("name")
        .or_else(|| find_named_child_by_kind(invocation, "identifier"))?;
    let name = source[name_node.byte_range()].to_string();

    let args_node = invocation
        .child_by_field_name("arguments")
        .or_else(|| find_named_child_by_kind(invocation, "argument_list"))?;
    let mut cursor = args_node.walk();
    let args: Vec<_> = args_node.named_children(&mut cursor).collect();

    Some(InvocationInfo {
        node: invocation,
        name,
        args,
        receiver,
    })
}

fn find_method_declaration<'tree>(
    root: Node<'tree>,
    source: &str,
    invocation: &InvocationInfo<'tree>,
) -> Option<Node<'tree>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "method_declaration" {
            let name_node = node
                .child_by_field_name("name")
                .or_else(|| find_named_child_by_kind(node, "identifier"));
            if let Some(name_node) = name_node {
                let name = source[name_node.byte_range()].trim();
                if name == invocation.name {
                    let params_node = node
                        .child_by_field_name("parameters")
                        .or_else(|| find_named_child_by_kind(node, "formal_parameters"))?;
                    let mut c = params_node.walk();
                    let param_count = params_node
                        .named_children(&mut c)
                        .filter(|p| {
                            p.kind() == "formal_parameter" || p.kind() == "spread_parameter"
                        })
                        .count();
                    if param_count == invocation.args.len() {
                        return Some(node);
                    }
                }
            }
        }

        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            stack.push(child);
        }
    }
    None
}

fn analyze_method<'tree>(
    method: Node<'tree>,
    source: &str,
) -> Result<MethodInfo<'tree>, InlineMethodError> {
    let name_node = method
        .child_by_field_name("name")
        .or_else(|| find_named_child_by_kind(method, "identifier"))
        .ok_or(InlineMethodError::MethodNotFound)?;
    let name = source[name_node.byte_range()].to_string();

    let modifiers_text = method
        .child_by_field_name("modifiers")
        .or_else(|| find_named_child_by_kind(method, "modifiers"))
        .map(|n| source[n.byte_range()].to_string())
        .unwrap_or_default();
    let is_private = modifiers_text.split_whitespace().any(|t| t == "private");
    let has_bad_modifiers = modifiers_text
        .split_whitespace()
        .any(|t| matches!(t, "abstract" | "native" | "synchronized"));

    let has_type_params = method
        .child_by_field_name("type_parameters")
        .or_else(|| find_named_child_by_kind(method, "type_parameters"))
        .is_some();

    let return_type_text = method
        .child_by_field_name("type")
        .map(|n| source[n.byte_range()].to_string())
        .unwrap_or_default();

    let params_node = method
        .child_by_field_name("parameters")
        .or_else(|| find_named_child_by_kind(method, "formal_parameters"))
        .ok_or(InlineMethodError::MethodNotFound)?;
    let mut params_cursor = params_node.walk();
    let mut parameters = Vec::new();
    for param in params_node
        .named_children(&mut params_cursor)
        .filter(|p| p.kind() == "formal_parameter")
    {
        let name_node = param
            .child_by_field_name("name")
            .or_else(|| find_named_child_by_kind(param, "identifier"));
        let ty_node = param.child_by_field_name("type").or_else(|| {
            // tree-sitter-java tends to expose type nodes as named children; keep this as a fallback.
            let mut cursor = param.walk();
            for child in param.named_children(&mut cursor) {
                if child.kind().ends_with("_type")
                    || matches!(
                        child.kind(),
                        "type_identifier"
                            | "scoped_type_identifier"
                            | "array_type"
                            | "generic_type"
                    )
                {
                    return Some(child);
                }
            }
            None
        });

        let (Some(name_node), Some(ty_node)) = (name_node, ty_node) else {
            continue;
        };
        parameters.push(ParameterInfo {
            name: source[name_node.byte_range()].to_string(),
            ty_text: source[ty_node.byte_range()].to_string(),
        });
    }

    let body = method
        .child_by_field_name("body")
        .or_else(|| find_named_child_by_kind(method, "block"))
        .ok_or(InlineMethodError::MethodNotFound)?;

    Ok(MethodInfo {
        name,
        is_private,
        has_bad_modifiers,
        has_type_params,
        return_type_text,
        parameters,
        body,
        decl_range: node_replacement_range(source, method),
    })
}

fn node_replacement_range(source: &str, node: Node<'_>) -> TextRange {
    let start = source[..node.start_byte()]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut end = node.end_byte();
    let bytes = source.as_bytes();
    while end < bytes.len() {
        match bytes[end] {
            b' ' | b'\t' => end += 1,
            b'\r' => {
                end += 1;
                if end < bytes.len() && bytes[end] == b'\n' {
                    end += 1;
                }
                break;
            }
            b'\n' => {
                end += 1;
                break;
            }
            _ => break,
        }
    }

    TextRange::new(start, end)
}

fn is_recursive(method: &MethodInfo<'_>, source: &str) -> bool {
    let mut stack = vec![method.body];
    while let Some(node) = stack.pop() {
        if node.kind() == "method_invocation" {
            if let Some(name_node) = node
                .child_by_field_name("name")
                .or_else(|| find_named_child_by_kind(node, "identifier"))
            {
                if source[name_node.byte_range()].trim() == method.name {
                    return true;
                }
            }
        }

        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            stack.push(child);
        }
    }
    false
}

fn has_unsupported_control_flow(method: &MethodInfo<'_>, _source: &str) -> bool {
    const UNSUPPORTED: &[&str] = &[
        "if_statement",
        "for_statement",
        "enhanced_for_statement",
        "while_statement",
        "do_statement",
        "switch_expression",
        "switch_statement",
        "try_statement",
        "catch_clause",
        "finally_clause",
        "synchronized_statement",
        "break_statement",
        "continue_statement",
        "yield_statement",
        "throw_statement",
    ];

    let mut stack = vec![method.body];
    while let Some(node) = stack.pop() {
        if UNSUPPORTED.contains(&node.kind()) {
            return true;
        }
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            stack.push(child);
        }
    }
    false
}

fn extract_single_return_statement<'tree>(
    method: &MethodInfo<'tree>,
    _source: &str,
) -> Result<Node<'tree>, InlineMethodError> {
    let mut cursor = method.body.walk();
    let stmts: Vec<_> = method.body.named_children(&mut cursor).collect();
    let return_stmts: Vec<_> = stmts
        .iter()
        .copied()
        .filter(|s| s.kind() == "return_statement")
        .collect();

    if return_stmts.len() != 1 {
        return Err(InlineMethodError::MultipleReturns);
    }

    // Enforce that the (single) return is the last top-level statement.
    if stmts.last().copied() != Some(return_stmts[0]) {
        return Err(InlineMethodError::MultipleReturns);
    }

    // For the first iteration we only support a "simple block": local variable declarations
    // followed by a single return.
    for stmt in stmts.iter().take(stmts.len().saturating_sub(1)) {
        if stmt.kind() != "local_variable_declaration" {
            return Err(InlineMethodError::UnsupportedControlFlow);
        }
    }

    Ok(return_stmts[0])
}

fn enclosing_statement<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let mut cur = node;
    loop {
        let kind = cur.kind();
        if kind.ends_with("_statement") || kind == "local_variable_declaration" {
            return Some(cur);
        }
        cur = cur.parent()?;
    }
}

fn find_all_invocations<'tree>(
    root: Node<'tree>,
    source: &str,
    name: &str,
    arg_count: usize,
) -> Vec<InvocationInfo<'tree>> {
    let mut invocations = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "method_invocation" {
            if let Some(info) = parse_invocation(node, source) {
                if info.name == name && info.args.len() == arg_count {
                    invocations.push(info);
                }
            }
        }

        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            stack.push(child);
        }
    }

    invocations
}

fn inline_into_statement<'tree>(
    source: &str,
    method: &MethodInfo<'tree>,
    return_expr: Node<'tree>,
    call_site: &InvocationInfo<'tree>,
    stmt: Node<'tree>,
) -> Result<String, InlineMethodError> {
    if method.parameters.len() != call_site.args.len() {
        return Err(InlineMethodError::ArgumentCountMismatch);
    }

    let (_, indent) = line_indent_at(source, stmt.start_byte());

    let enclosing_method =
        enclosing_method_declaration(stmt).ok_or(InlineMethodError::UnsupportedCallSite)?;
    let mut used_names = collect_declared_names(enclosing_method, source);

    // Parameter temp variables: evaluate arguments exactly once, in order.
    let mut param_value_names = HashMap::new();
    let mut param_decls = String::new();
    for (param, arg) in method.parameters.iter().zip(call_site.args.iter()) {
        let base = format!("{}_arg", param.name);
        let unique = make_unique_name(&base, &mut used_names);
        param_value_names.insert(param.name.clone(), unique.clone());
        let arg_text = source[arg.byte_range()].trim();
        param_decls.push_str(&format!(
            "{indent}{} {} = {};\n",
            param.ty_text.trim(),
            unique,
            arg_text
        ));
    }

    // Local variable renaming from the inlined body.
    let local_vars = top_level_local_var_names(method.body, source);
    let mut local_rename = HashMap::new();
    for name in local_vars {
        if used_names.contains(&name) {
            let base = format!("{name}_inlined");
            let unique = make_unique_name(&base, &mut used_names);
            local_rename.insert(name, unique);
        } else {
            used_names.insert(name.clone());
        }
    }

    let mut replacements = HashMap::new();
    replacements.extend(param_value_names);
    replacements.extend(local_rename);

    let mut inlined_stmts = String::new();
    let mut cursor = method.body.walk();
    let stmts: Vec<_> = method.body.named_children(&mut cursor).collect();
    for stmt_node in stmts.iter().take(stmts.len().saturating_sub(1)) {
        let rewritten = rewrite_with_identifier_map(*stmt_node, source, &replacements);
        inlined_stmts.push_str(&reindent_preserving_relative(&rewritten, &indent));
    }

    let rewritten_return_expr = rewrite_with_identifier_map(return_expr, source, &replacements);

    // Only support cases where the invocation is directly the return expression / initializer /
    // assignment rhs. For the initial implementation we focus on return statements.
    let replaced_stmt = match stmt.kind() {
        "return_statement" => {
            let value_node = stmt
                .child_by_field_name("value")
                .or_else(|| stmt.named_child(0));
            if value_node.map(|n| n.byte_range()) != Some(call_site.node.byte_range()) {
                return Err(InlineMethodError::UnsupportedCallSite);
            }
            format!("{indent}return {rewritten_return_expr};\n")
        }
        "local_variable_declaration" => {
            let initializer = local_var_initializer(stmt)?;
            if initializer.byte_range() != call_site.node.byte_range() {
                return Err(InlineMethodError::UnsupportedCallSite);
            }
            let decl_text = source[stmt.byte_range()].to_string();
            replace_range_in_text(
                &decl_text,
                stmt.byte_range(),
                call_site.node.byte_range(),
                &rewritten_return_expr,
            )
        }
        "expression_statement" => {
            let expr = stmt
                .named_child(0)
                .ok_or(InlineMethodError::UnsupportedCallSite)?;
            if expr.kind() == "assignment_expression" {
                let right = expr
                    .child_by_field_name("right")
                    .or_else(|| expr.named_child(1));
                if right.map(|n| n.byte_range()) != Some(call_site.node.byte_range()) {
                    return Err(InlineMethodError::UnsupportedCallSite);
                }
                let stmt_text = source[stmt.byte_range()].to_string();
                replace_range_in_text(
                    &stmt_text,
                    stmt.byte_range(),
                    call_site.node.byte_range(),
                    &rewritten_return_expr,
                )
            } else {
                return Err(InlineMethodError::UnsupportedCallSite);
            }
        }
        _ => return Err(InlineMethodError::UnsupportedCallSite),
    };

    let mut out = String::new();
    out.push_str(&param_decls);
    out.push_str(&inlined_stmts);
    out.push_str(&reindent_preserving_relative(&replaced_stmt, &indent));

    Ok(out)
}

fn local_var_initializer<'tree>(stmt: Node<'tree>) -> Result<Node<'tree>, InlineMethodError> {
    if stmt.kind() != "local_variable_declaration" {
        return Err(InlineMethodError::UnsupportedCallSite);
    }
    let mut cursor = stmt.walk();
    let declarators: Vec<_> = stmt
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "variable_declarator")
        .collect();
    if declarators.len() != 1 {
        return Err(InlineMethodError::UnsupportedCallSite);
    }
    declarators[0]
        .child_by_field_name("value")
        .ok_or(InlineMethodError::UnsupportedCallSite)
}

fn line_indent_at(text: &str, byte: usize) -> (usize, String) {
    let line_start = text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let indent = text[line_start..byte]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect::<String>();
    (line_start, indent)
}

fn enclosing_method_declaration<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let mut cur = node;
    loop {
        if cur.kind() == "method_declaration" {
            return Some(cur);
        }
        cur = cur.parent()?;
    }
}

fn collect_declared_names<'tree>(method: Node<'tree>, source: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut stack = vec![method];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "formal_parameter" => {
                if let Some(name_node) = node
                    .child_by_field_name("name")
                    .or_else(|| find_named_child_by_kind(node, "identifier"))
                {
                    names.insert(source[name_node.byte_range()].to_string());
                }
            }
            "variable_declarator" => {
                if let Some(name_node) = node
                    .child_by_field_name("name")
                    .or_else(|| find_named_child_by_kind(node, "identifier"))
                {
                    names.insert(source[name_node.byte_range()].to_string());
                }
            }
            _ => {}
        }

        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            stack.push(child);
        }
    }
    names
}

fn make_unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if !used.contains(base) {
        used.insert(base.to_string());
        return base.to_string();
    }
    for i in 1.. {
        let candidate = format!("{base}{i}");
        if !used.contains(&candidate) {
            used.insert(candidate.clone());
            return candidate;
        }
    }
    unreachable!()
}

fn top_level_local_var_names<'tree>(body: Node<'tree>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = body.walk();
    for stmt in body.named_children(&mut cursor) {
        if stmt.kind() != "local_variable_declaration" {
            continue;
        }
        let mut c = stmt.walk();
        for declarator in stmt
            .named_children(&mut c)
            .filter(|child| child.kind() == "variable_declarator")
        {
            if let Some(name_node) = declarator
                .child_by_field_name("name")
                .or_else(|| find_named_child_by_kind(declarator, "identifier"))
            {
                names.push(source[name_node.byte_range()].to_string());
            }
        }
    }
    names
}

#[derive(Debug, Clone)]
struct LocalEdit {
    range: Range<usize>,
    replacement: String,
}

fn rewrite_with_identifier_map<'tree>(
    node: Node<'tree>,
    source: &str,
    replacements: &HashMap<String, String>,
) -> String {
    let mut edits = Vec::new();
    collect_identifier_rewrites(node, source, replacements, &mut edits);
    apply_rewrites(&source[node.byte_range()], node.start_byte(), edits)
}

fn collect_identifier_rewrites<'tree>(
    node: Node<'tree>,
    source: &str,
    replacements: &HashMap<String, String>,
    out: &mut Vec<LocalEdit>,
) {
    if node.kind() == "identifier" {
        let ident = source[node.byte_range()].to_string();
        if let Some(repl) = replacements.get(&ident) {
            // Skip method names in method invocations / field names in field accesses.
            if let Some(parent) = node.parent() {
                if parent.kind() == "method_invocation" {
                    if let Some(name_node) = parent.child_by_field_name("name") {
                        if name_node.byte_range() == node.byte_range() {
                            return;
                        }
                    }
                }
                if parent.kind() == "field_access" {
                    if let Some(field_node) = parent.child_by_field_name("field") {
                        if field_node.byte_range() == node.byte_range() {
                            return;
                        }
                    }
                }
            }
            out.push(LocalEdit {
                range: node.byte_range(),
                replacement: repl.clone(),
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_identifier_rewrites(child, source, replacements, out);
    }
}

fn apply_rewrites(fragment: &str, base_offset: usize, mut edits: Vec<LocalEdit>) -> String {
    if edits.is_empty() {
        return fragment.to_string();
    }

    edits.sort_by_key(|e| e.range.start);
    let mut out = fragment.to_string();
    for edit in edits.into_iter().rev() {
        let start = edit.range.start - base_offset;
        let end = edit.range.end - base_offset;
        out.replace_range(start..end, &edit.replacement);
    }
    out
}

fn reindent_preserving_relative(text: &str, new_indent: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= 1 {
        return format!("{new_indent}{}\n", text.trim_end());
    }

    let mut min_indent = None::<usize>;
    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }
        let count = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
        min_indent = Some(min_indent.map_or(count, |m| m.min(count)));
    }
    let min_indent = min_indent.unwrap_or(0);

    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == lines.len() - 1 && line.is_empty() {
            break;
        }
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let stripped = line.chars().skip(min_indent).collect::<String>();
        out.push_str(new_indent);
        out.push_str(stripped.trim_end());
        out.push('\n');
    }
    out
}

fn replace_range_in_text(
    original_stmt_text: &str,
    stmt_range: Range<usize>,
    target_range: Range<usize>,
    replacement: &str,
) -> String {
    let relative_start = target_range.start - stmt_range.start;
    let relative_end = target_range.end - stmt_range.start;
    let mut out = original_stmt_text.to_string();
    out.replace_range(relative_start..relative_end, replacement);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}
