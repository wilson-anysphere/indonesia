use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use nova_core::Name;
use nova_flow::build_cfg_with;
use nova_hir::body::{Body, ExprId, ExprKind, LocalId, LocalKind, StmtId, StmtKind};
use nova_hir::body_lowering::lower_flow_body_with;
use nova_syntax::ast::{self, AstNode};
use nova_syntax::{parse_java, SyntaxKind};
use nova_types::Span;

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
    Yield,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractMethodIssue {
    InvalidSelection,
    InvalidMethodName { name: String },
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
        if !is_valid_java_identifier(&self.name) {
            issues.push(ExtractMethodIssue::InvalidMethodName {
                name: self.name.clone(),
            });
        }
        if let Some(class_decl) = class_decl.as_ref() {
            if issues.is_empty() && class_has_method_named(class_decl, &self.name) {
                issues.push(ExtractMethodIssue::NameCollision {
                    name: self.name.clone(),
                });
            }
        }

        let Some(selection_info) = find_statement_selection(&method_body, selection) else {
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
        collect_control_flow_hazards(&selection_info.statements, selection, &mut hazards, &mut issues);

        let type_map = collect_declared_types(source, &method, &method_body);

        let flow_params = collect_method_param_spans(&method);
        let flow_body = lower_flow_body_with(&method_body, flow_params, &mut || {});

        let (reads_in_selection, writes_in_selection) =
            collect_reads_writes_in_flow_selection(&flow_body, selection);

        let live_after_selection = live_locals_after_selection(&flow_body, selection);

        let return_value = compute_return_value(
            &flow_body,
            &type_map,
            selection,
            &writes_in_selection,
            &live_after_selection,
            &mut issues,
        );

        // Determine parameters in order of first appearance in the selection.
        let mut parameters = Vec::new();
        for local in reads_in_selection {
            if local_declared_in_selection(&flow_body, local, selection) {
                continue;
            }
            let name = flow_body.locals()[local.index()].name.as_str().to_string();
            let ty = type_for_local(&flow_body, &type_map, local, &mut issues);
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
            .ok_or("selection must be inside a method, constructor, or initializer block")?;
        let enclosing_method_is_static = method.is_static();
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
        let signature = match (vis_kw.is_empty(), enclosing_method_is_static) {
            (true, false) => format!(
                "{method_indent}{return_ty} {}({params_sig}) {{\n",
                self.name
            ),
            (true, true) => format!(
                "{method_indent}static {return_ty} {}({params_sig}) {{\n",
                self.name
            ),
            (false, false) => format!(
                "{method_indent}{vis_kw} {return_ty} {}({params_sig}) {{\n",
                self.name
            ),
            (false, true) => format!(
                "{method_indent}{vis_kw} static {return_ty} {}({params_sig}) {{\n",
                self.name
            ),
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

fn method_is_static(method: &ast::MethodDeclaration) -> bool {
    method.modifiers().is_some_and(|modifiers| {
        modifiers
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|tok| tok.kind() == SyntaxKind::StaticKw)
    })
}

fn initializer_is_static(init: &ast::InitializerBlock) -> bool {
    init.modifiers().is_some_and(|modifiers| {
        modifiers
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|tok| tok.kind() == SyntaxKind::StaticKw)
    })
}

fn syntax_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EnclosingMethod {
    Method(ast::MethodDeclaration),
    Constructor(ast::ConstructorDeclaration),
    Initializer(ast::InitializerBlock),
}

impl EnclosingMethod {
    fn syntax(&self) -> &nova_syntax::SyntaxNode {
        match self {
            EnclosingMethod::Method(method) => method.syntax(),
            EnclosingMethod::Constructor(ctor) => ctor.syntax(),
            EnclosingMethod::Initializer(init) => init.syntax(),
        }
    }

    fn parameter_list(&self) -> Option<ast::ParameterList> {
        match self {
            EnclosingMethod::Method(method) => method.parameter_list(),
            EnclosingMethod::Constructor(ctor) => ctor.parameter_list(),
            EnclosingMethod::Initializer(_) => None,
        }
    }

    fn is_static(&self) -> bool {
        match self {
            EnclosingMethod::Method(method) => method_is_static(method),
            EnclosingMethod::Constructor(_) => false,
            EnclosingMethod::Initializer(init) => initializer_is_static(init),
        }
    }
}

fn slice_syntax<'a>(source: &'a str, node: &nova_syntax::SyntaxNode) -> Option<&'a str> {
    let range = syntax_range(node);
    source.get(range.start..range.end)
}

