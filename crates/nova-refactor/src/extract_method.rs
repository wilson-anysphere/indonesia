use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use nova_syntax::ast::{self, AstNode};
use nova_syntax::{parse_java, SyntaxKind};

use crate::edit::{FileId, TextEdit as WorkspaceTextEdit, TextRange, WorkspaceEdit};

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
        if selection.len() == 0 || selection.end > source.len() {
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Statements,
                parameters: Vec::new(),
                return_value: None,
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues: vec![ExtractMethodIssue::InvalidSelection],
            });
        }

        let parsed = parse_java(source);
        if !parsed.errors.is_empty() {
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Statements,
                parameters: Vec::new(),
                return_value: None,
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues: vec![ExtractMethodIssue::InvalidSelection],
            });
        }

        let root = parsed.syntax();
        let Some((method, method_body)) = find_enclosing_method(root.clone(), selection) else {
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Statements,
                parameters: Vec::new(),
                return_value: None,
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues: vec![ExtractMethodIssue::InvalidSelection],
            });
        };

        let class_decl = method
            .syntax()
            .ancestors()
            .find_map(ast::ClassDeclaration::cast);

        let mut issues = Vec::new();
        if let Some(class_decl) = class_decl.as_ref() {
            if class_has_method_named(class_decl, &self.name) {
                issues.push(ExtractMethodIssue::NameCollision {
                    name: self.name.clone(),
                });
            }
        }

        let Some(selected_stmt) = find_statement_exact(&method_body, selection) else {
            issues.push(ExtractMethodIssue::InvalidSelection);
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Statements,
                parameters: Vec::new(),
                return_value: None,
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues,
            });
        };

        let mut hazards = Vec::new();
        match &selected_stmt {
            ast::Statement::ReturnStatement(_) => hazards.push(ControlFlowHazard::Return),
            ast::Statement::BreakStatement(_) => hazards.push(ControlFlowHazard::Break),
            ast::Statement::ContinueStatement(_) => hazards.push(ControlFlowHazard::Continue),
            ast::Statement::ThrowStatement(_) => hazards.push(ControlFlowHazard::Throw),
            _ => {}
        }

        for hazard in &hazards {
            match hazard {
                ControlFlowHazard::Throw => {
                    // Allowed; would be modeled as `throws` in the future.
                }
                _ => issues.push(ExtractMethodIssue::IllegalControlFlow { hazard: *hazard }),
            }
        }

        // Collect locals and parameters for the enclosing method.
        let (param_types, local_types, declared_in_selection) =
            collect_locals_and_params(source, &method, &method_body, selection);

        let known: HashSet<String> = param_types
            .keys()
            .chain(local_types.keys())
            .cloned()
            .collect();

        let (reads_in_selection, writes_in_selection) =
            collect_reads_writes_in_statement(&selected_stmt, &known);

        let reads_after = collect_reads_after_offset(&method_body, selection.end, &known);

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
                let ty = param_types
                    .get(name)
                    .or_else(|| local_types.get(name))
                    .cloned()
                    .unwrap_or_else(|| {
                        issues.push(ExtractMethodIssue::UnknownType { name: name.clone() });
                        "Object".to_string()
                    });
                Some(ReturnValue {
                    name: name.clone(),
                    ty,
                    declared_in_selection: declared_in_selection.contains(name),
                })
            }
            names => {
                issues.push(ExtractMethodIssue::MultipleReturnValues {
                    names: names.to_vec(),
                });
                None
            }
        };

        // Determine parameters in order of first appearance in the selected statement.
        let mut parameters = Vec::new();
        let mut seen = HashSet::new();
        for (name, _range) in reads_in_selection {
            if declared_in_selection.contains(&name) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(ty) = param_types
                .get(&name)
                .or_else(|| local_types.get(&name))
                .cloned()
            else {
                continue;
            };
            parameters.push(Parameter { name, ty });
        }

        Ok(ExtractMethodAnalysis {
            region: ExtractRegionKind::Statements,
            parameters,
            return_value,
            thrown_exceptions: Vec::new(),
            hazards,
            issues,
        })
    }

    pub fn apply(&self, source: &str) -> Result<WorkspaceEdit, String> {
        let analysis = self.analyze(source)?;
        if !analysis.is_extractable() {
            return Err(format!(
                "extract method is not applicable: {:?}",
                analysis.issues
            ));
        }

        let selection = trim_range(source, self.selection);
        let parsed = parse_java(source);
        if !parsed.errors.is_empty() {
            return Err("failed to parse source".to_string());
        }
        let root = parsed.syntax();

        let (method, _method_body) = find_enclosing_method(root.clone(), selection)
            .ok_or("selection must be inside a method")?;
        let class_decl = method
            .syntax()
            .ancestors()
            .find_map(ast::ClassDeclaration::cast)
            .ok_or("selection must be inside a class")?;

        let method_indent = indentation_at(source, syntax_range(method.syntax()).start);
        let call_indent = indentation_at(source, selection.start);

        let insertion_offset = match self.insertion_strategy {
            InsertionStrategy::AfterCurrentMethod => syntax_range(method.syntax()).end,
            InsertionStrategy::EndOfClass => insertion_offset_end_of_class(source, &class_decl),
        };

        let extracted_text = source
            .get(selection.start..selection.end)
            .ok_or("selection out of bounds")?
            .to_string();

        let new_body_indent = format!("{method_indent}    ");
        let extracted_body = reindent(&extracted_text, &call_indent, &new_body_indent);

        let mut method_body_text = extracted_body;
        if !method_body_text.ends_with('\n') {
            method_body_text.push('\n');
        }

        if let Some(ret) = &analysis.return_value {
            let declared_as_param = analysis.parameters.iter().any(|p| p.name == ret.name);
            if !ret.declared_in_selection && !declared_as_param {
                let decl = format!("{new_body_indent}{} {};\n", ret.ty, ret.name);
                method_body_text = format!("{decl}{method_body_text}");
            }
            method_body_text.push_str(&format!("{new_body_indent}return {};\n", ret.name));
        }

        let return_ty = analysis
            .return_value
            .as_ref()
            .map(|r| r.ty.clone())
            .unwrap_or_else(|| "void".to_string());

        let params_sig = analysis
            .parameters
            .iter()
            .map(|p| format!("{} {}", p.ty, p.name))
            .collect::<Vec<_>>()
            .join(", ");

        let vis_kw = self.visibility.keyword();
        let signature = if vis_kw.is_empty() {
            format!(
                "{method_indent}{return_ty} {}({params_sig}) {{\n",
                self.name
            )
        } else {
            format!(
                "{method_indent}{vis_kw} {return_ty} {}({params_sig}) {{\n",
                self.name
            )
        };

        let mut new_method_text = String::new();
        new_method_text.push_str("\n\n");
        new_method_text.push_str(&signature);
        new_method_text.push_str(&method_body_text);
        new_method_text.push_str(&method_indent);
        new_method_text.push('}');

        let args = analysis
            .parameters
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let call_expr = format!("{}({})", self.name, args);

        let replacement = if let Some(ret) = &analysis.return_value {
            if ret.declared_in_selection {
                format!("{} {} = {call_expr};", ret.ty, ret.name)
            } else {
                format!("{} = {call_expr};", ret.name)
            }
        } else {
            format!("{call_expr};")
        };

        let file_id = FileId::new(self.file.clone());
        let mut edit = WorkspaceEdit::new(vec![
            WorkspaceTextEdit::replace(file_id.clone(), selection, replacement),
            WorkspaceTextEdit::insert(file_id, insertion_offset, new_method_text),
        ]);
        edit.normalize().map_err(|e| e.to_string())?;
        Ok(edit)
    }
}

