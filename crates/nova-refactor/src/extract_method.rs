use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

use crate::{TextEdit, TextRange};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Private,
    Protected,
    Public,
    /// Java's package-private visibility (no modifier).
    PackagePrivate,
}

impl Visibility {
    fn keyword(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Protected => "protected",
            Visibility::Public => "public",
            Visibility::PackagePrivate => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InsertionStrategy {
    /// Insert the extracted method immediately after the enclosing method.
    AfterCurrentMethod,
    /// Insert the extracted method at the end of the enclosing class.
    EndOfClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    pub ty: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnValue {
    pub name: String,
    pub ty: String,
    pub declared_in_selection: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractRegionKind {
    Statements,
    Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlowHazard {
    Return,
    Break,
    Continue,
    Throw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractMethodIssue {
    InvalidSelection,
    NameCollision { name: String },
    MultipleReturnValues { names: Vec<String> },
    IllegalControlFlow { hazard: ControlFlowHazard },
    UnknownType { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractMethodAnalysis {
    pub region: ExtractRegionKind,
    pub parameters: Vec<Parameter>,
    pub return_value: Option<ReturnValue>,
    pub thrown_exceptions: Vec<String>,
    pub hazards: Vec<ControlFlowHazard>,
    pub issues: Vec<ExtractMethodIssue>,
}

impl ExtractMethodAnalysis {
    pub fn is_extractable(&self) -> bool {
        self.issues.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractMethod {
    /// File containing the selection (workspace-relative or absolute path).
    pub file: String,
    pub selection: TextRange,
    pub name: String,
    pub visibility: Visibility,
    pub insertion_strategy: InsertionStrategy,
}

impl ExtractMethod {
    pub fn analyze(&self, source: &str) -> Result<ExtractMethodAnalysis, String> {
        let selection = trim_range(source, self.selection);
        if selection.len() == 0 {
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Statements,
                parameters: Vec::new(),
                return_value: None,
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues: vec![ExtractMethodIssue::InvalidSelection],
            });
        }

        let tree = parse(source)?;
        let root = tree.root_node();

        let selection_node = root
            .descendant_for_byte_range(selection.start, selection.end)
            .ok_or("selection not inside syntax tree")?;

        let method = find_ancestor(selection_node, |n| n.kind() == "method_declaration")
            .ok_or("selection must be inside a method")?;
        let class = find_ancestor(method, |n| n.kind() == "class_declaration")
            .ok_or("selection must be inside a class")?;

        let mut issues = Vec::new();

        if class_has_method_named(source, class, &self.name) {
            issues.push(ExtractMethodIssue::NameCollision {
                name: self.name.clone(),
            });
        }

        let method_body = method
            .child_by_field_name("body")
            .ok_or("method has no body")?;

        let enclosing_block = smallest_enclosing_block(method_body, selection)
            .ok_or("selection must be inside a block")?;

        let (region, extracted_nodes) =
            classify_selection(source, enclosing_block, selection).unwrap_or_else(|| {
                (
                    ExtractRegionKind::Statements,
                    ExtractedNodes::Invalid,
                )
            });

        if matches!(extracted_nodes, ExtractedNodes::Invalid) {
            issues.push(ExtractMethodIssue::InvalidSelection);
        }

        let mut hazards = Vec::new();
        let mut thrown_exceptions = Vec::new();
        collect_control_flow_and_exceptions(source, root, selection, &mut hazards, &mut thrown_exceptions);

        for hazard in &hazards {
            match hazard {
                ControlFlowHazard::Throw => {
                    // Allowed; handled via thrown exception list.
                }
                _ => issues.push(ExtractMethodIssue::IllegalControlFlow { hazard: *hazard }),
            }
        }

        let locals = collect_method_locals_and_params(source, method);
        let defined_in_selection =
            collect_definitions_in_range(source, root, selection);
        let reads_in_selection =
            collect_reads_in_range(source, root, selection, &locals.names);
        let writes_in_selection =
            collect_writes_in_range(source, root, selection, &locals.names);

        let reads_after = collect_reads_in_range(
            source,
            root,
            TextRange::new(selection.end, method_body.end_byte()),
            &locals.names,
        );

        let mut return_candidates: Vec<String> = writes_in_selection
            .iter()
            .filter(|name| reads_after.contains(*name))
            .cloned()
            .collect();
        return_candidates.sort();
        return_candidates.dedup();

        let return_value = match return_candidates.as_slice() {
            [] => None,
            [name] => {
                let ty = match locals.types.get(name).cloned() {
                    Some(ty) => ty,
                    None => {
                        issues.push(ExtractMethodIssue::UnknownType { name: (*name).clone() });
                        "Object".to_string()
                    }
                };

                Some(ReturnValue {
                    name: (*name).clone(),
                    ty,
                    declared_in_selection: defined_in_selection.contains(name),
                })
            }
            many => {
                issues.push(ExtractMethodIssue::MultipleReturnValues {
                    names: many.to_vec(),
                });
                None
            }
        };

        let mut parameters = Vec::new();
        let mut seen = HashSet::new();
        for name in reads_in_selection {
            if defined_in_selection.contains(&name) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }

            // If the variable is only ever written in the selection and never read,
            // it won't be in reads_in_selection. So we only consider reads.
            if let Some(ty) = locals.types.get(&name).cloned() {
                parameters.push(Parameter { name, ty });
            } else {
                issues.push(ExtractMethodIssue::UnknownType { name });
            }
        }

        Ok(ExtractMethodAnalysis {
            region,
            parameters,
            return_value,
            thrown_exceptions: thrown_exceptions.into_iter().collect(),
            hazards,
            issues,
        })
    }

    pub fn apply(&self, source: &str) -> Result<Vec<TextEdit>, String> {
        let analysis = self.analyze(source)?;
        if !analysis.is_extractable() {
            return Err(format!("extract method is not applicable: {:?}", analysis.issues));
        }

        let selection = trim_range(source, self.selection);
        let tree = parse(source)?;
        let root = tree.root_node();
        let selection_node = root
            .descendant_for_byte_range(selection.start, selection.end)
            .ok_or("selection not inside syntax tree")?;

        let method = find_ancestor(selection_node, |n| n.kind() == "method_declaration")
            .ok_or("selection must be inside a method")?;
        let class = find_ancestor(method, |n| n.kind() == "class_declaration")
            .ok_or("selection must be inside a class")?;

        let method_body = method
            .child_by_field_name("body")
            .ok_or("method has no body")?;
        let enclosing_block = smallest_enclosing_block(method_body, selection)
            .ok_or("selection must be inside a block")?;

        let (_, extracted_nodes) =
            classify_selection(source, enclosing_block, selection).ok_or("invalid selection")?;

        let method_indent = indentation_at(source, method.start_byte());
        let call_indent = indentation_at(source, selection.start);

        let insertion_offset = match self.insertion_strategy {
            InsertionStrategy::AfterCurrentMethod => method.end_byte(),
            InsertionStrategy::EndOfClass => {
                let body = class
                    .child_by_field_name("body")
                    .ok_or("class has no body")?;
                // Insert immediately before the newline that starts the closing brace line.
                let closing_brace = body.end_byte().saturating_sub(1);
                let brace_line_start = line_start_offset(source, closing_brace);
                brace_line_start.saturating_sub(1)
            }
        };

        let extracted_text = match &extracted_nodes {
            ExtractedNodes::Statements(range) | ExtractedNodes::Expression(range) => {
                source
                    .get(range.start..range.end)
                    .ok_or("selection out of bounds")?
                    .to_string()
            }
            ExtractedNodes::Invalid => return Err("invalid selection".into()),
        };

        let new_body_indent = format!("{method_indent}    ");
        let extracted_body = match analysis.region {
            ExtractRegionKind::Statements => reindent(&extracted_text, &call_indent, &new_body_indent),
            ExtractRegionKind::Expression => {
                // Expression extraction: wrap in `return`.
                let expr = extracted_text.trim().to_string();
                format!("{new_body_indent}return {expr};\n")
            }
        };

        let mut method_body_text = extracted_body;
        if !method_body_text.ends_with('\n') {
            method_body_text.push('\n');
        }

        if analysis.region == ExtractRegionKind::Statements {
            if let Some(ret) = &analysis.return_value {
                // If the return variable isn't a parameter, declare it so the extracted
                // statements can assign to it.
                let declared_as_param = analysis.parameters.iter().any(|p| p.name == ret.name);
                if !ret.declared_in_selection && !declared_as_param {
                    let decl = format!("{new_body_indent}{} {};\n", ret.ty, ret.name);
                    method_body_text = format!("{decl}{method_body_text}");
                }

                method_body_text.push_str(&format!("{new_body_indent}return {};\n", ret.name));
            }
        }

        let mut thrown = analysis.thrown_exceptions.clone();
        thrown.sort();
        thrown.dedup();
        let throws_clause = if thrown.is_empty() {
            String::new()
        } else {
            format!(" throws {}", thrown.join(", "))
        };

        let return_ty = match analysis.region {
            ExtractRegionKind::Expression => "Object".to_string(),
            ExtractRegionKind::Statements => analysis
                .return_value
                .as_ref()
                .map(|r| r.ty.clone())
                .unwrap_or_else(|| "void".to_string()),
        };

        let params_sig = analysis
            .parameters
            .iter()
            .map(|p| format!("{} {}", p.ty, p.name))
            .collect::<Vec<_>>()
            .join(", ");

        let vis_kw = self.visibility.keyword();
        let signature = if vis_kw.is_empty() {
            format!(
                "{method_indent}{return_ty} {}({params_sig}){throws_clause} {{\n",
                self.name
            )
        } else {
            format!(
                "{method_indent}{vis_kw} {return_ty} {}({params_sig}){throws_clause} {{\n",
                self.name
            )
        };

        let new_method_text = format!("\n\n{signature}{method_body_text}{method_indent}}}");

        let args = analysis
            .parameters
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let call_expr = format!("{}({})", self.name, args);

        let replacement = match analysis.region {
            ExtractRegionKind::Expression => call_expr,
            ExtractRegionKind::Statements => {
                if let Some(ret) = analysis.return_value {
                    if ret.declared_in_selection {
                        format!("{} {} = {call_expr};", ret.ty, ret.name)
                    } else {
                        format!("{} = {call_expr};", ret.name)
                    }
                } else {
                    format!("{call_expr};")
                }
            }
        };

        Ok(vec![
            replace_edit(&self.file, selection, replacement),
            insert_edit(&self.file, insertion_offset, new_method_text),
        ])
    }
}

struct Locals {
    names: HashSet<String>,
    types: HashMap<String, String>,
}

fn parse(source: &str) -> Result<tree_sitter::Tree, String> {
    let mut parser = Parser::new();
    parser
        .set_language(tree_sitter_java::language())
        .map_err(|_| "failed to load Java grammar")?;
    parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())
}

fn trim_range(source: &str, mut range: TextRange) -> TextRange {
    let bytes = source.as_bytes();
    while range.start < range.end && bytes[range.start].is_ascii_whitespace() {
        range.start += 1;
    }
    while range.start < range.end && bytes[range.end - 1].is_ascii_whitespace() {
        range.end -= 1;
    }
    range
}

fn find_ancestor(mut node: Node<'_>, mut pred: impl FnMut(Node<'_>) -> bool) -> Option<Node<'_>> {
    loop {
        if pred(node) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn indentation_at(source: &str, offset: usize) -> String {
    let line_start = line_start_offset(source, offset);
    source[line_start..offset]
        .chars()
        .take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
        .collect()
}

fn line_start_offset(source: &str, offset: usize) -> usize {
    source[..offset]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

fn class_has_method_named(source: &str, class: Node<'_>, name: &str) -> bool {
    let Some(body) = class.child_by_field_name("body") else {
        return false;
    };

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let Some(id) = child.child_by_field_name("name") else {
            continue;
        };
        if node_text(source, id) == name {
            return true;
        }
    }
    false
}

fn smallest_enclosing_block<'a>(method_body: Node<'a>, selection: TextRange) -> Option<Node<'a>> {
    let mut best: Option<Node<'a>> = None;
    let mut stack = vec![method_body];
    while let Some(node) = stack.pop() {
        if node.kind() == "block"
            && node.start_byte() <= selection.start
            && node.end_byte() >= selection.end
        {
            let replace = match best {
                None => true,
                Some(prev) => (node.end_byte() - node.start_byte()) < (prev.end_byte() - prev.start_byte()),
            };
            if replace {
                best = Some(node);
            }
        }

        let mut child_cursor = node.walk();
        for child in node.named_children(&mut child_cursor) {
            if child.start_byte() <= selection.start && child.end_byte() >= selection.end {
                stack.push(child);
            }
        }
    }

    // Ensure we return a block (for a method body it's already a block).
    match best {
        Some(node) if node.kind() == "block" => Some(node),
        _ => None,
    }
}

enum ExtractedNodes {
    Statements(TextRange),
    Expression(TextRange),
    Invalid,
}

fn classify_selection(
    source: &str,
    block: Node<'_>,
    selection: TextRange,
) -> Option<(ExtractRegionKind, ExtractedNodes)> {
    let mut statements = Vec::new();
    let mut cursor = block.walk();
    for child in block.named_children(&mut cursor) {
        statements.push(child);
    }

    // Statement range: selection must match whole block_statement nodes.
    let mut start_idx = None;
    let mut end_idx = None;
    for (idx, stmt) in statements.iter().enumerate() {
        if stmt.start_byte() == selection.start {
            start_idx = Some(idx);
        }
        if stmt.end_byte() == selection.end {
            end_idx = Some(idx);
        }
    }

    if let (Some(s), Some(e)) = (start_idx, end_idx) {
        if s <= e {
            return Some((
                ExtractRegionKind::Statements,
                ExtractedNodes::Statements(selection),
            ));
        }
    }

    // Expression range: exact match for a single expression node.
    let node = block.descendant_for_byte_range(selection.start, selection.end)?;
    if node.start_byte() == selection.start
        && node.end_byte() == selection.end
        && is_expression_kind(node.kind())
    {
        return Some((
            ExtractRegionKind::Expression,
            ExtractedNodes::Expression(selection),
        ));
    }

    let _ = source;
    Some((ExtractRegionKind::Statements, ExtractedNodes::Invalid))
}

fn is_expression_kind(kind: &str) -> bool {
    kind.ends_with("_expression")
        || matches!(
            kind,
            "identifier"
                | "this"
                | "super"
                | "string_literal"
                | "decimal_integer_literal"
                | "decimal_floating_point_literal"
                | "true"
                | "false"
                | "null_literal"
                | "method_invocation"
                | "field_access"
                | "array_access"
                | "object_creation_expression"
                | "parenthesized_expression"
        )
}

fn collect_control_flow_and_exceptions(
    source: &str,
    root: Node<'_>,
    selection: TextRange,
    hazards: &mut Vec<ControlFlowHazard>,
    thrown: &mut Vec<String>,
) {
    walk_nodes_in_range(root, selection, &mut |node| {
        match node.kind() {
            "return_statement" => hazards.push(ControlFlowHazard::Return),
            "break_statement" => hazards.push(ControlFlowHazard::Break),
            "continue_statement" => hazards.push(ControlFlowHazard::Continue),
            "throw_statement" => {
                hazards.push(ControlFlowHazard::Throw);
                if let Some(exc) = thrown_exception_type(source, node) {
                    thrown.push(exc);
                }
            }
            _ => {}
        }
    });
}

fn thrown_exception_type(source: &str, throw_stmt: Node<'_>) -> Option<String> {
    // `throw new Foo();` – take the constructed type as the thrown exception.
    let expr = throw_stmt.child_by_field_name("expression")?;
    if expr.kind() != "object_creation_expression" {
        return None;
    }
    let ty = expr.child_by_field_name("type")?;
    Some(node_text(source, ty))
}

fn collect_method_locals_and_params(source: &str, method: Node<'_>) -> Locals {
    let mut names = HashSet::new();
    let mut types = HashMap::new();

    walk(method, &mut |node| {
        match node.kind() {
            "formal_parameter" => {
                if let (Some(ty), Some(name)) =
                    (node.child_by_field_name("type"), node.child_by_field_name("name"))
                {
                    let name = node_text(source, name);
                    let ty_text = node_text(source, ty);
                    names.insert(name.clone());
                    if ty_text.trim() != "var" {
                        types.insert(name, ty_text);
                    }
                }
            }
            "local_variable_declaration" => {
                let Some(ty) = node.child_by_field_name("type") else {
                    return;
                };
                let ty_text = node_text(source, ty);
                let mut decl_cursor = node.walk();
                for child in node.named_children(&mut decl_cursor) {
                    if child.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name) = child.child_by_field_name("name") else {
                        continue;
                    };
                    let name = node_text(source, name);
                    names.insert(name.clone());
                    if ty_text.trim() != "var" {
                        types.insert(name, ty_text.clone());
                    }
                }
            }
            _ => {}
        }
    });

    Locals { names, types }
}

fn collect_definitions_in_range(source: &str, root: Node<'_>, range: TextRange) -> HashSet<String> {
    let mut defs = HashSet::new();
    walk_nodes_in_range(root, range, &mut |node| {
        if node.kind() == "variable_declarator" {
            if let Some(name) = node.child_by_field_name("name") {
                let name = node_text(source, name);
                defs.insert(name);
            }
        }
    });
    defs
}

fn collect_reads_in_range(
    source: &str,
    root: Node<'_>,
    range: TextRange,
    known: &HashSet<String>,
) -> Vec<String> {
    let mut reads = Vec::new();
    walk_nodes_in_range(root, range, &mut |node| {
        if node.kind() != "identifier" {
            return;
        }
        let name = node_text(source, node);
        if !known.contains(&name) {
            return;
        }
        if is_definition_identifier(node) {
            return;
        }
        if is_method_name_identifier(node) {
            return;
        }
        if is_pure_assignment_lhs(source, node) {
            return;
        }
        reads.push(name);
    });
    reads
}

fn collect_writes_in_range(
    source: &str,
    root: Node<'_>,
    range: TextRange,
    known: &HashSet<String>,
) -> HashSet<String> {
    let mut writes = HashSet::new();
    walk_nodes_in_range(root, range, &mut |node| {
        match node.kind() {
            "assignment_expression" => {
                let Some(left) = node.child_by_field_name("left") else {
                    return;
                };
                if left.kind() == "identifier" {
                    let name = node_text(source, left);
                    if known.contains(&name) {
                        writes.insert(name);
                    }
                }
            }
            "update_expression" => {
                // ++x / x--
                let Some(arg) = node.child_by_field_name("argument") else {
                    return;
                };
                if arg.kind() == "identifier" {
                    let name = node_text(source, arg);
                    if known.contains(&name) {
                        writes.insert(name);
                    }
                }
            }
            "variable_declarator" => {
                if node.start_byte() >= range.start && node.end_byte() <= range.end {
                    if let Some(name) = node.child_by_field_name("name") {
                        let name = node_text(source, name);
                        if known.contains(&name) {
                            writes.insert(name);
                        }
                    }
                }
            }
            _ => {}
        }
    });
    writes
}

fn is_definition_identifier(node: Node<'_>) -> bool {
    matches!(
        node.parent().map(|p| p.kind()),
        Some("variable_declarator")
            | Some("variable_declarator_id")
            | Some("formal_parameter")
            | Some("catch_formal_parameter")
    )
}

fn is_method_name_identifier(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "method_invocation" => parent.child_by_field_name("name").is_some_and(|n| {
            n.start_byte() == node.start_byte() && n.end_byte() == node.end_byte()
        }),
        "method_declaration" => parent.child_by_field_name("name").is_some_and(|n| {
            n.start_byte() == node.start_byte() && n.end_byte() == node.end_byte()
        }),
        _ => false,
    }
}

fn is_pure_assignment_lhs(source: &str, node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "assignment_expression" {
        return false;
    }

    let Some(left) = parent.child_by_field_name("left") else {
        return false;
    };
    if left.start_byte() != node.start_byte() || left.end_byte() != node.end_byte() {
        return false;
    }

    // Only treat plain `=` assignments as pure writes. Compound assignments (`+=`, etc.)
    // read the previous value.
    let op_text = if let Some(op) = parent.child_by_field_name("operator") {
        node_text(source, op)
    } else if let Some(right) = parent.child_by_field_name("right") {
        source[left.end_byte()..right.start_byte()].to_string()
    } else {
        // Best-effort: assume simple assignment.
        return true;
    };

    op_text.trim() == "="
}

fn walk(node: Node<'_>, f: &mut impl FnMut(Node<'_>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, f);
    }
}