fn non_trivia_range(node: &nova_syntax::SyntaxNode) -> Option<TextRange> {
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    for tok in node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| !tok.kind().is_trivia())
    {
        let tok_range = tok.text_range();
        if start.is_none() {
            start = Some(u32::from(tok_range.start()) as usize);
        }
        end = Some(u32::from(tok_range.end()) as usize);
    }
    Some(TextRange::new(start?, end?))
}

fn span_of_token(token: &nova_syntax::SyntaxToken) -> Span {
    let range = token.text_range();
    Span::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn span_within_range(span: Span, range: TextRange) -> bool {
    range.start <= span.start && span.end <= range.end
}

fn span_intersects_range(span: Span, range: TextRange) -> bool {
    span.start < range.end && range.start < span.end
}

fn find_enclosing_method(
    root: nova_syntax::SyntaxNode,
    selection: TextRange,
) -> Option<(EnclosingMethod, ast::Block)> {
    let mut best: Option<(usize, EnclosingMethod, ast::Block)> = None;

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
                best = Some((span, EnclosingMethod::Method(method), body));
            }
        }
    }

    for ctor in root
        .descendants()
        .filter_map(ast::ConstructorDeclaration::cast)
    {
        let Some(body) = ctor.body() else {
            continue;
        };
        let body_range = syntax_range(body.syntax());
        if body_range.start <= selection.start && selection.end <= body_range.end {
            let span = body_range.len();
            if best
                .as_ref()
                .is_none_or(|(best_span, _, _)| span < *best_span)
            {
                best = Some((span, EnclosingMethod::Constructor(ctor), body));
            }
        }
    }

    for init in root.descendants().filter_map(ast::InitializerBlock::cast) {
        let Some(body) = init.body() else {
            continue;
        };
        let body_range = syntax_range(body.syntax());
        if body_range.start <= selection.start && selection.end <= body_range.end {
            let span = body_range.len();
            if best
                .as_ref()
                .is_none_or(|(best_span, _, _)| span < *best_span)
            {
                best = Some((span, EnclosingMethod::Initializer(init), body));
            }
        }
    }

    best.map(|(_, m, b)| (m, b))
}

#[derive(Debug, Clone)]
struct StatementSelection {
    #[allow(dead_code)]
    block: ast::Block,
    statements: Vec<ast::Statement>,
}