fn syntax_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn find_enclosing_method(
    root: nova_syntax::SyntaxNode,
    selection: TextRange,
) -> Option<(ast::MethodDeclaration, ast::Block)> {
    let mut best: Option<(usize, ast::MethodDeclaration, ast::Block)> = None;
    for method in root.descendants().filter_map(ast::MethodDeclaration::cast) {
        let Some(body) = method.body() else {
            continue;
        };
        let body_range = syntax_range(body.syntax());
        if body_range.start <= selection.start && selection.end <= body_range.end {
            let span = body_range.len();
            if best
                .as_ref()
                .is_none_or(|(best_span, _, _)| span < *best_span)
            {
                best = Some((span, method, body));
            }
        }
    }
    best.map(|(_, m, b)| (m, b))
}

fn find_statement_exact(body: &ast::Block, selection: TextRange) -> Option<ast::Statement> {
    body.syntax()
        .descendants()
        .filter_map(ast::Statement::cast)
        .find(|stmt| syntax_range(stmt.syntax()) == selection)
}

fn class_has_method_named(class_decl: &ast::ClassDeclaration, name: &str) -> bool {
    let Some(body) = class_decl.body() else {
        return false;
    };
    let found = body.members().any(|member| {
        let ast::ClassMember::MethodDeclaration(method) = member else {
            return false;
        };
        method.name_token().is_some_and(|tok| tok.text() == name)
    });
    found
}

fn collect_locals_and_params(
    source: &str,
    method: &ast::MethodDeclaration,
    body: &ast::Block,
    selection: TextRange,
) -> (
    HashMap<String, String>,
    HashMap<String, String>,
    HashSet<String>,
) {
    let mut param_types = HashMap::new();
    if let Some(params) = method.parameter_list() {
        for param in params.parameters() {
            let Some(name) = param.name_token() else {
                continue;
            };
            let Some(ty) = param.ty() else {
                continue;
            };
            let ty_text = source
                .get(syntax_range(ty.syntax()).start..syntax_range(ty.syntax()).end)
                .unwrap_or("Object")
                .trim()
                .to_string();
            param_types.insert(name.text().to_string(), ty_text);
        }
    }

    let mut local_types = HashMap::new();
    let mut declared_in_selection = HashSet::new();
    for stmt in body
        .syntax()
        .descendants()
        .filter_map(ast::LocalVariableDeclarationStatement::cast)
    {
        let Some(ty) = stmt.ty() else {
            continue;
        };
        let ty_text = source
            .get(syntax_range(ty.syntax()).start..syntax_range(ty.syntax()).end)
            .unwrap_or("Object")
            .trim()
            .to_string();
        let Some(list) = stmt.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            let Some(name) = decl.name_token() else {
                continue;
            };
            local_types.insert(name.text().to_string(), ty_text.clone());
            let decl_range = syntax_range(decl.syntax());
            if selection.start <= decl_range.start && decl_range.end <= selection.end {
                declared_in_selection.insert(name.text().to_string());
            }
        }
    }

    (param_types, local_types, declared_in_selection)
}