fn walk_nodes_in_range(node: Node<'_>, range: TextRange, f: &mut impl FnMut(Node<'_>)) {
    if node.end_byte() <= range.start || node.start_byte() >= range.end {
        return;
    }

    if node.start_byte() >= range.start && node.end_byte() <= range.end {
        f(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_nodes_in_range(child, range, f);
    }
}

fn node_text(source: &str, node: Node<'_>) -> String {
    source[node.start_byte()..node.end_byte()].to_string()
}

fn reindent(text: &str, old_indent: &str, new_indent: &str) -> String {
    let mut out = String::new();
    for chunk in text.split_inclusive('\n') {
        let (line, newline) = if chunk.ends_with('\n') {
            (&chunk[..chunk.len() - 1], "\n")
        } else {
            (chunk, "")
        };

        if line.trim().is_empty() {
            out.push_str(newline);
            continue;
        }

        let stripped = line.strip_prefix(old_indent).unwrap_or(line);
        out.push_str(new_indent);
        out.push_str(stripped);
        out.push_str(newline);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn replace_edit(file: &str, range: TextRange, replacement: impl Into<String>) -> TextEdit {
    TextEdit {
        file: file.to_string(),
        range,
        replacement: replacement.into(),
    }
}

fn insert_edit(file: &str, offset: usize, text: impl Into<String>) -> TextEdit {
    TextEdit {
        file: file.to_string(),
        range: TextRange::new(offset, offset),
        replacement: text.into(),
    }
}