/// Resolve a trimmed selection to a contiguous sequence of complete statements.
///
/// Finds the *innermost* [`ast::Block`] whose direct child statements contain a
/// slice `[i..=j]` such that:
/// - `selection.start == start(stmts[i])`
/// - `selection.end == end(stmts[j])`
/// - all statements between `i` and `j` are fully covered (contiguous).
fn find_statement_selection(method_body: &ast::Block, selection: TextRange) -> Option<StatementSelection> {
    let mut best: Option<(usize, StatementSelection)> = None;
    let blocks = std::iter::once(method_body.clone())
        .chain(method_body.syntax().descendants().filter_map(ast::Block::cast));

    for block in blocks {
        let stmts: Vec<_> = block.statements().collect();
        if stmts.is_empty() {
            continue;
        }

        let start_idx = stmts.iter().position(|stmt| {
            non_trivia_range(stmt.syntax())
                .is_some_and(|range| range.start == selection.start)
        });
        let end_idx = stmts.iter().position(|stmt| {
            non_trivia_range(stmt.syntax()).is_some_and(|range| range.end == selection.end)
        });
        let (Some(start_idx), Some(end_idx)) = (start_idx, end_idx) else {
            continue;
        };
        if start_idx > end_idx {
            continue;
        }

        let span = syntax_range(block.syntax()).len();
        let sel = StatementSelection {
            block: block.clone(),
            statements: stmts[start_idx..=end_idx].to_vec(),
        };
        if best
            .as_ref()
            .is_none_or(|(best_span, _)| span < *best_span)
        {
            best = Some((span, sel));
        }
    }

    best.map(|(_, sel)| sel)
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

fn collect_control_flow_hazards(
    selection_statements: &[ast::Statement],
    selection: TextRange,
    hazards: &mut Vec<ControlFlowHazard>,
    issues: &mut Vec<ExtractMethodIssue>,
) {
    for stmt in selection_statements {
        let stmts = std::iter::once(stmt.clone())
            .chain(stmt.syntax().descendants().filter_map(ast::Statement::cast));
        for nested in stmts {
            match nested {
                ast::Statement::ReturnStatement(_) => {
                    push_hazard(hazards, ControlFlowHazard::Return);
                    issues.push(ExtractMethodIssue::IllegalControlFlow {
                        hazard: ControlFlowHazard::Return,
                    });
                }
                ast::Statement::YieldStatement(_) => {
                    push_hazard(hazards, ControlFlowHazard::Yield);
                    issues.push(ExtractMethodIssue::IllegalControlFlow {
                        hazard: ControlFlowHazard::Yield,
                    });
                }
                ast::Statement::ThrowStatement(_) => {
                    // Allowed (best-effort): would be modeled as `throws` in the future.
                    push_hazard(hazards, ControlFlowHazard::Throw);
                }
                ast::Statement::BreakStatement(brk) => {
                    push_hazard(hazards, ControlFlowHazard::Break);

                    if brk.label_token().is_some() {
                        issues.push(ExtractMethodIssue::IllegalControlFlow {
                            hazard: ControlFlowHazard::Break,
                        });
                        continue;
                    }

                    let Some(target) = nearest_break_target(brk.syntax()) else {
                        issues.push(ExtractMethodIssue::IllegalControlFlow {
                            hazard: ControlFlowHazard::Break,
                        });
                        continue;
                    };
                    let target_range = syntax_range(target.syntax());
                    if !(selection.start <= target_range.start && target_range.end <= selection.end) {
                        issues.push(ExtractMethodIssue::IllegalControlFlow {
                            hazard: ControlFlowHazard::Break,
                        });
                    }
                }
                ast::Statement::ContinueStatement(cont) => {
                    push_hazard(hazards, ControlFlowHazard::Continue);

                    if cont.label_token().is_some() {
                        issues.push(ExtractMethodIssue::IllegalControlFlow {
                            hazard: ControlFlowHazard::Continue,
                        });
                        continue;
                    }

                    let Some(target) = nearest_continue_target(cont.syntax()) else {
                        issues.push(ExtractMethodIssue::IllegalControlFlow {
                            hazard: ControlFlowHazard::Continue,
                        });
                        continue;
                    };
                    let target_range = syntax_range(target.syntax());
                    if !(selection.start <= target_range.start && target_range.end <= selection.end) {
                        issues.push(ExtractMethodIssue::IllegalControlFlow {
                            hazard: ControlFlowHazard::Continue,
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

fn push_hazard(hazards: &mut Vec<ControlFlowHazard>, hazard: ControlFlowHazard) {
    if !hazards.contains(&hazard) {
        hazards.push(hazard);
    }
}

fn nearest_break_target(from: &nova_syntax::SyntaxNode) -> Option<ast::Statement> {
    from.ancestors().find_map(|node| {
        let stmt = ast::Statement::cast(node)?;
        match stmt {
            ast::Statement::WhileStatement(_)
            | ast::Statement::DoWhileStatement(_)
            | ast::Statement::ForStatement(_)
            | ast::Statement::SwitchStatement(_) => Some(stmt),
            _ => None,
        }
    })
}

fn nearest_continue_target(from: &nova_syntax::SyntaxNode) -> Option<ast::Statement> {
    from.ancestors().find_map(|node| {
        let stmt = ast::Statement::cast(node)?;
        match stmt {
            ast::Statement::WhileStatement(_)
            | ast::Statement::DoWhileStatement(_)
            | ast::Statement::ForStatement(_) => Some(stmt),
            _ => None,
        }
    })
}

fn collect_method_param_spans(method: &EnclosingMethod) -> Vec<(Name, Span)> {
    let mut out = Vec::new();
    if let Some(params) = method.parameter_list() {
        for param in params.parameters() {
            let Some(name_tok) = param.name_token() else {
                continue;
            };
            out.push((
                Name::new(name_tok.text().to_string()),
                span_of_token(&name_tok),
            ));
        }
    }
    out
}

/// Best-effort mapping from a local/param *name token* span to its declared type text.
///
/// This is used to recover type strings for extracted method parameters/return values. Using spans
/// (rather than just names) lets us handle shadowing more correctly.
fn collect_declared_types(
    source: &str,
    method: &EnclosingMethod,
    method_body: &ast::Block,
) -> HashMap<Span, String> {
    let mut out = HashMap::new();

    if let Some(params) = method.parameter_list() {
        for param in params.parameters() {
            let (Some(name_tok), Some(ty)) = (param.name_token(), param.ty()) else {
                continue;
            };
            let ty_text = slice_syntax(source, ty.syntax()).unwrap_or("Object").trim().to_string();
            out.insert(span_of_token(&name_tok), ty_text);
        }
    }

    for stmt in method_body
        .syntax()
        .descendants()
        .filter_map(ast::LocalVariableDeclarationStatement::cast)
    {
        let Some(ty) = stmt.ty() else {
            continue;
        };
        let ty_text = slice_syntax(source, ty.syntax()).unwrap_or("Object").trim().to_string();
        let Some(list) = stmt.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            let Some(name_tok) = decl.name_token() else {
                continue;
            };
            out.insert(span_of_token(&name_tok), ty_text.clone());
        }
    }

    out
}

fn collect_reads_writes_in_flow_selection(
    body: &Body,
    selection: TextRange,
) -> (Vec<LocalId>, HashSet<LocalId>) {
    let mut reads: Vec<(LocalId, Span)> = Vec::new();
    let mut writes: HashSet<LocalId> = HashSet::new();

    collect_reads_writes_in_stmt(body, body.root(), selection, &mut reads, &mut writes);

    // Dedup reads by local id, in first-use order (expression span start).
    reads.sort_by(|a, b| {
        a.1.start
            .cmp(&b.1.start)
            .then_with(|| a.1.end.cmp(&b.1.end))
    });
    let mut seen: HashSet<LocalId> = HashSet::new();
    let reads = reads
        .into_iter()
        .filter(|(local, _)| seen.insert(*local))
        .map(|(local, _)| local)
        .collect();

    (reads, writes)
}

fn collect_reads_writes_in_stmt(
    body: &Body,
    stmt_id: StmtId,
    selection: TextRange,
    reads: &mut Vec<(LocalId, Span)>,
    writes: &mut HashSet<LocalId>,
) {
    let stmt = body.stmt(stmt_id);
    if !span_intersects_range(stmt.span, selection) {
        return;
    }
    let contained = span_within_range(stmt.span, selection);

    match &stmt.kind {
        StmtKind::Block(stmts) => {
            for child in stmts {
                collect_reads_writes_in_stmt(body, *child, selection, reads, writes);
            }
        }
        StmtKind::Let { local, initializer } => {
            if contained {
                writes.insert(*local);
                if let Some(init) = initializer {
                    collect_reads_in_expr(body, *init, selection, reads);
                }
            }
        }
        StmtKind::Assign { target, value } => {
            if contained {
                writes.insert(*target);
                collect_reads_in_expr(body, *value, selection, reads);
            }
        }
        StmtKind::Expr(expr) => {
            if contained {
                collect_reads_in_expr(body, *expr, selection, reads);
            }
        }
        StmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            if contained {
                collect_reads_in_expr(body, *condition, selection, reads);
            }
            collect_reads_writes_in_stmt(body, *then_branch, selection, reads, writes);
            if let Some(else_branch) = else_branch {
                collect_reads_writes_in_stmt(body, *else_branch, selection, reads, writes);
            }
        }
        StmtKind::While { condition, body: inner } => {
            if contained {
                collect_reads_in_expr(body, *condition, selection, reads);
            }
            collect_reads_writes_in_stmt(body, *inner, selection, reads, writes);
        }
        StmtKind::DoWhile { body: inner, condition } => {
            collect_reads_writes_in_stmt(body, *inner, selection, reads, writes);
            if contained {
                collect_reads_in_expr(body, *condition, selection, reads);
            }
        }
        StmtKind::For {
            init,
            condition,
            update,
            body: inner,
        } => {
            if let Some(init) = init {
                collect_reads_writes_in_stmt(body, *init, selection, reads, writes);
            }
            if contained {
                if let Some(cond) = condition {
                    collect_reads_in_expr(body, *cond, selection, reads);
                }
            }
            if let Some(update) = update {
                collect_reads_writes_in_stmt(body, *update, selection, reads, writes);
            }
            collect_reads_writes_in_stmt(body, *inner, selection, reads, writes);
        }
        StmtKind::Switch { expression, arms } => {
            if contained {
                collect_reads_in_expr(body, *expression, selection, reads);
                // Best-effort: include locals referenced in case labels.
                for arm in arms {
                    for value in &arm.values {
                        collect_reads_in_expr(body, *value, selection, reads);
                    }
                }
            }
            for arm in arms {
                collect_reads_writes_in_stmt(body, arm.body, selection, reads, writes);
            }
        }
        StmtKind::Try { body: inner, catches, finally } => {
            collect_reads_writes_in_stmt(body, *inner, selection, reads, writes);
            for catch in catches {
                collect_reads_writes_in_stmt(body, *catch, selection, reads, writes);
            }
            if let Some(finally) = finally {
                collect_reads_writes_in_stmt(body, *finally, selection, reads, writes);
            }
        }
        StmtKind::Return(expr) => {
            if contained {
                if let Some(expr) = expr {
                    collect_reads_in_expr(body, *expr, selection, reads);
                }
            }
        }
        StmtKind::Throw(expr) => {
            if contained {
                collect_reads_in_expr(body, *expr, selection, reads);
            }
        }
        StmtKind::Break | StmtKind::Continue | StmtKind::Nop => {}
    }
}

fn collect_reads_in_expr(body: &Body, expr_id: ExprId, selection: TextRange, reads: &mut Vec<(LocalId, Span)>) {
    let expr = body.expr(expr_id);
    if !span_within_range(expr.span, selection) {
        return;
    }

    match &expr.kind {
        ExprKind::Local(local) => reads.push((*local, expr.span)),
        ExprKind::Null | ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::String(_) => {}
        ExprKind::New { args, .. } => {
            for arg in args {
                collect_reads_in_expr(body, *arg, selection, reads);
            }
        }
        ExprKind::Unary { expr, .. } => collect_reads_in_expr(body, *expr, selection, reads),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_reads_in_expr(body, *lhs, selection, reads);
            collect_reads_in_expr(body, *rhs, selection, reads);
        }
        ExprKind::FieldAccess { receiver, .. } => collect_reads_in_expr(body, *receiver, selection, reads),
        ExprKind::Call { receiver, args, .. } => {
            if let Some(recv) = receiver {
                collect_reads_in_expr(body, *recv, selection, reads);
            }
            for arg in args {
                collect_reads_in_expr(body, *arg, selection, reads);
            }
        }
        ExprKind::Invalid { children } => {
            for child in children {
                collect_reads_in_expr(body, *child, selection, reads);
            }
        }
    }
}

fn local_declared_in_selection(body: &Body, local: LocalId, selection: TextRange) -> bool {
    let local_data = &body.locals()[local.index()];
    local_data.kind == LocalKind::Local
        && selection.start <= local_data.span.start
        && local_data.span.end <= selection.end
}

fn type_for_local(
    body: &Body,
    types: &HashMap<Span, String>,
    local: LocalId,
    issues: &mut Vec<ExtractMethodIssue>,
) -> String {
    let local_data = &body.locals()[local.index()];
    types
        .get(&local_data.span)
        .cloned()
        .unwrap_or_else(|| {
            let name = local_data.name.as_str().to_string();
            issues.push(ExtractMethodIssue::UnknownType { name });
            "Object".to_string()
        })
}

fn compute_return_value(
    body: &Body,
    types: &HashMap<Span, String>,
    selection: TextRange,
    writes_in_selection: &HashSet<LocalId>,
    live_after_selection: &HashSet<LocalId>,
    issues: &mut Vec<ExtractMethodIssue>,
) -> Option<ReturnValue> {
    let mut candidates: Vec<LocalId> = writes_in_selection
        .iter()
        .copied()
        .filter(|local| live_after_selection.contains(local))
        .collect();

    // Keep behavior deterministic.
    candidates.sort_by(|a, b| {
        let a_name = body.locals()[a.index()].name.as_str();
        let b_name = body.locals()[b.index()].name.as_str();
        a_name.cmp(b_name).then_with(|| a.index().cmp(&b.index()))
    });

    match candidates.as_slice() {
        [] => None,
        [local] => {
            let name = body.locals()[local.index()].name.as_str().to_string();
            let ty = type_for_local(body, types, *local, issues);
            Some(ReturnValue {
                name,
                ty,
                declared_in_selection: local_declared_in_selection(body, *local, selection),
            })
        }
        many => {
            let mut names: Vec<String> = many
                .iter()
                .map(|local| body.locals()[local.index()].name.as_str().to_string())
                .collect();
            names.sort();
            names.dedup();
            issues.push(ExtractMethodIssue::MultipleReturnValues { names });
            None
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StmtLocation {
    InBlock { block: nova_flow::BlockId, index: usize },
    Terminator { block: nova_flow::BlockId },
}

fn live_locals_after_selection(body: &Body, selection: TextRange) -> HashSet<LocalId> {
    let cfg = build_cfg_with(body, &mut || {});
    let (_live_in, live_out) = compute_cfg_liveness(body, &cfg);
    let stmt_locations = collect_stmt_locations(&cfg);

    let Some(last_stmt) = last_stmt_in_selection(body, selection, &stmt_locations) else {
        return HashSet::new();
    };
    let Some(location) = stmt_locations.get(&last_stmt).copied() else {
        return HashSet::new();
    };

    live_after_stmt(body, &cfg, &live_out, location)
        .unwrap_or_else(HashSet::new)
}

fn collect_stmt_locations(cfg: &nova_flow::ControlFlowGraph) -> HashMap<StmtId, StmtLocation> {
    let mut out = HashMap::new();
    for (idx, bb) in cfg.blocks.iter().enumerate() {
        let bb_id = nova_flow::BlockId(idx);
        for (pos, stmt) in bb.stmts.iter().enumerate() {
            out.entry(*stmt)
                .or_insert(StmtLocation::InBlock { block: bb_id, index: pos });
        }
        if let Some(from) = bb.terminator.from_stmt() {
            out.entry(from)
                .or_insert(StmtLocation::Terminator { block: bb_id });
        }
    }
    out
}

fn last_stmt_in_selection(
    body: &Body,
    selection: TextRange,
    locations: &HashMap<StmtId, StmtLocation>,
) -> Option<StmtId> {
    let mut best: Option<(usize, usize, usize, StmtId)> = None; // (end, start, stmt_idx, id)

    for stmt_id in locations.keys().copied() {
        let span = body.stmt(stmt_id).span;
        if !span_within_range(span, selection) {
            continue;
        }
        let key = (span.end, span.start, stmt_id.index());
        if best
            .as_ref()
            .is_none_or(|(end, start, idx, _)| key > (*end, *start, *idx))
        {
            best = Some((key.0, key.1, key.2, stmt_id));
        }
    }

    best.map(|(_, _, _, id)| id)
}

fn compute_cfg_liveness(
    body: &Body,
    cfg: &nova_flow::ControlFlowGraph,
) -> (Vec<HashSet<LocalId>>, Vec<HashSet<LocalId>>) {
    let n = cfg.blocks.len();
    let mut live_in: Vec<HashSet<LocalId>> = vec![HashSet::new(); n];
    let mut live_out: Vec<HashSet<LocalId>> = vec![HashSet::new(); n];

    loop {
        let mut changed = false;

        // Backward analysis (iterate blocks in reverse order for faster convergence).
        for idx in (0..n).rev() {
            let bb_id = nova_flow::BlockId(idx);

            // out[bb] = union(in[succ])
            let mut out = HashSet::new();
            for succ in cfg.successors(bb_id) {
                out.extend(live_in[succ.index()].iter().copied());
            }

            // in[bb] = transfer(bb, out)
            let mut live = out.clone();
            add_terminator_uses(body, &cfg.block(bb_id).terminator, &mut live);

            for stmt in cfg.block(bb_id).stmts.iter().rev() {
                transfer_stmt_liveness(body, *stmt, &mut live);
            }

            if live != live_in[idx] {
                live_in[idx] = live;
                changed = true;
            }
            if out != live_out[idx] {
                live_out[idx] = out;
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    (live_in, live_out)
}

fn live_after_stmt(
    body: &Body,
    cfg: &nova_flow::ControlFlowGraph,
    live_out: &[HashSet<LocalId>],
    location: StmtLocation,
) -> Option<HashSet<LocalId>> {
    match location {
        StmtLocation::InBlock { block, index } => {
            let bb = cfg.block(block);
            let mut live = live_out.get(block.index())?.clone();
            add_terminator_uses(body, &bb.terminator, &mut live);

            // Walk statements *after* the selected one backwards.
            for stmt in bb.stmts.iter().skip(index + 1).rev() {
                transfer_stmt_liveness(body, *stmt, &mut live);
            }

            Some(live)
        }
        StmtLocation::Terminator { block } => live_out.get(block.index()).cloned(),
    }
}

fn transfer_stmt_liveness(body: &Body, stmt: StmtId, live: &mut HashSet<LocalId>) {
    match &body.stmt(stmt).kind {
        StmtKind::Let { local, initializer } => {
            live.remove(local);
            if let Some(init) = initializer {
                add_expr_uses(body, *init, live);
            }
        }
        StmtKind::Assign { target, value } => {
            live.remove(target);
            add_expr_uses(body, *value, live);
        }
        StmtKind::Expr(expr) => {
            add_expr_uses(body, *expr, live);
        }
        StmtKind::Nop => {}
        // Control-flow statements do not appear in `BasicBlock.stmts`.
        other => {
            debug_assert!(
                matches!(
                    other,
                    StmtKind::Block(_)
                        | StmtKind::If { .. }
                        | StmtKind::While { .. }
                        | StmtKind::DoWhile { .. }
                        | StmtKind::For { .. }
                        | StmtKind::Switch { .. }
                        | StmtKind::Try { .. }
                        | StmtKind::Return(_)
                        | StmtKind::Throw(_)
                        | StmtKind::Break
                        | StmtKind::Continue
                ),
                "unexpected statement in basic block: {other:?}"
            );
        }
    }
}

fn add_terminator_uses(body: &Body, term: &nova_flow::Terminator, live: &mut HashSet<LocalId>) {
    match term {
        nova_flow::Terminator::If { condition, .. } => add_expr_uses(body, *condition, live),
        nova_flow::Terminator::Switch { expression, .. } => add_expr_uses(body, *expression, live),
        nova_flow::Terminator::Return { value, .. } => {
            if let Some(value) = value {
                add_expr_uses(body, *value, live);
            }
        }
        nova_flow::Terminator::Throw { exception, .. } => add_expr_uses(body, *exception, live),
        nova_flow::Terminator::Goto { .. }
        | nova_flow::Terminator::Multi { .. }
        | nova_flow::Terminator::Exit => {}
    }
}

fn add_expr_uses(body: &Body, expr: ExprId, live: &mut HashSet<LocalId>) {
    match &body.expr(expr).kind {
        ExprKind::Local(local) => {
            live.insert(*local);
        }
        ExprKind::Null | ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::String(_) => {}
        ExprKind::New { args, .. } => {
            for arg in args {
                add_expr_uses(body, *arg, live);
            }
        }
        ExprKind::Unary { expr, .. } => add_expr_uses(body, *expr, live),
        ExprKind::Binary { lhs, rhs, .. } => {
            add_expr_uses(body, *lhs, live);
            add_expr_uses(body, *rhs, live);
        }
        ExprKind::FieldAccess { receiver, .. } => add_expr_uses(body, *receiver, live),
        ExprKind::Call { receiver, args, .. } => {
            if let Some(recv) = receiver {
                add_expr_uses(body, *recv, live);
            }
            for arg in args {
                add_expr_uses(body, *arg, live);
            }
        }
        ExprKind::Invalid { children } => {
            for child in children {
                add_expr_uses(body, *child, live);
            }
        }
    }
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

fn is_valid_java_identifier(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let tokens = nova_syntax::lex(name);
    match tokens.as_slice() {
        [tok, eof] => {
            eof.kind == SyntaxKind::Eof && tok.kind.is_identifier_like() && !tok.kind.is_keyword()
        }
        _ => false,
    }
}
