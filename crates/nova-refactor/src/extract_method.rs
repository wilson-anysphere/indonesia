use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use nova_format::NewlineStyle;
use serde::{Deserialize, Serialize};

use nova_core::Name;
use nova_db::salsa::{Database as SalsaDatabase, NovaTypeck, Snapshot as SalsaSnapshot};
use nova_db::{FileId as DbFileId, ProjectId};
use nova_flow::build_cfg_with;
use nova_hir::body::{Body, ExprId, ExprKind, LocalId, LocalKind, StmtId, StmtKind};
use nova_hir::body_lowering::lower_flow_body_with;
use nova_syntax::ast::{self, AstNode};
use nova_syntax::{parse_java, SyntaxKind};
use nova_types::Span;

use crate::edit::{FileId, TextEdit as WorkspaceTextEdit, TextRange, WorkspaceEdit};

struct SingleFileTypecheck {
    file: DbFileId,
    snapshot: SalsaSnapshot,
}

fn typecheck_single_file(source: &str) -> SingleFileTypecheck {
    let db = SalsaDatabase::new();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, Arc::new(nova_jdk::JdkIndex::new()));
    db.set_classpath_index(project, None);

    let file = DbFileId::from_raw(0);
    db.set_file_text(file, source.to_string());
    db.set_file_rel_path(file, Arc::new("Main.java".to_string()));
    db.set_project_files(project, Arc::new(vec![file]));

    SingleFileTypecheck {
        file,
        snapshot: db.snapshot(),
    }
}

fn is_valid_signature_type_string(ty: &str) -> bool {
    let ty = ty.trim();
    if ty.is_empty() {
        return false;
    }
    // `var` is illegal in (non-lambda) parameter and return types.
    if ty == "var" {
        return false;
    }
    // These are Nova-only placeholders and not valid Java source types.
    if matches!(ty, "<?>" | "<error>" | "null") {
        return false;
    }
    // `nova-types` renders unknown class/typevar ids as `<class#...>` / `<tv#...>`.
    if ty.starts_with('<') {
        return false;
    }
    if ty == "void" {
        return false;
    }
    true
}