fn collect_ident_tokens(
    node: &nova_syntax::SyntaxNode,
    known: &HashSet<String>,
) -> Vec<(String, TextRange)> {
    let mut out = Vec::new();
    for tok in node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
    {
        if !tok.kind().is_identifier_like() {
            continue;
        }
        let name = tok.text().to_string();
        if !known.contains(&name) {
            continue;
        }
        let range = tok.text_range();
        out.push((
            name,
            TextRange::new(
                u32::from(range.start()) as usize,
                u32::from(range.end()) as usize,
            ),
        ));
    }
    out.sort_by(|a, b| {
        a.1.start
            .cmp(&b.1.start)
            .then_with(|| a.1.end.cmp(&b.1.end))
    });
    out
}

fn collect_reads_writes_in_statement(
    stmt: &ast::Statement,
    known: &HashSet<String>,
) -> (Vec<(String, TextRange)>, HashSet<String>) {
    let mut writes: HashSet<String> = HashSet::new();
    let mut reads: Vec<(String, TextRange)> = Vec::new();

    match stmt {
        ast::Statement::ExpressionStatement(expr_stmt) => {
            let Some(expr) = expr_stmt.expression() else {
                return (reads, writes);
            };
            match expr {
                ast::Expression::AssignmentExpression(assign) => {
                    if let Some(lhs) = assign.lhs() {
                        if let ast::Expression::NameExpression(name_expr) = lhs {
                            if let Some(tok) = name_expr
                                .syntax()
                                .descendants_with_tokens()
                                .filter_map(|el| el.into_token())
                                .find(|t| t.kind().is_identifier_like())
                            {
                                let name = tok.text().to_string();
                                if known.contains(&name) {
                                    writes.insert(name);
                                }
                            }
                        }
                    }
                    if let Some(rhs) = assign.rhs() {
                        reads.extend(collect_ident_tokens(rhs.syntax(), known));
                    }
                }
                _ => reads.extend(collect_ident_tokens(expr.syntax(), known)),
            }
        }
        ast::Statement::ReturnStatement(ret) => {
            if let Some(expr) = ret.expression() {
                reads.extend(collect_ident_tokens(expr.syntax(), known));
            }
        }
        _ => reads.extend(collect_ident_tokens(stmt.syntax(), known)),
    }

    // Stable order + dedup by first occurrence.
    reads.sort_by(|a, b| {
        a.1.start
            .cmp(&b.1.start)
            .then_with(|| a.1.end.cmp(&b.1.end))
    });
    let mut seen = HashSet::new();
    reads.retain(|(name, _)| seen.insert(name.clone()));

    (reads, writes)
}

fn collect_reads_after_offset(
    body: &ast::Block,
    offset: usize,
    known: &HashSet<String>,
) -> HashSet<String> {
    let mut out = HashSet::new();
    for expr in body
        .syntax()
        .descendants()
        .filter_map(ast::Expression::cast)
    {
        let range = syntax_range(expr.syntax());
        if range.start < offset {
            continue;
        }
        for (name, _) in collect_ident_tokens(expr.syntax(), known) {
            out.insert(name);
        }
    }
    out
}

fn insertion_offset_end_of_class(source: &str, class_decl: &ast::ClassDeclaration) -> usize {
    let Some(body) = class_decl.body() else {
        return syntax_range(class_decl.syntax()).end;
    };
    // Insert immediately before the newline that starts the closing brace line.
    let mut close = None;
    for tok in body
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
    {
        if tok.kind() == SyntaxKind::RBrace {
            close = Some(u32::from(tok.text_range().start()) as usize);
        }
    }
    let close = close.unwrap_or_else(|| syntax_range(body.syntax()).end);
    let line_start = line_start_offset(source, close);
    line_start.saturating_sub(1)
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

fn line_start_offset(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

fn indentation_at(source: &str, offset: usize) -> String {
    let start = line_start_offset(source, offset);
    source[start..offset]
        .chars()
        .take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
        .collect()
}

fn reindent(block: &str, old_indent: &str, new_indent: &str) -> String {
    let mut out = String::new();
    for line in block.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_prefix(old_indent).unwrap_or(line);
        if !line.trim().is_empty() {
            out.push_str(new_indent);
        }
        out.push_str(line);
        if has_newline {
            out.push('\n');
        }
    }
    if !block.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}