fn infer_type_at_offsets(
    typeck: &mut Option<SingleFileTypecheck>,
    source: &str,
    offsets: impl IntoIterator<Item = usize>,
) -> Option<String> {
    for offset in offsets {
        let typeck = typeck.get_or_insert_with(|| typecheck_single_file(source));
        let Some(ty) = typeck
            .snapshot
            .type_at_offset_display(typeck.file, offset as u32)
        else {
            continue;
        };
        if is_valid_signature_type_string(&ty) {
            return Some(ty);
        }
    }
    None
}

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
    /// Insert the extracted method immediately after the enclosing member (method/constructor/initializer).
    AfterCurrentMethod,
    /// Insert the extracted method at the end of the enclosing type.
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
    InvalidMethodName {
        name: String,
    },
    InvalidVisibilityForInterface {
        visibility: Visibility,
    },
    NameCollision {
        name: String,
    },
    MultipleReturnValues {
        names: Vec<String>,
    },
    IllegalControlFlow {
        hazard: ControlFlowHazard,
    },
    /// The selection references a local type (a class/interface/enum/record/@interface declared
    /// inside the enclosing method/constructor/initializer). Local types are only in scope within
    /// that body/block, but Extract Method inserts the extracted method at the type level where
    /// the local type is out of scope.
    ReferencesLocalType {
        name: String,
    },
    UnknownType {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractMethodAnalysis {
    pub region: ExtractRegionKind,
    pub parameters: Vec<Parameter>,
    pub return_value: Option<ReturnValue>,
    /// Return type of the extracted method.
    pub return_ty: String,
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
                return_ty: "void".to_string(),
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
                return_ty: "void".to_string(),
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues: vec![ExtractMethodIssue::InvalidSelection],
            });
        }

        let root = parsed.syntax();
        let Some((method, method_body, _type_params_text)) =
            find_enclosing_method(source, root.clone(), selection)
        else {
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Statements,
                parameters: Vec::new(),
                return_value: None,
                return_ty: "void".to_string(),
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues: vec![ExtractMethodIssue::InvalidSelection],
            });
        };

        let enclosing_type_body = find_enclosing_type_body(method.syntax());
        let local_type_names = collect_local_type_names(&method_body);

        let mut issues = Vec::new();
        if !is_valid_java_identifier(&self.name) {
            issues.push(ExtractMethodIssue::InvalidMethodName {
                name: self.name.clone(),
            });
        }
        let interface_like = matches!(
            enclosing_type_body.as_ref(),
            Some(EnclosingTypeBody::Interface(_) | EnclosingTypeBody::Annotation(_))
        );
        if interface_like {
            match self.visibility {
                Visibility::Protected | Visibility::PackagePrivate => {
                    issues.push(ExtractMethodIssue::InvalidVisibilityForInterface {
                        visibility: self.visibility,
                    });
                }
                Visibility::Private | Visibility::Public => {}
            }
        }
        if let Some(enclosing_type_body) = enclosing_type_body.as_ref() {
            if issues.is_empty() && type_body_has_method_named(enclosing_type_body, &self.name) {
                issues.push(ExtractMethodIssue::NameCollision {
                    name: self.name.clone(),
                });
            }
        } else {
            issues.push(ExtractMethodIssue::InvalidSelection);
        }

        if let Some(selection_info) = find_statement_selection(&method_body, selection) {
            // Until lambdas are modeled in flow IR, selections inside lambda bodies are invalid.
            //
            // `nova_hir::body_lowering` intentionally skips lowering lambda bodies (they're lazily
            // executed). If we allowed extraction from within a lambda body, we could end up analyzing
            // the enclosing method while ignoring the selected code.
            let body_range = syntax_range(method_body.syntax());
            let selection_inside_lambda = selection_info.statements.iter().any(|stmt| {
                let Some(lambda) = stmt
                    .syntax()
                    .ancestors()
                    .find_map(ast::LambdaExpression::cast)
                else {
                    return false;
                };

                let lambda_range = syntax_range(lambda.syntax());
                body_range.start <= lambda_range.start && lambda_range.end <= body_range.end
            });
            if selection_inside_lambda {
                issues.push(ExtractMethodIssue::InvalidSelection);
                return Ok(ExtractMethodAnalysis {
                    region: ExtractRegionKind::Statements,
                    parameters: Vec::new(),
                    return_value: None,
                    return_ty: "void".to_string(),
                    thrown_exceptions: Vec::new(),
                    hazards: Vec::new(),
                    issues,
                });
            }

            if let Some(name) = find_referenced_local_type_name_in_range(
                method_body.syntax(),
                selection,
                &local_type_names,
            ) {
                issues.push(ExtractMethodIssue::ReferencesLocalType { name });
            }

            let mut hazards = Vec::new();
            collect_control_flow_hazards(
                &selection_info.statements,
                selection,
                &mut hazards,
                &mut issues,
            );

            let declared_types = collect_declared_types(source, &method, &method_body);
            let declared_types_by_name =
                collect_declared_types_by_name(source, &method, &method_body);
            let thrown_exceptions = collect_thrown_exceptions_in_statements(
                source,
                &selection_info.statements,
                &declared_types_by_name,
            );

            let flow_params = collect_method_param_spans(&method);
            let flow_body = lower_flow_body_with(&method_body, flow_params, &mut || {});

            let mut typeck: Option<SingleFileTypecheck> = None;
            // Flow IR statement spans include trivia in some cases (notably `if` statements),
            // while user selections are typically trimmed to non-trivia tokens. When analyzing
            // locals/CFG properties, expand the selection to cover the full syntax range of the
            // selected statements so flow spans are treated as "contained".
            let flow_selection = selection_info
                .statements
                .first()
                .and_then(|first| {
                    selection_info.statements.last().map(|last| {
                        TextRange::new(
                            syntax_range(first.syntax()).start,
                            syntax_range(last.syntax()).end,
                        )
                    })
                })
                .unwrap_or(selection);

            let (reads_in_selection, writes_in_selection) =
                collect_reads_writes_in_flow_selection(&flow_body, flow_selection);

            let live_after_selection = live_locals_after_selection(&flow_body, flow_selection);

            let return_candidate = compute_return_value(
                &mut typeck,
                source,
                &flow_body,
                &declared_types,
                flow_selection,
                &writes_in_selection,
                &live_after_selection,
                &mut issues,
            );

            // Determine parameters in order of first appearance in the selection.
            let mut parameters = Vec::new();
            for (local, first_use) in reads_in_selection {
                if local_declared_in_selection(&flow_body, local, flow_selection) {
                    continue;
                }
                let name = flow_body.locals()[local.index()].name.as_str().to_string();
                let ty = type_for_local(
                    &mut typeck,
                    source,
                    &flow_body,
                    &declared_types,
                    local,
                    Some(first_use.start),
                    &mut issues,
                );
                parameters.push(Parameter { name, ty });
            }

            // If a return candidate is not declared inside the selection but is definitely
            // assigned at the start of the selection, thread its pre-selection value through the
            // extracted method as the first parameter.
            if let Some((ret_local, ret)) = return_candidate.as_ref() {
                if !ret.declared_in_selection {
                    if definitely_assigned_at_selection_start(
                        &flow_body,
                        flow_selection,
                        *ret_local,
                    ) == Some(true)
                    {
                        if let Some(pos) = parameters.iter().position(|p| p.name == ret.name) {
                            let param = parameters.remove(pos);
                            parameters.insert(0, param);
                        } else {
                            parameters.insert(
                                0,
                                Parameter {
                                    name: ret.name.clone(),
                                    ty: ret.ty.clone(),
                                },
                            );
                        }
                    }
                }
            }

            let return_value = return_candidate.as_ref().map(|(_, ret)| ret.clone());

            let return_ty = return_value
                .as_ref()
                .map(|r| r.ty.clone())
                .unwrap_or_else(|| "void".to_string());

            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Statements,
                parameters,
                return_value,
                return_ty,
                thrown_exceptions,
                hazards,
                issues,
            });
        }

        // Fall back to extracting an expression when the selection isn't a statement range.
        let Some(selected_expr) = find_expression_exact(&method_body, selection) else {
            issues.push(ExtractMethodIssue::InvalidSelection);
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Statements,
                parameters: Vec::new(),
                return_value: None,
                return_ty: "void".to_string(),
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues,
            });
        };

        // Until lambdas are modeled in flow IR, selections inside lambda bodies are invalid.
        let body_range = syntax_range(method_body.syntax());
        let selection_inside_lambda = selected_expr
            .syntax()
            .ancestors()
            .find_map(ast::LambdaExpression::cast)
            .is_some_and(|lambda| {
                let lambda_range = syntax_range(lambda.syntax());
                body_range.start <= lambda_range.start && lambda_range.end <= body_range.end
            });
        if selection_inside_lambda {
            issues.push(ExtractMethodIssue::InvalidSelection);
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Expression,
                parameters: Vec::new(),
                return_value: None,
                return_ty: "Object".to_string(),
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues,
            });
        }

        if let Some(name) = find_referenced_local_type_name_in_range(
            method_body.syntax(),
            selection,
            &local_type_names,
        ) {
            issues.push(ExtractMethodIssue::ReferencesLocalType { name });
        }

        if expression_has_local_mutation(&selected_expr) {
            issues.push(ExtractMethodIssue::InvalidSelection);
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Expression,
                parameters: Vec::new(),
                return_value: None,
                return_ty: "Object".to_string(),
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues,
            });
        }

        let declared_types = collect_declared_types(source, &method, &method_body);

        let flow_params = collect_method_param_spans(&method);
        let flow_body = lower_flow_body_with(&method_body, flow_params, &mut || {});

        let target_span = Span::new(selection.start, selection.end);
        let Some(flow_expr_id) = find_flow_expr_exact(&flow_body, target_span) else {
            issues.push(ExtractMethodIssue::InvalidSelection);
            return Ok(ExtractMethodAnalysis {
                region: ExtractRegionKind::Expression,
                parameters: Vec::new(),
                return_value: None,
                return_ty: "Object".to_string(),
                thrown_exceptions: Vec::new(),
                hazards: Vec::new(),
                issues,
            });
        };

        let mut typeck: Option<SingleFileTypecheck> = None;

        let mut parameters = Vec::new();
        for (local, first_use) in collect_reads_in_flow_expr(&flow_body, flow_expr_id, selection) {
            let local_data = &flow_body.locals()[local.index()];
            if span_within_range(local_data.span, selection) {
                continue;
            }
            let name = local_data.name.as_str().to_string();
            let ty = type_for_local(
                &mut typeck,
                source,
                &flow_body,
                &declared_types,
                local,
                Some(first_use.start),
                &mut issues,
            );
            parameters.push(Parameter { name, ty });
        }

        let mut return_ty = infer_expression_return_type(
            source,
            &selected_expr,
            &flow_body,
            Some(flow_expr_id),
            &declared_types.types,
        );
        if return_ty.as_deref().is_some_and(|ty| ty.trim() == "var") {
            let mut offsets = Vec::new();
            if let ExprKind::Local(local_id) = &flow_body.expr(flow_expr_id).kind {
                let local = &flow_body.locals()[local_id.index()];
                if let Some(offset) = declared_types.initializer_offsets.get(&local.span).copied() {
                    offsets.push(offset);
                }
            }
            offsets.push(selection.start);
            if let Some(inferred) = infer_type_at_offsets(&mut typeck, source, offsets) {
                return_ty = Some(inferred);
            }
        }
        if return_ty
            .as_deref()
            .is_some_and(|ty| !is_valid_signature_type_string(ty))
        {
            issues.push(ExtractMethodIssue::UnknownType {
                name: self.name.clone(),
            });
            return_ty = Some("Object".to_string());
        } else if return_ty.is_none() {
            issues.push(ExtractMethodIssue::UnknownType {
                name: self.name.clone(),
            });
            return_ty = Some("Object".to_string());
        }

        Ok(ExtractMethodAnalysis {
            region: ExtractRegionKind::Expression,
            parameters,
            return_value: None,
            return_ty: return_ty.unwrap_or_else(|| "Object".to_string()),
            thrown_exceptions: Vec::new(),
            hazards: Vec::new(),
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

        let newline = NewlineStyle::detect(source).as_str();

        let selection = trim_range(source, self.selection);
        let parsed = parse_java(source);
        if !parsed.errors.is_empty() {
            return Err("failed to parse source".to_string());
        }
        let root = parsed.syntax();

        let (method, method_body, type_params_text) =
            find_enclosing_method(source, root.clone(), selection)
                .ok_or("selection must be inside a method, constructor, or initializer block")?;
        let enclosing_method_is_static = method.is_static();
        let enclosing_type_body = find_enclosing_type_body(method.syntax())
            .ok_or("selection must be inside a type declaration")?;
        let interface_like = matches!(
            &enclosing_type_body,
            EnclosingTypeBody::Interface(_) | EnclosingTypeBody::Annotation(_)
        );
        let needs_default_modifier =
            interface_like && self.visibility == Visibility::Public && !enclosing_method_is_static;

        let method_indent = indentation_at(source, syntax_range(method.syntax()).start);
        let call_indent = indentation_at(source, selection.start);

        let insertion_offset = match self.insertion_strategy {
            InsertionStrategy::AfterCurrentMethod => syntax_range(method.syntax()).end,
            InsertionStrategy::EndOfClass => {
                insertion_offset_end_of_type_body(source, enclosing_type_body.syntax(), newline)
            }
        };

        let extracted_text = source
            .get(selection.start..selection.end)
            .ok_or("selection out of bounds")?
            .to_string();

        let new_body_indent = format!("{method_indent}    ");
        let (method_body_text, replacement, return_ty) = match analysis.region {
            ExtractRegionKind::Statements => {
                let extracted_body =
                    reindent(&extracted_text, &call_indent, &new_body_indent, newline);

                let mut method_body_text = extracted_body;
                if !method_body_text.ends_with(newline) {
                    method_body_text.push_str(newline);
                }

                if let Some(ret) = &analysis.return_value {
                    let declared_as_param = analysis.parameters.iter().any(|p| p.name == ret.name);
                    if !ret.declared_in_selection && !declared_as_param {
                        let decl = format!("{new_body_indent}{} {};{newline}", ret.ty, ret.name);
                        method_body_text = format!("{decl}{method_body_text}");
                    }
                    method_body_text
                        .push_str(&format!("{new_body_indent}return {};{newline}", ret.name));
                }

                let return_ty = analysis.return_ty.clone();

                let args = analysis
                    .parameters
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                let call_expr = format!("{}({})", self.name, args);

                let replacement = if let Some(ret) = &analysis.return_value {
                    if ret.declared_in_selection {
                        let is_final =
                            local_var_is_final_in_selection(&method_body, selection, &ret.name);
                        if is_final {
                            format!("final {} {} = {call_expr};", ret.ty, ret.name)
                        } else {
                            format!("{} {} = {call_expr};", ret.ty, ret.name)
                        }
                    } else {
                        format!("{} = {call_expr};", ret.name)
                    }
                } else {
                    format!("{call_expr};")
                };

                (method_body_text, replacement, return_ty)
            }
            ExtractRegionKind::Expression => {
                let mut expr_reindented =
                    reindent(&extracted_text, &call_indent, &new_body_indent, newline);
                expr_reindented = expr_reindented.trim_end().to_string();

                let expr_without_indent = expr_reindented
                    .strip_prefix(&new_body_indent)
                    .unwrap_or(expr_reindented.as_str());
                let method_body_text =
                    format!("{new_body_indent}return {expr_without_indent};{newline}");

                let args = analysis
                    .parameters
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                let call_expr = format!("{}({})", self.name, args);

                (method_body_text, call_expr, analysis.return_ty.clone())
            }
        };

        let params_sig = analysis
            .parameters
            .iter()
            .map(|p| format!("{} {}", p.ty, p.name))
            .collect::<Vec<_>>()
            .join(", ");

        let throws_clause = if analysis.thrown_exceptions.is_empty() {
            String::new()
        } else {
            format!(" throws {}", analysis.thrown_exceptions.join(", "))
        };

        let vis_kw = self.visibility.keyword();
        let mut modifiers = String::new();
        if !vis_kw.is_empty() {
            modifiers.push_str(vis_kw);
            modifiers.push(' ');
        }
        if needs_default_modifier {
            modifiers.push_str("default ");
        }
        if enclosing_method_is_static {
            modifiers.push_str("static ");
        }
        if let Some(type_params) = type_params_text.as_deref() {
            modifiers.push_str(type_params);
            modifiers.push(' ');
        }
        let signature = format!(
            "{method_indent}{modifiers}{return_ty} {}({params_sig}){throws_clause} {{{newline}",
            self.name
        );

        let mut new_method_text = String::new();
        new_method_text.push_str(newline);
        new_method_text.push_str(newline);
        new_method_text.push_str(&signature);
        new_method_text.push_str(&method_body_text);
        new_method_text.push_str(&method_indent);
        new_method_text.push('}');

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
    CompactConstructor(ast::CompactConstructorDeclaration),
    Initializer(ast::InitializerBlock),
}

impl EnclosingMethod {
    fn syntax(&self) -> &nova_syntax::SyntaxNode {
        match self {
            EnclosingMethod::Method(method) => method.syntax(),
            EnclosingMethod::Constructor(ctor) => ctor.syntax(),
            EnclosingMethod::CompactConstructor(ctor) => ctor.syntax(),
            EnclosingMethod::Initializer(init) => init.syntax(),
        }
    }

    fn parameter_list(&self) -> Option<ast::ParameterList> {
        match self {
            EnclosingMethod::Method(method) => method.parameter_list(),
            EnclosingMethod::Constructor(ctor) => ctor.parameter_list(),
            // Compact constructors have no explicit parameter list, but record components are in
            // scope as if they were constructor parameters.
            EnclosingMethod::CompactConstructor(ctor) => ctor
                .syntax()
                .ancestors()
                .find_map(ast::RecordDeclaration::cast)
                .and_then(|record| record.parameter_list()),
            EnclosingMethod::Initializer(_) => None,
        }
    }

    fn is_static(&self) -> bool {
        match self {
            EnclosingMethod::Method(method) => method_is_static(method),
            EnclosingMethod::Constructor(_) => false,
            EnclosingMethod::CompactConstructor(_) => false,
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
    source: &str,
    root: nova_syntax::SyntaxNode,
    selection: TextRange,
) -> Option<(EnclosingMethod, ast::Block, Option<String>)> {
    let mut best: Option<(usize, EnclosingMethod, ast::Block, Option<String>)> = None;

    for method in root.descendants().filter_map(ast::MethodDeclaration::cast) {
        let Some(body) = method.body() else {
            continue;
        };
        let type_params_text = method
            .type_parameters()
            .and_then(|tp| slice_syntax(source, tp.syntax()))
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let body_range = syntax_range(body.syntax());
        if body_range.start <= selection.start && selection.end <= body_range.end {
            let span = body_range.len();
            if best
                .as_ref()
                .is_none_or(|(best_span, _, _, _)| span < *best_span)
            {
                best = Some((
                    span,
                    EnclosingMethod::Method(method),
                    body,
                    type_params_text,
                ));
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
        let type_params_text = ctor
            .type_parameters()
            .and_then(|tp| slice_syntax(source, tp.syntax()))
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let body_range = syntax_range(body.syntax());
        if body_range.start <= selection.start && selection.end <= body_range.end {
            let span = body_range.len();
            if best
                .as_ref()
                .is_none_or(|(best_span, _, _, _)| span < *best_span)
            {
                best = Some((
                    span,
                    EnclosingMethod::Constructor(ctor),
                    body,
                    type_params_text,
                ));
            }
        }
    }

    for ctor in root
        .descendants()
        .filter_map(ast::CompactConstructorDeclaration::cast)
    {
        let Some(body) = ctor.body() else {
            continue;
        };
        let type_params_text = ctor
            .type_parameters()
            .and_then(|tp| slice_syntax(source, tp.syntax()))
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let body_range = syntax_range(body.syntax());
        if body_range.start <= selection.start && selection.end <= body_range.end {
            let span = body_range.len();
            if best
                .as_ref()
                .is_none_or(|(best_span, _, _, _)| span < *best_span)
            {
                best = Some((
                    span,
                    EnclosingMethod::CompactConstructor(ctor),
                    body,
                    type_params_text,
                ));
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
                .is_none_or(|(best_span, _, _, _)| span < *best_span)
            {
                best = Some((span, EnclosingMethod::Initializer(init), body, None));
            }
        }
    }

    best.map(|(_, m, b, t)| (m, b, t))
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
fn find_statement_selection(
    method_body: &ast::Block,
    selection: TextRange,
) -> Option<StatementSelection> {
    let mut best: Option<(usize, StatementSelection)> = None;
    let blocks = std::iter::once(method_body.clone()).chain(
        method_body
            .syntax()
            .descendants()
            .filter_map(ast::Block::cast),
    );

    for block in blocks {
        let stmts: Vec<_> = block.statements().collect();
        if stmts.is_empty() {
            continue;
        }

        let start_idx = stmts.iter().position(|stmt| {
            non_trivia_range(stmt.syntax()).is_some_and(|range| range.start == selection.start)
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
        if best.as_ref().is_none_or(|(best_span, _)| span < *best_span) {
            best = Some((span, sel));
        }
    }

    best.map(|(_, sel)| sel)
}

#[derive(Debug, Clone)]
enum EnclosingTypeBody {
    Class(ast::ClassBody),
    Interface(ast::InterfaceBody),
    Enum(ast::EnumBody),
    Record(ast::RecordBody),
    Annotation(ast::AnnotationBody),
}

impl EnclosingTypeBody {
    fn syntax(&self) -> &nova_syntax::SyntaxNode {
        match self {
            EnclosingTypeBody::Class(b) => b.syntax(),
            EnclosingTypeBody::Interface(b) => b.syntax(),
            EnclosingTypeBody::Enum(b) => b.syntax(),
            EnclosingTypeBody::Record(b) => b.syntax(),
            EnclosingTypeBody::Annotation(b) => b.syntax(),
        }
    }
}

fn find_enclosing_type_body(node: &nova_syntax::SyntaxNode) -> Option<EnclosingTypeBody> {
    node.ancestors().find_map(|ancestor| {
        if let Some(class_decl) = ast::ClassDeclaration::cast(ancestor.clone()) {
            return class_decl.body().map(EnclosingTypeBody::Class);
        }
        if let Some(intf_decl) = ast::InterfaceDeclaration::cast(ancestor.clone()) {
            return intf_decl.body().map(EnclosingTypeBody::Interface);
        }
        if let Some(enum_decl) = ast::EnumDeclaration::cast(ancestor.clone()) {
            return enum_decl.body().map(EnclosingTypeBody::Enum);
        }
        if let Some(record_decl) = ast::RecordDeclaration::cast(ancestor.clone()) {
            return record_decl.body().map(EnclosingTypeBody::Record);
        }
        if let Some(annot_decl) = ast::AnnotationTypeDeclaration::cast(ancestor.clone()) {
            return annot_decl.body().map(EnclosingTypeBody::Annotation);
        }
        None
    })
}

fn type_body_has_method_named(type_body: &EnclosingTypeBody, name: &str) -> bool {
    type_body
        .syntax()
        .children()
        .filter_map(ast::ClassMember::cast)
        .any(|member| {
            let ast::ClassMember::MethodDeclaration(method) = member else {
                return false;
            };
            method.name_token().is_some_and(|tok| tok.text() == name)
        })
}

fn collect_local_type_names(body: &ast::Block) -> HashSet<String> {
    let mut names = HashSet::new();
    for stmt in body
        .syntax()
        .descendants()
        .filter_map(ast::LocalTypeDeclarationStatement::cast)
    {
        let Some(decl) = stmt.declaration() else {
            continue;
        };
        let name_tok = match decl {
            ast::TypeDeclaration::ClassDeclaration(it) => it.name_token(),
            ast::TypeDeclaration::InterfaceDeclaration(it) => it.name_token(),
            ast::TypeDeclaration::EnumDeclaration(it) => it.name_token(),
            ast::TypeDeclaration::RecordDeclaration(it) => it.name_token(),
            ast::TypeDeclaration::AnnotationTypeDeclaration(it) => it.name_token(),
            ast::TypeDeclaration::EmptyDeclaration(_) => None,
            _ => None,
        };
        let Some(name_tok) = name_tok else {
            continue;
        };
        names.insert(name_tok.text().to_string());
    }
    names
}

fn find_referenced_local_type_name_in_range(
    enclosing_body: &nova_syntax::SyntaxNode,
    selection: TextRange,
    local_type_names: &HashSet<String>,
) -> Option<String> {
    if local_type_names.is_empty() {
        return None;
    }

    // Conservative heuristic: if any identifier-like token in the selection matches a local type
    // declared in the enclosing body, treat it as a reference.
    //
    // This may reject some otherwise-valid selections (e.g. a variable whose name happens to match
    // a local type), but prevents Extract Method from generating uncompilable code.
    let mut idents: Vec<(String, TextRange)> = Vec::new();
    for tok in enclosing_body
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
    {
        if !tok.kind().is_identifier_like() {
            continue;
        }
        let range = tok.text_range();
        let start = u32::from(range.start()) as usize;
        let end = u32::from(range.end()) as usize;
        if start < selection.start || end > selection.end {
            continue;
        }
        idents.push((tok.text().to_string(), TextRange::new(start, end)));
    }
    idents.sort_by(|a, b| {
        a.1.start
            .cmp(&b.1.start)
            .then_with(|| a.1.end.cmp(&b.1.end))
    });

    for (name, _) in idents {
        if local_type_names.contains(&name) {
            return Some(name);
        }
    }
    None
}

fn collect_control_flow_hazards(
    selection_statements: &[ast::Statement],
    selection: TextRange,
    hazards: &mut Vec<ControlFlowHazard>,
    issues: &mut Vec<ExtractMethodIssue>,
) {
    fn walk(
        node: &nova_syntax::SyntaxNode,
        selection: TextRange,
        hazards: &mut Vec<ControlFlowHazard>,
        issues: &mut Vec<ExtractMethodIssue>,
    ) {
        // Returns/break/continue/etc inside lambdas return from / target the lambda body, not the
        // enclosing method. Skip descending into lambda bodies so statements that *contain* a
        // lambda (e.g. `Runnable r = () -> { return; };`) don't get rejected.
        if ast::LambdaExpression::can_cast(node.kind()) {
            return;
        }

        if let Some(stmt) = ast::Statement::cast(node.clone()) {
            match stmt {
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
                    } else if let Some(target) = nearest_break_target(brk.syntax()) {
                        let target_range = syntax_range(target.syntax());
                        if !(selection.start <= target_range.start
                            && target_range.end <= selection.end)
                        {
                            issues.push(ExtractMethodIssue::IllegalControlFlow {
                                hazard: ControlFlowHazard::Break,
                            });
                        }
                    } else {
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
                    } else if let Some(target) = nearest_continue_target(cont.syntax()) {
                        let target_range = syntax_range(target.syntax());
                        if !(selection.start <= target_range.start
                            && target_range.end <= selection.end)
                        {
                            issues.push(ExtractMethodIssue::IllegalControlFlow {
                                hazard: ControlFlowHazard::Continue,
                            });
                        }
                    } else {
                        issues.push(ExtractMethodIssue::IllegalControlFlow {
                            hazard: ControlFlowHazard::Continue,
                        });
                    }
                }
                _ => {}
            }
        }

        for child in node.children() {
            walk(&child, selection, hazards, issues);
        }
    }

    for stmt in selection_statements {
        walk(stmt.syntax(), selection, hazards, issues);
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
struct DeclaredTypes {
    types: HashMap<Span, String>,
    initializer_offsets: HashMap<Span, usize>,
}

fn collect_declared_types(
    source: &str,
    method: &EnclosingMethod,
    method_body: &ast::Block,
) -> DeclaredTypes {
    let mut types = HashMap::new();
    let mut initializer_offsets = HashMap::new();

    if let Some(params) = method.parameter_list() {
        for param in params.parameters() {
            let (Some(name_tok), Some(ty)) = (param.name_token(), param.ty()) else {
                continue;
            };
            let ty_text = slice_syntax(source, ty.syntax())
                .unwrap_or("Object")
                .trim()
                .to_string();
            types.insert(span_of_token(&name_tok), ty_text);
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
        let ty_text = slice_syntax(source, ty.syntax())
            .unwrap_or("Object")
            .trim()
            .to_string();
        let Some(list) = stmt.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            let Some(name_tok) = decl.name_token() else {
                continue;
            };
            let name_span = span_of_token(&name_tok);
            types.insert(name_span, ty_text.clone());

            if let Some(initializer) = decl.initializer() {
                initializer_offsets.insert(name_span, syntax_range(initializer.syntax()).start);
            }
        }
    }

    // Try-with-resources variables are declared in `ResourceSpecification`, not in ordinary local
    // variable declaration statements.
    for try_stmt in method_body
        .syntax()
        .descendants()
        .filter_map(ast::TryStatement::cast)
    {
        let Some(resources) = try_stmt.resources() else {
            continue;
        };
        for resource in resources.resources() {
            // Local variable declaration form: `Type name = initializer`.
            let ty = resource.syntax().children().find_map(ast::Type::cast);
            let decl = resource
                .syntax()
                .children()
                .find_map(ast::VariableDeclarator::cast);
            let (Some(ty), Some(decl)) = (ty, decl) else {
                continue;
            };
            let Some(name_tok) = decl.name_token() else {
                continue;
            };
            let name_span = span_of_token(&name_tok);
            let ty_text = slice_syntax(source, ty.syntax())
                .unwrap_or("Object")
                .trim()
                .to_string();
            types.insert(name_span, ty_text);
            if let Some(initializer) = decl.initializer() {
                initializer_offsets.insert(name_span, syntax_range(initializer.syntax()).start);
            }
        }
    }

    // Catch clause parameters aren't represented as a `Parameter` node in the typed AST, but they
    // do introduce locals that we may need to type for extractions within the catch body.
    for catch in method_body
        .syntax()
        .descendants()
        .filter_map(ast::CatchClause::cast)
    {
        let Some(body) = catch.body() else {
            continue;
        };
        let body_start = syntax_range(body.syntax()).start;
        let header_tokens: Vec<_> = catch
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| u32::from(tok.text_range().end()) as usize <= body_start)
            .collect();

        let Some(name_tok) = header_tokens
            .iter()
            .rev()
            .find(|tok| tok.kind().is_identifier_like())
        else {
            continue;
        };
        let Some(lparen) = header_tokens
            .iter()
            .find(|tok| tok.kind() == SyntaxKind::LParen)
        else {
            continue;
        };

        let lparen_end = u32::from(lparen.text_range().end()) as usize;
        let name_start = u32::from(name_tok.text_range().start()) as usize;
        if lparen_end >= name_start || name_start > source.len() {
            continue;
        }
        let ty_text = source
            .get(lparen_end..name_start)
            .unwrap_or("Object")
            .trim()
            .to_string();
        types.insert(span_of_token(name_tok), ty_text);
    }

    // Catch parameters, enhanced-for variables, explicitly typed lambda parameters, etc. are all
    // represented as `Parameter` nodes but are not part of the enclosing method's `ParameterList`.
    // Collect them too so we can recover types for extractions inside nested blocks.
    for param in method_body
        .syntax()
        .descendants()
        .filter_map(ast::Parameter::cast)
    {
        let (Some(name_tok), Some(ty)) = (param.name_token(), param.ty()) else {
            continue;
        };
        let ty_text = slice_syntax(source, ty.syntax())
            .unwrap_or("Object")
            .trim()
            .to_string();
        types.insert(span_of_token(&name_tok), ty_text);
    }
    DeclaredTypes {
        types,
        initializer_offsets,
    }
}

#[derive(Debug, Clone)]
struct DeclaredTypeCandidate {
    offset: usize,
    ty: String,
}

fn collect_declared_types_by_name(
    source: &str,
    method: &EnclosingMethod,
    method_body: &ast::Block,
) -> HashMap<String, Vec<DeclaredTypeCandidate>> {
    fn strip_type_arguments(ty: &str) -> &str {
        ty.split_once('<')
            .map(|(before, _)| before)
            .unwrap_or(ty)
            .trim()
    }

    let mut out: HashMap<String, Vec<DeclaredTypeCandidate>> = HashMap::new();

    if let Some(params) = method.parameter_list() {
        for param in params.parameters() {
            let (Some(name_tok), Some(ty)) = (param.name_token(), param.ty()) else {
                continue;
            };
            let name = name_tok.text().to_string();
            let offset = u32::from(name_tok.text_range().start()) as usize;

            let ty_text_full = slice_syntax(source, ty.syntax()).unwrap_or("Object").trim();
            let ty_text = strip_type_arguments(ty_text_full);
            if ty_text.is_empty() {
                continue;
            }

            out.entry(name).or_default().push(DeclaredTypeCandidate {
                offset,
                ty: ty_text.to_string(),
            });
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
        let ty_text_full = slice_syntax(source, ty.syntax()).unwrap_or("Object").trim();
        let ty_text = strip_type_arguments(ty_text_full);
        if ty_text.is_empty() {
            continue;
        }
        let Some(list) = stmt.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            let Some(name_tok) = decl.name_token() else {
                continue;
            };
            let name = name_tok.text().to_string();
            let offset = u32::from(name_tok.text_range().start()) as usize;
            out.entry(name).or_default().push(DeclaredTypeCandidate {
                offset,
                ty: ty_text.to_string(),
            });
        }
    }

    // Try-with-resources variables are declared in `ResourceSpecification`, not in ordinary local
    // variable declaration statements.
    for try_stmt in method_body
        .syntax()
        .descendants()
        .filter_map(ast::TryStatement::cast)
    {
        let Some(resources) = try_stmt.resources() else {
            continue;
        };
        for resource in resources.resources() {
            // Local variable declaration form: `Type name = initializer`.
            let ty = resource.syntax().children().find_map(ast::Type::cast);
            let decl = resource
                .syntax()
                .children()
                .find_map(ast::VariableDeclarator::cast);
            let (Some(ty), Some(decl)) = (ty, decl) else {
                continue;
            };
            let Some(name_tok) = decl.name_token() else {
                continue;
            };
            let name = name_tok.text().to_string();
            let offset = u32::from(name_tok.text_range().start()) as usize;

            let ty_text_full = slice_syntax(source, ty.syntax()).unwrap_or("Object").trim();
            let ty_text = strip_type_arguments(ty_text_full);
            if ty_text.is_empty() {
                continue;
            }

            out.entry(name).or_default().push(DeclaredTypeCandidate {
                offset,
                ty: ty_text.to_string(),
            });
        }
    }

    // Catch clause parameters aren't represented as a `Parameter` node in the typed AST, but they
    // do introduce locals that we may need when inferring thrown exception types for extractions
    // within the catch body.
    for catch in method_body
        .syntax()
        .descendants()
        .filter_map(ast::CatchClause::cast)
    {
        let Some(body) = catch.body() else {
            continue;
        };
        let body_start = syntax_range(body.syntax()).start;
        let header_tokens: Vec<_> = catch
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| u32::from(tok.text_range().end()) as usize <= body_start)
            .collect();

        let Some(name_tok) = header_tokens
            .iter()
            .rev()
            .find(|tok| tok.kind().is_identifier_like())
        else {
            continue;
        };
        let Some(lparen) = header_tokens
            .iter()
            .find(|tok| tok.kind() == SyntaxKind::LParen)
        else {
            continue;
        };

        let lparen_end = u32::from(lparen.text_range().end()) as usize;
        let name_start = u32::from(name_tok.text_range().start()) as usize;
        if lparen_end >= name_start || name_start > source.len() {
            continue;
        }

        // Best-effort: recover the textual type between `(` and the param name.
        //
        // If this is a multi-catch (`A | B e`), we intentionally skip it to avoid picking an
        // incorrect throws type.
        let ty_text_full = source.get(lparen_end..name_start).unwrap_or("Object").trim();
        if ty_text_full.contains('|') {
            continue;
        }

        let ty_text = strip_type_arguments(ty_text_full);
        if ty_text.is_empty() {
            continue;
        }

        let name = name_tok.text().to_string();
        let offset = u32::from(name_tok.text_range().start()) as usize;
        out.entry(name).or_default().push(DeclaredTypeCandidate {
            offset,
            ty: ty_text.to_string(),
        });
    }

    // Variables introduced by classic `for` headers (e.g. `for (int i = 0; ...)`) and
    // enhanced-for headers (best-effort; varies by syntax shape).
    for for_stmt in method_body
        .syntax()
        .descendants()
        .filter_map(ast::ForStatement::cast)
    {
        let Some(header) = for_stmt.header() else {
            continue;
        };

        // Local variable declaration form: `Type x = ...` / `Type x, y = ...`.
        if let (Some(ty), Some(list)) = (
            header.syntax().children().find_map(ast::Type::cast),
            header
                .syntax()
                .children()
                .find_map(ast::VariableDeclaratorList::cast),
        ) {
            let ty_text_full = slice_syntax(source, ty.syntax()).unwrap_or("Object").trim();
            let ty_text = strip_type_arguments(ty_text_full);
            if !ty_text.is_empty() {
                for decl in list.declarators() {
                    let Some(name_tok) = decl.name_token() else {
                        continue;
                    };
                    let name = name_tok.text().to_string();
                    let offset = u32::from(name_tok.text_range().start()) as usize;
                    out.entry(name).or_default().push(DeclaredTypeCandidate {
                        offset,
                        ty: ty_text.to_string(),
                    });
                }
            }
        }
    }

    // Keep behavior deterministic.
    for candidates in out.values_mut() {
        candidates.sort_by_key(|cand| cand.offset);
    }

    out
}

fn collect_thrown_exceptions_in_statements(
    source: &str,
    selection_statements: &[ast::Statement],
    declared_types_by_name: &HashMap<String, Vec<DeclaredTypeCandidate>>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for stmt in selection_statements {
        for throw_stmt in stmt
            .syntax()
            .descendants()
            .filter_map(ast::ThrowStatement::cast)
        {
            let Some(ty) = infer_thrown_exception_type(source, &throw_stmt, declared_types_by_name)
            else {
                continue;
            };
            if seen.insert(ty.clone()) {
                out.push(ty);
            }
        }
    }

    out
}

fn infer_thrown_exception_type(
    source: &str,
    throw_stmt: &ast::ThrowStatement,
    declared_types_by_name: &HashMap<String, Vec<DeclaredTypeCandidate>>,
) -> Option<String> {
    let expr = throw_stmt.expression()?;
    infer_thrown_exception_type_from_expr(source, &expr, declared_types_by_name)
}

fn infer_thrown_exception_type_from_expr(
    source: &str,
    expr: &ast::Expression,
    declared_types_by_name: &HashMap<String, Vec<DeclaredTypeCandidate>>,
) -> Option<String> {
    fn strip_type_arguments(ty: &str) -> &str {
        ty.split_once('<')
            .map(|(before, _)| before)
            .unwrap_or(ty)
            .trim()
    }

    match expr {
        ast::Expression::ParenthesizedExpression(paren) => {
            let inner = paren.expression()?;
            infer_thrown_exception_type_from_expr(source, &inner, declared_types_by_name)
        }
        ast::Expression::NewExpression(new_expr) => {
            let ty = new_expr.ty()?;
            let ty_range = syntax_range(ty.syntax());
            let ty_text_full = source.get(ty_range.start..ty_range.end)?.trim();
            // Best-effort: strip away any type arguments (e.g. `Foo<Bar>` -> `Foo`) since Java
            // doesn't allow parameterized types in throws clauses.
            let ty_text = strip_type_arguments(ty_text_full);
            if ty_text.is_empty() {
                None
            } else {
                Some(ty_text.to_string())
            }
        }
        ast::Expression::NameExpression(name_expr) => {
            // Only handle simple identifiers, since we can only map those back to method locals or
            // parameters. If we can't infer a precise type, we omit it instead of falling back to
            // `Exception` to avoid widening the throws clause and potentially requiring changes at
            // the call site.
            if name_expr
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|tok| tok.kind() == SyntaxKind::Dot)
            {
                return None;
            }

            let mut ident_toks = name_expr
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|tok| tok.kind().is_identifier_like());

            let Some(tok) = ident_toks.next() else {
                return None;
            };
            // Ensure this is a simple name and not something like `a.b` (which would have been
            // caught above) or other weird constructs with multiple identifiers.
            if ident_toks.next().is_some() {
                return None;
            }

            let name = tok.text();
            let use_offset = u32::from(tok.text_range().start()) as usize;
            let candidates = declared_types_by_name.get(name)?;

            // Best-effort: pick the closest declaration that appears before the use site.
            let mut best: Option<&DeclaredTypeCandidate> = None;
            for cand in candidates {
                if cand.offset <= use_offset {
                    best = Some(cand);
                } else {
                    break;
                }
            }
            best.and_then(|cand| {
                let ty_text = strip_type_arguments(&cand.ty);
                if ty_text.is_empty() || ty_text.contains('|') {
                    None
                } else {
                    Some(ty_text.to_string())
                }
            })
        }
        _ => None,
    }
}

fn collect_reads_writes_in_flow_selection(
    body: &Body,
    selection: TextRange,
) -> (Vec<(LocalId, Span)>, HashSet<LocalId>) {
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
        StmtKind::While {
            condition,
            body: inner,
        } => {
            if contained {
                collect_reads_in_expr(body, *condition, selection, reads);
            }
            collect_reads_writes_in_stmt(body, *inner, selection, reads, writes);
        }
        StmtKind::DoWhile {
            body: inner,
            condition,
        } => {
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
        StmtKind::Try {
            body: inner,
            catches,
            finally,
        } => {
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

fn collect_reads_in_expr(
    body: &Body,
    expr_id: ExprId,
    selection: TextRange,
    reads: &mut Vec<(LocalId, Span)>,
) {
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
        ExprKind::FieldAccess { receiver, .. } => {
            collect_reads_in_expr(body, *receiver, selection, reads)
        }
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
    typeck: &mut Option<SingleFileTypecheck>,
    source: &str,
    body: &Body,
    types: &DeclaredTypes,
    local: LocalId,
    fallback_offset: Option<usize>,
    issues: &mut Vec<ExtractMethodIssue>,
) -> String {
    let local_data = &body.locals()[local.index()];
    let name = local_data.name.as_str().to_string();
    let Some(mut ty) = types.types.get(&local_data.span).cloned() else {
        issues.push(ExtractMethodIssue::UnknownType { name });
        return "Object".to_string();
    };

    if ty.trim() != "var" {
        return ty;
    }

    let mut offsets = Vec::new();
    if let Some(offset) = types.initializer_offsets.get(&local_data.span).copied() {
        offsets.push(offset);
    }
    if let Some(offset) = fallback_offset {
        offsets.push(offset);
    }

    if let Some(inferred) = infer_type_at_offsets(typeck, source, offsets) {
        ty = inferred;
        return ty;
    }

    issues.push(ExtractMethodIssue::UnknownType { name });
    "Object".to_string()
}

fn update_first_use(best: &mut Option<usize>, candidate: usize) {
    if best.is_none_or(|best| candidate < best) {
        *best = Some(candidate);
    }
}

fn first_local_use_after(body: &Body, local: LocalId, after: usize) -> Option<usize> {
    let mut best = None;
    collect_first_local_use_in_stmt(body, body.root(), local, after, &mut best);
    best
}

fn collect_first_local_use_in_stmt(
    body: &Body,
    stmt_id: StmtId,
    local: LocalId,
    after: usize,
    best: &mut Option<usize>,
) {
    let stmt = body.stmt(stmt_id);
    if stmt.span.end <= after {
        return;
    }

    match &stmt.kind {
        StmtKind::Block(stmts) => {
            for child in stmts {
                collect_first_local_use_in_stmt(body, *child, local, after, best);
            }
        }
        StmtKind::Let { initializer, .. } => {
            if let Some(init) = initializer {
                collect_first_local_use_in_expr(body, *init, local, after, best);
            }
        }
        StmtKind::Assign { value, .. } => {
            collect_first_local_use_in_expr(body, *value, local, after, best);
        }
        StmtKind::Expr(expr) => {
            collect_first_local_use_in_expr(body, *expr, local, after, best);
        }
        StmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_first_local_use_in_expr(body, *condition, local, after, best);
            collect_first_local_use_in_stmt(body, *then_branch, local, after, best);
            if let Some(else_branch) = else_branch {
                collect_first_local_use_in_stmt(body, *else_branch, local, after, best);
            }
        }
        StmtKind::While {
            condition,
            body: inner,
        } => {
            collect_first_local_use_in_expr(body, *condition, local, after, best);
            collect_first_local_use_in_stmt(body, *inner, local, after, best);
        }
        StmtKind::DoWhile {
            body: inner,
            condition,
        } => {
            collect_first_local_use_in_stmt(body, *inner, local, after, best);
            collect_first_local_use_in_expr(body, *condition, local, after, best);
        }
        StmtKind::For {
            init,
            condition,
            update,
            body: inner,
        } => {
            if let Some(init) = init {
                collect_first_local_use_in_stmt(body, *init, local, after, best);
            }
            if let Some(cond) = condition {
                collect_first_local_use_in_expr(body, *cond, local, after, best);
            }
            if let Some(update) = update {
                collect_first_local_use_in_stmt(body, *update, local, after, best);
            }
            collect_first_local_use_in_stmt(body, *inner, local, after, best);
        }
        StmtKind::Switch { expression, arms } => {
            collect_first_local_use_in_expr(body, *expression, local, after, best);
            for arm in arms {
                for value in &arm.values {
                    collect_first_local_use_in_expr(body, *value, local, after, best);
                }
                collect_first_local_use_in_stmt(body, arm.body, local, after, best);
            }
        }
        StmtKind::Try {
            body: inner,
            catches,
            finally,
        } => {
            collect_first_local_use_in_stmt(body, *inner, local, after, best);
            for catch in catches {
                collect_first_local_use_in_stmt(body, *catch, local, after, best);
            }
            if let Some(finally) = finally {
                collect_first_local_use_in_stmt(body, *finally, local, after, best);
            }
        }
        StmtKind::Return(expr) => {
            if let Some(expr) = expr {
                collect_first_local_use_in_expr(body, *expr, local, after, best);
            }
        }
        StmtKind::Throw(expr) => {
            collect_first_local_use_in_expr(body, *expr, local, after, best);
        }
        StmtKind::Break | StmtKind::Continue | StmtKind::Nop => {}
    }
}

fn collect_first_local_use_in_expr(
    body: &Body,
    expr_id: ExprId,
    local: LocalId,
    after: usize,
    best: &mut Option<usize>,
) {
    let expr = body.expr(expr_id);
    if expr.span.end <= after {
        return;
    }

    match &expr.kind {
        ExprKind::Local(l) => {
            if *l == local && expr.span.start >= after {
                update_first_use(best, expr.span.start);
            }
        }
        ExprKind::Null | ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::String(_) => {}
        ExprKind::New { args, .. } => {
            for arg in args {
                collect_first_local_use_in_expr(body, *arg, local, after, best);
            }
        }
        ExprKind::Unary { expr, .. } => {
            collect_first_local_use_in_expr(body, *expr, local, after, best);
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_first_local_use_in_expr(body, *lhs, local, after, best);
            collect_first_local_use_in_expr(body, *rhs, local, after, best);
        }
        ExprKind::FieldAccess { receiver, .. } => {
            collect_first_local_use_in_expr(body, *receiver, local, after, best);
        }
        ExprKind::Call { receiver, args, .. } => {
            if let Some(recv) = receiver {
                collect_first_local_use_in_expr(body, *recv, local, after, best);
            }
            for arg in args {
                collect_first_local_use_in_expr(body, *arg, local, after, best);
            }
        }
        ExprKind::Invalid { children } => {
            for child in children {
                collect_first_local_use_in_expr(body, *child, local, after, best);
            }
        }
    }
}

fn compute_return_value(
    typeck: &mut Option<SingleFileTypecheck>,
    source: &str,
    body: &Body,
    types: &DeclaredTypes,
    selection: TextRange,
    writes_in_selection: &HashSet<LocalId>,
    live_after_selection: &HashSet<LocalId>,
    issues: &mut Vec<ExtractMethodIssue>,
) -> Option<(LocalId, ReturnValue)> {
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
            let fallback_offset = first_local_use_after(body, *local, selection.end);
            let ty = type_for_local(typeck, source, body, types, *local, fallback_offset, issues);
            Some((
                *local,
                ReturnValue {
                    name,
                    ty,
                    declared_in_selection: local_declared_in_selection(body, *local, selection),
                },
            ))
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
    InBlock {
        block: nova_flow::BlockId,
        index: usize,
    },
    Terminator {
        block: nova_flow::BlockId,
    },
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

    live_after_stmt(body, &cfg, &live_out, location).unwrap_or_else(HashSet::new)
}

fn collect_stmt_locations(cfg: &nova_flow::ControlFlowGraph) -> HashMap<StmtId, StmtLocation> {
    let mut out = HashMap::new();
    for (idx, bb) in cfg.blocks.iter().enumerate() {
        let bb_id = nova_flow::BlockId(idx);
        for (pos, stmt) in bb.stmts.iter().enumerate() {
            out.entry(*stmt).or_insert(StmtLocation::InBlock {
                block: bb_id,
                index: pos,
            });
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

fn first_stmt_in_selection(
    body: &Body,
    selection: TextRange,
    locations: &HashMap<StmtId, StmtLocation>,
) -> Option<StmtId> {
    let mut best: Option<(usize, usize, usize, StmtId)> = None; // (start, end, stmt_idx, id)

    for stmt_id in locations.keys().copied() {
        let span = body.stmt(stmt_id).span;
        if !span_within_range(span, selection) {
            continue;
        }
        let key = (span.start, span.end, stmt_id.index());
        if best
            .as_ref()
            .is_none_or(|(start, end, idx, _)| key < (*start, *end, *idx))
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

// === Definite assignment (for Extract Method parameter threading) ===

fn definitely_assigned_at_selection_start(
    body: &Body,
    selection: TextRange,
    local: LocalId,
) -> Option<bool> {
    let cfg = build_cfg_with(body, &mut || {});
    let reachable = cfg.reachable_blocks();
    let (in_states, _) = compute_cfg_definite_assignment(body, &cfg, &reachable);

    let stmt_locations = collect_stmt_locations(&cfg);
    let first_stmt = first_stmt_in_selection(body, selection, &stmt_locations)?;
    let location = stmt_locations.get(&first_stmt).copied()?;

    let (block_id, stmt_index) = match location {
        StmtLocation::InBlock { block, index } => (block, Some(index)),
        StmtLocation::Terminator { block } => (block, None),
    };

    let mut state = in_states.get(block_id.index())?.clone();
    let block = cfg.block(block_id);
    match stmt_index {
        Some(index) => {
            for stmt in block.stmts.iter().take(index) {
                transfer_stmt_definite_assignment(body, *stmt, &mut state);
            }
        }
        None => {
            for stmt in &block.stmts {
                transfer_stmt_definite_assignment(body, *stmt, &mut state);
            }
        }
    }

    state.get(local.index()).copied()
}

fn compute_cfg_definite_assignment(
    body: &Body,
    cfg: &nova_flow::ControlFlowGraph,
    reachable: &[bool],
) -> (Vec<Vec<bool>>, Vec<Vec<bool>>) {
    let n_blocks = cfg.blocks.len();
    let n_locals = body.locals().len();

    let mut in_states = vec![vec![true; n_locals]; n_blocks];
    let mut out_states = vec![vec![true; n_locals]; n_blocks];

    let init = initial_assigned(body);
    in_states[cfg.entry.index()] = init.clone();

    let mut worklist = VecDeque::new();
    for idx in 0..n_blocks {
        if reachable[idx] {
            worklist.push_back(nova_flow::BlockId(idx));
        }
    }

    while let Some(bb) = worklist.pop_front() {
        if !reachable[bb.index()] {
            continue;
        }

        let new_in = if bb == cfg.entry {
            init.clone()
        } else {
            meet_assigned(
                n_locals,
                cfg.predecessors(bb).iter().filter_map(|pred| {
                    if reachable[pred.index()] {
                        Some(&out_states[pred.index()])
                    } else {
                        None
                    }
                }),
            )
        };

        if new_in != in_states[bb.index()] {
            in_states[bb.index()] = new_in.clone();
        }

        let new_out = transfer_definite_assignment(body, cfg, bb, &new_in);
        if new_out != out_states[bb.index()] {
            out_states[bb.index()] = new_out;
            for succ in cfg.successors(bb) {
                worklist.push_back(succ);
            }
        }
    }

    (in_states, out_states)
}

fn initial_assigned(body: &Body) -> Vec<bool> {
    body.locals()
        .iter()
        .map(|local| matches!(local.kind, LocalKind::Param))
        .collect()
}

fn meet_assigned<'a>(
    n_locals: usize,
    mut inputs: impl Iterator<Item = &'a Vec<bool>>,
) -> Vec<bool> {
    let Some(first) = inputs.next() else {
        return vec![false; n_locals];
    };
    let mut out = first.clone();
    for inp in inputs {
        for (slot, v) in out.iter_mut().zip(inp.iter().copied()) {
            *slot &= v;
        }
    }
    out
}

fn transfer_definite_assignment(
    body: &Body,
    cfg: &nova_flow::ControlFlowGraph,
    bb: nova_flow::BlockId,
    in_state: &[bool],
) -> Vec<bool> {
    let mut state = in_state.to_vec();
    let block = cfg.block(bb);
    for stmt in &block.stmts {
        transfer_stmt_definite_assignment(body, *stmt, &mut state);
    }
    state
}

fn transfer_stmt_definite_assignment(body: &Body, stmt: StmtId, state: &mut [bool]) {
    match &body.stmt(stmt).kind {
        StmtKind::Let { local, initializer } => {
            state[local.index()] = initializer.is_some();
        }
        StmtKind::Assign { target, .. } => {
            state[target.index()] = true;
        }
        _ => {}
    }
}

fn insertion_offset_end_of_type_body(
    source: &str,
    body: &nova_syntax::SyntaxNode,
    newline: &str,
) -> usize {
    // Insert immediately before the newline that starts the closing brace line.
    let mut close = None;
    for tok in body.children_with_tokens().filter_map(|el| el.into_token()) {
        if tok.kind() == SyntaxKind::RBrace {
            close = Some(u32::from(tok.text_range().start()) as usize);
        }
    }
    let close = close.unwrap_or_else(|| syntax_range(body).end);
    let line_start = line_start_offset(source, close);
    line_start.saturating_sub(newline.len())
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
    source[..offset]
        .rfind(['\n', '\r'])
        .map(|p| p + 1)
        .unwrap_or(0)
}

fn indentation_at(source: &str, offset: usize) -> String {
    let start = line_start_offset(source, offset);
    source[start..offset]
        .chars()
        .take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
        .collect()
}

fn reindent(block: &str, old_indent: &str, new_indent: &str, newline: &str) -> String {
    let normalized = block.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    for line in normalized.split_inclusive('\n') {
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
    if !normalized.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    if newline == "\n" {
        out
    } else {
        out.replace('\n', newline)
    }
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

fn find_expression_exact(body: &ast::Block, selection: TextRange) -> Option<ast::Expression> {
    body.syntax()
        .descendants()
        .filter_map(ast::Expression::cast)
        .find(|expr| syntax_range(expr.syntax()) == selection)
}

fn expression_has_local_mutation(expr: &ast::Expression) -> bool {
    if expr
        .syntax()
        .descendants()
        .any(|node| node.kind() == SyntaxKind::AssignmentExpression)
    {
        return true;
    }
    expr.syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|tok| matches!(tok.kind(), SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
}

fn find_flow_expr_exact(body: &Body, target: Span) -> Option<ExprId> {
    let mut found = None;
    find_flow_expr_in_stmt(body, body.root(), target, &mut found);
    found
}

fn find_flow_expr_in_stmt(body: &Body, stmt_id: StmtId, target: Span, found: &mut Option<ExprId>) {
    if found.is_some() {
        return;
    }

    let stmt = body.stmt(stmt_id);
    match &stmt.kind {
        StmtKind::Block(stmts) => {
            for stmt in stmts {
                find_flow_expr_in_stmt(body, *stmt, target, found);
            }
        }
        StmtKind::Let { initializer, .. } => {
            if let Some(init) = initializer {
                find_flow_expr_in_expr(body, *init, target, found);
            }
        }
        StmtKind::Assign { value, .. } => find_flow_expr_in_expr(body, *value, target, found),
        StmtKind::Expr(expr) => find_flow_expr_in_expr(body, *expr, target, found),
        StmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            find_flow_expr_in_expr(body, *condition, target, found);
            find_flow_expr_in_stmt(body, *then_branch, target, found);
            if let Some(else_branch) = else_branch {
                find_flow_expr_in_stmt(body, *else_branch, target, found);
            }
        }
        StmtKind::While {
            condition,
            body: inner,
        } => {
            find_flow_expr_in_expr(body, *condition, target, found);
            find_flow_expr_in_stmt(body, *inner, target, found);
        }
        StmtKind::DoWhile {
            body: inner,
            condition,
        } => {
            find_flow_expr_in_stmt(body, *inner, target, found);
            find_flow_expr_in_expr(body, *condition, target, found);
        }
        StmtKind::For {
            init,
            condition,
            update,
            body: inner,
        } => {
            if let Some(init) = init {
                find_flow_expr_in_stmt(body, *init, target, found);
            }
            if let Some(cond) = condition {
                find_flow_expr_in_expr(body, *cond, target, found);
            }
            if let Some(update) = update {
                find_flow_expr_in_stmt(body, *update, target, found);
            }
            find_flow_expr_in_stmt(body, *inner, target, found);
        }
        StmtKind::Switch { expression, arms } => {
            find_flow_expr_in_expr(body, *expression, target, found);
            for arm in arms {
                for value in &arm.values {
                    find_flow_expr_in_expr(body, *value, target, found);
                }
                find_flow_expr_in_stmt(body, arm.body, target, found);
            }
        }
        StmtKind::Try {
            body: inner,
            catches,
            finally,
        } => {
            find_flow_expr_in_stmt(body, *inner, target, found);
            for catch in catches {
                find_flow_expr_in_stmt(body, *catch, target, found);
            }
            if let Some(finally) = finally {
                find_flow_expr_in_stmt(body, *finally, target, found);
            }
        }
        StmtKind::Return(expr) => {
            if let Some(expr) = expr {
                find_flow_expr_in_expr(body, *expr, target, found);
            }
        }
        StmtKind::Throw(expr) => find_flow_expr_in_expr(body, *expr, target, found),
        StmtKind::Break | StmtKind::Continue | StmtKind::Nop => {}
    }
}

fn find_flow_expr_in_expr(body: &Body, expr_id: ExprId, target: Span, found: &mut Option<ExprId>) {
    if found.is_some() {
        return;
    }

    let expr = body.expr(expr_id);
    if expr.span == target {
        *found = Some(expr_id);
        return;
    }

    match &expr.kind {
        ExprKind::Local(_)
        | ExprKind::Null
        | ExprKind::Bool(_)
        | ExprKind::Int(_)
        | ExprKind::String(_) => {}
        ExprKind::New { args, .. } => {
            for arg in args {
                find_flow_expr_in_expr(body, *arg, target, found);
            }
        }
        ExprKind::Unary { expr, .. } => find_flow_expr_in_expr(body, *expr, target, found),
        ExprKind::Binary { lhs, rhs, .. } => {
            find_flow_expr_in_expr(body, *lhs, target, found);
            find_flow_expr_in_expr(body, *rhs, target, found);
        }
        ExprKind::FieldAccess { receiver, .. } => {
            find_flow_expr_in_expr(body, *receiver, target, found)
        }
        ExprKind::Call { receiver, args, .. } => {
            if let Some(recv) = receiver {
                find_flow_expr_in_expr(body, *recv, target, found);
            }
            for arg in args {
                find_flow_expr_in_expr(body, *arg, target, found);
            }
        }
        ExprKind::Invalid { children } => {
            for child in children {
                find_flow_expr_in_expr(body, *child, target, found);
            }
        }
    }
}

fn collect_reads_in_flow_expr(
    body: &Body,
    expr_id: ExprId,
    selection: TextRange,
) -> Vec<(LocalId, Span)> {
    let mut reads: Vec<(LocalId, Span)> = Vec::new();
    collect_reads_in_expr(body, expr_id, selection, &mut reads);

    reads.sort_by(|a, b| {
        a.1.start
            .cmp(&b.1.start)
            .then_with(|| a.1.end.cmp(&b.1.end))
    });
    let mut seen: HashSet<LocalId> = HashSet::new();
    reads
        .into_iter()
        .filter(|(local, _)| seen.insert(*local))
        .collect()
}

fn infer_expression_return_type(
    source: &str,
    expr: &ast::Expression,
    flow_body: &Body,
    flow_expr: Option<ExprId>,
    types_by_span: &HashMap<Span, String>,
) -> Option<String> {
    if let Some(flow_expr) = flow_expr {
        if let ExprKind::Local(local_id) = &flow_body.expr(flow_expr).kind {
            let local = &flow_body.locals()[local_id.index()];
            if let Some(ty) = types_by_span.get(&local.span) {
                return Some(ty.clone());
            }
        }
    }

    match expr {
        ast::Expression::LiteralExpression(lit) => {
            let tok = lit
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)?;
            match tok.kind() {
                SyntaxKind::IntLiteral | SyntaxKind::Number => Some("int".to_string()),
                SyntaxKind::StringLiteral => Some("String".to_string()),
                SyntaxKind::CharLiteral => Some("char".to_string()),
                SyntaxKind::TrueKw | SyntaxKind::FalseKw => Some("boolean".to_string()),
                _ => None,
            }
        }
        ast::Expression::ParenthesizedExpression(par) => par.expression().and_then(|inner| {
            infer_expression_return_type(source, &inner, flow_body, flow_expr, types_by_span)
        }),
        ast::Expression::UnaryExpression(unary) => {
            // `!x` is boolean; otherwise default numeric.
            let is_not =
                nova_syntax::ast::support::token(unary.syntax(), SyntaxKind::Bang).is_some();
            if is_not {
                return Some("boolean".to_string());
            }
            Some("int".to_string())
        }
        ast::Expression::BinaryExpression(_) | ast::Expression::NameExpression(_) => {
            let range = syntax_range(expr.syntax());
            let text = source
                .get(range.start..range.end)
                .unwrap_or_default()
                .trim();
            if text.contains('"') {
                return Some("String".to_string());
            }
            // Comparison / boolean operators.
            let has_bool_op = expr
                .syntax()
                .children_with_tokens()
                .filter_map(|it| it.into_token())
                .any(|tok| {
                    matches!(
                        tok.kind(),
                        SyntaxKind::EqEq
                            | SyntaxKind::BangEq
                            | SyntaxKind::AmpAmp
                            | SyntaxKind::PipePipe
                            | SyntaxKind::Less
                            | SyntaxKind::LessEq
                            | SyntaxKind::Greater
                            | SyntaxKind::GreaterEq
                    )
                });
            if has_bool_op {
                return Some("boolean".to_string());
            }
            Some("int".to_string())
        }
        _ => None,
    }
}

fn local_var_is_final_in_selection(body: &ast::Block, selection: TextRange, name: &str) -> bool {
    for stmt in body
        .syntax()
        .descendants()
        .filter_map(ast::LocalVariableDeclarationStatement::cast)
    {
        let Some(list) = stmt.declarator_list() else {
            continue;
        };
        let declares_name_in_selection = list.declarators().any(|decl| {
            let Some(tok) = decl.name_token() else {
                return false;
            };
            tok.text() == name && span_within_range(span_of_token(&tok), selection)
        });
        if !declares_name_in_selection {
            continue;
        }
        return stmt.modifiers().is_some_and(|mods| {
            mods.syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|tok| tok.kind() == SyntaxKind::FinalKw)
        });
    }
    false
}
