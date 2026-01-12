use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use nova_format::NewlineStyle;
use nova_syntax::ast::{self, AstNode};
use nova_syntax::{parse_java, SyntaxKind};
use thiserror::Error;

use crate::edit::{apply_text_edits, FileId, TextEdit, TextRange, WorkspaceEdit};
use crate::java::{JavaSymbolKind, SymbolId};
use crate::materialize::{materialize, MaterializeError};
use crate::semantic::{Conflict, RefactorDatabase, SemanticChange};

#[derive(Debug, Error)]
pub enum RefactorError {
    #[error("refactoring has conflicts: {0:?}")]
    Conflicts(Vec<Conflict>),
    #[error("rename is not supported for this symbol (got {kind:?})")]
    RenameNotSupported { kind: Option<JavaSymbolKind> },
    #[error("extract variable is not supported inside assert statements")]
    ExtractNotSupportedInAssert,
    #[error(transparent)]
    Materialize(#[from] MaterializeError),
    #[error(transparent)]
    MoveJava(#[from] crate::move_java::RefactorError),
    #[error("unknown file {0:?}")]
    UnknownFile(FileId),
    #[error("invalid variable name `{name}`: {reason}")]
    InvalidIdentifier { name: String, reason: &'static str },
    #[error("expected a variable with initializer for inline")]
    InlineNotSupported,
    #[error("no variable usage at the given cursor/usage range")]
    InlineNoUsageAtCursor,
    #[error("variable initializer has side effects and cannot be inlined safely")]
    InlineSideEffects,
    #[error("inlining would change value: {reason}")]
    InlineWouldChangeValue { reason: String },
    #[error("failed to parse Java source")]
    ParseError,
    #[error("selection does not resolve to a single expression")]
    InvalidSelection,
    #[error("extract variable is not supported in this context: {reason}")]
    ExtractNotSupported { reason: &'static str },
    #[error("could not infer type for extracted expression")]
    TypeInferenceFailed,
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

pub struct RenameParams {
    pub symbol: SymbolId,
    pub new_name: String,
}

pub fn rename(
    db: &dyn RefactorDatabase,
    params: RenameParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let kind = db.symbol_kind(params.symbol);
    if matches!(kind, Some(JavaSymbolKind::Package)) {
        let def = db
            .symbol_definition(params.symbol)
            .ok_or(RefactorError::RenameNotSupported { kind })?;

        let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
        for file in db.all_files() {
            let text = db
                .file_text(&file)
                .ok_or_else(|| RefactorError::UnknownFile(file.clone()))?;
            files.insert(PathBuf::from(file.0), text.to_string());
        }

        return Ok(crate::move_java::move_package_workspace_edit(
            &files,
            crate::move_java::MovePackageParams {
                old_package: def.name,
                new_package: params.new_name,
            },
        )?);
    }

    match kind {
        Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter) => {
            let conflicts = check_rename_conflicts(db, params.symbol, &params.new_name);
            if !conflicts.is_empty() {
                return Err(RefactorError::Conflicts(conflicts));
            }
        }
        Some(JavaSymbolKind::Field | JavaSymbolKind::Method | JavaSymbolKind::Type) => {
            // Best-effort: conflict checking is currently scope-based and tuned for local/parameter
            // renames. Allow member/type renames without additional validation for now.
        }
        Some(JavaSymbolKind::Package) | None => {
            return Err(RefactorError::RenameNotSupported { kind })
        }
    }

    let mut changes = vec![SemanticChange::Rename {
        symbol: params.symbol,
        new_name: params.new_name.clone(),
    }];

    // Java annotation shorthand `@Anno(expr)` is desugared as `@Anno(value = expr)`. If the
    // annotation element method `value()` is renamed, shorthand usages must be rewritten to an
    // explicit element-value pair using the new name.
    if matches!(kind, Some(JavaSymbolKind::Method)) {
        changes.extend(annotation_value_shorthand_updates(
            db,
            params.symbol,
            &params.new_name,
        ));
    }

    Ok(materialize(db, changes)?)
}

fn annotation_value_shorthand_updates(
    db: &dyn RefactorDatabase,
    symbol: SymbolId,
    new_name: &str,
) -> Vec<SemanticChange> {
    if new_name == "value" {
        return Vec::new();
    }

    let Some(kind) = db.symbol_kind(symbol) else {
        return Vec::new();
    };
    if kind != JavaSymbolKind::Method {
        return Vec::new();
    }

    let Some(def) = db.symbol_definition(symbol) else {
        return Vec::new();
    };
    if def.name != "value" {
        return Vec::new();
    }

    let Some(text) = db.file_text(&def.file) else {
        return Vec::new();
    };

    let parsed = parse_java(text);
    let root = parsed.syntax();

    // Find the method declaration in the syntax tree and confirm it's a 0-arg `value()` inside an
    // `@interface`. This ensures we only apply the rewrite when renaming the special annotation
    // element, not arbitrary methods named `value`.
    let mut annotation_name = None;
    for method in root.descendants().filter_map(ast::MethodDeclaration::cast) {
        let Some(name_tok) = method.name_token() else {
            continue;
        };
        if syntax_token_range(&name_tok) != def.name_range {
            continue;
        }

        let param_count = method
            .parameter_list()
            .map(|list| list.parameters().count())
            .unwrap_or(0);
        if param_count != 0 {
            return Vec::new();
        }

        let Some(annotation_ty) = method
            .syntax()
            .ancestors()
            .find_map(ast::AnnotationTypeDeclaration::cast)
        else {
            return Vec::new();
        };
        annotation_name = annotation_ty.name_token().map(|tok| tok.text().to_string());
        break;
    }

    let Some(annotation_name) = annotation_name else {
        return Vec::new();
    };

    let existing_refs = db.find_references(symbol);

    fn annotation_args_inner_range(
        source: &str,
        args: &ast::AnnotationElementValuePairList,
    ) -> Option<TextRange> {
        let range = syntax_range(args.syntax());
        if range.len() < 2 {
            return None;
        }

        let bytes = source.as_bytes();
        if bytes.get(range.start) != Some(&b'(') {
            return None;
        }
        if bytes.get(range.end.saturating_sub(1)) != Some(&b')') {
            return None;
        }

        Some(TextRange::new(range.start + 1, range.end - 1))
    }

    let mut out = Vec::new();
    let mut seen: HashSet<(FileId, TextRange)> = HashSet::new();

    for file in db.all_files() {
        let Some(source) = db.file_text(&file) else {
            continue;
        };
        let parsed = parse_java(source);
        let root = parsed.syntax();

        for ann in root.descendants().filter_map(ast::Annotation::cast) {
            let Some(name) = ann.name() else {
                continue;
            };
            let name_text = name.text();
            let simple = name_text
                .rsplit('.')
                .next()
                .unwrap_or_else(|| name_text.as_str());
            if simple != annotation_name {
                continue;
            }

            let Some(args) = ann.arguments() else {
                continue;
            };

            let has_pairs = args.pairs().next().is_some();
            let value = args.value();

            // If the parse produced both a shorthand value and named pairs, skip (shouldn't happen
            // in valid Java).
            if value.is_some() && has_pairs {
                continue;
            }

            if let Some(value) = value {
                // Shorthand `@Anno(expr)` form.
                if has_pairs {
                    continue;
                }

                let Some(inner_range) = annotation_args_inner_range(source, &args) else {
                    continue;
                };
                if !seen.insert((file.clone(), inner_range)) {
                    continue;
                }

                let value_range = syntax_range(value.syntax());
                let value_text = source
                    .get(value_range.start..value_range.end)
                    .unwrap_or_default()
                    .trim();
                if value_text.is_empty() {
                    continue;
                }

                out.push(SemanticChange::UpdateReferences {
                    file: file.clone(),
                    range: inner_range,
                    new_text: format!("{new_name} = {value_text}"),
                });
            } else if has_pairs {
                // Named pair `@Anno(value = expr)` form.
                for pair in args.pairs() {
                    let Some(name_tok) = pair.name_token() else {
                        continue;
                    };
                    if name_tok.text() != "value" {
                        continue;
                    }

                    let name_range = syntax_token_range(&name_tok);
                    // If the semantic DB already records this as a reference, rely on the normal
                    // rename path to avoid overlapping edits.
                    if existing_refs.iter().any(|r| {
                        r.file == file && ranges_overlap(r.range, name_range)
                    }) {
                        continue;
                    }

                    if !seen.insert((file.clone(), name_range)) {
                        continue;
                    }

                    out.push(SemanticChange::UpdateReferences {
                        file: file.clone(),
                        range: name_range,
                        new_text: new_name.to_string(),
                    });
                }
            }
        }
    }

    out
}

fn check_rename_conflicts(
    db: &dyn RefactorDatabase,
    symbol: SymbolId,
    new_name: &str,
) -> Vec<Conflict> {
    let mut conflicts = Vec::new();

    let Some(def) = db.symbol_definition(symbol) else {
        return conflicts;
    };

    let Some(scope) = db.symbol_scope(symbol) else {
        return conflicts;
    };

    if let Some(existing) = db.resolve_name_in_scope(scope, new_name) {
        if existing != symbol {
            conflicts.push(Conflict::NameCollision {
                file: def.file.clone(),
                name: new_name.to_string(),
                existing_symbol: existing,
            });
        }
    }

    if let Some(shadowed) = db.would_shadow(scope, new_name) {
        if shadowed != symbol {
            conflicts.push(Conflict::Shadowing {
                file: def.file.clone(),
                name: new_name.to_string(),
                shadowed_symbol: shadowed,
            });
        }
    }

    for usage in db.find_references(symbol) {
        if !db.is_visible_from(symbol, &usage.file, new_name) {
            conflicts.push(Conflict::VisibilityLoss {
                file: usage.file.clone(),
                usage_range: usage.range,
                name: new_name.to_string(),
            });
        }
    }

    conflicts
}

pub struct ExtractVariableParams {
    pub file: FileId,
    pub expr_range: TextRange,
    pub name: String,
    pub use_var: bool,
    pub replace_all: bool,
}

pub fn extract_variable(
    db: &dyn RefactorDatabase,
    params: ExtractVariableParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let name = crate::java::validate_java_identifier(&params.name).map_err(|err| {
        let trimmed = params.name.trim();
        let display_name = if trimmed.is_empty() {
            "<empty>".to_string()
        } else {
            trimmed.to_string()
        };
        RefactorError::InvalidIdentifier {
            name: display_name,
            reason: err.reason(),
        }
    })?;

    let text = db
        .file_text(&params.file)
        .ok_or_else(|| RefactorError::UnknownFile(params.file.clone()))?;

    if params.expr_range.start > params.expr_range.end {
        return Err(RefactorError::InvalidSelection);
    }

    if params.expr_range.end > text.len() {
        return Err(RefactorError::Edit(crate::edit::EditError::OutOfBounds {
            file: params.file.clone(),
            range: params.expr_range,
            len: text.len(),
        }));
    }

    let selection = trim_range(text, params.expr_range);
    if selection.len() == 0 {
        return Err(RefactorError::InvalidSelection);
    }

    let parsed = parse_java(text);
    if !parsed.errors.is_empty() {
        return Err(RefactorError::ParseError);
    }

    let root = parsed.syntax();
    let expr =
        find_expression(text, root.clone(), selection).ok_or(RefactorError::InvalidSelection)?;

    // Extracting expressions from `assert` statements is unsafe: Java assertions may be disabled
    // at runtime, and hoisting the expression into a preceding local variable would force it to be
    // evaluated unconditionally.
    if expr
        .syntax()
        .ancestors()
        .any(|node| ast::AssertStatement::cast(node).is_some())
    {
        return Err(RefactorError::ExtractNotSupportedInAssert);
    }

    // Java pattern matching for `instanceof` introduces pattern variables whose scope is tied to
    // the conditional expression. Extracting an `instanceof <pattern>` would remove the binding
    // and either break compilation (pattern variable no longer in scope) or change semantics.
    if let ast::Expression::InstanceofExpression(instanceof) = &expr {
        if instanceof.pattern().is_some() {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract `instanceof` pattern matching expression",
            });
        }
    }

    // Be conservative: reject extracting any expression subtree that contains patterns (nested
    // patterns, switch patterns, etc) so we never let pattern variables escape their scope.
    if expr
        .syntax()
        .descendants()
        .any(|node| node.kind() == SyntaxKind::Pattern)
    {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract expression containing pattern variables",
        });
    }

    // `extract_variable` inserts the new declaration before the enclosing statement. For
    // expression-bodied lambdas (`x -> x + 1`), there is no statement inside the lambda body,
    // so extraction would hoist the declaration outside the lambda (breaking compilation when
    // referencing parameters and/or changing evaluation timing). Reject this case.
    let in_expression_bodied_lambda = expr
        .syntax()
        .ancestors()
        .filter_map(ast::LambdaBody::cast)
        .any(|body| body.expression().is_some());
    if in_expression_bodied_lambda {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract from expression-bodied lambda body",
        });
    }

    // Expressions inside `try ( ... )` resource specifications have special semantics: the
    // AutoCloseable(s) created/used there are closed automatically at the end of the try block.
    // Naively extracting such expressions to a normal local variable declared before the `try`
    // can change resource lifetime/closing behavior. Until we implement a semantics-preserving
    // strategy (e.g. rewriting to `try (var tmp = <expr>)` where legal), refuse extraction here.
    if expr.syntax().ancestors().any(|node| {
        ast::ResourceSpecification::cast(node.clone()).is_some()
            || ast::Resource::cast(node).is_some()
    }) {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract from try-with-resources resource specification",
        });
    }

    if let Some(reason) = constant_expression_only_context_reason(&expr) {
        return Err(RefactorError::ExtractNotSupported { reason });
    }

    let expr_range = syntax_range(expr.syntax());
    let expr_text = text
        .get(selection.start..selection.end)
        .ok_or(RefactorError::InvalidSelection)?
        .to_string();

    if let Some(reason) = extract_variable_crosses_execution_boundary(&expr) {
        return Err(RefactorError::ExtractNotSupported { reason });
    }

    // Extracting a side-effectful expression into a new statement can change evaluation order or
    // conditionality (e.g. when the expression appears under `?:`, `&&`, etc). Be conservative.
    //
    // For explicit-typed extraction we allow side-effectful expressions as a best-effort fallback,
    // since many common selections (`new Foo()`) would otherwise be rejected entirely.
    if params.use_var && has_side_effects(expr.syntax()) {
        return Err(RefactorError::ExtractNotSupported {
            reason: "expression has side effects and cannot be extracted safely",
        });
    }

    let stmt = expr
        .syntax()
        .ancestors()
        .find_map(ast::Statement::cast)
        .ok_or(RefactorError::InvalidSelection)?;

    // Java requires an explicit constructor invocation (`this(...)` / `super(...)`) to be the
    // first statement in a constructor body. Extracting a variable would insert a new statement
    // before it, producing uncompilable code.
    if matches!(stmt, ast::Statement::ExplicitConstructorInvocation(_)) {
        return Err(RefactorError::ExtractNotSupported {
            reason:
                "cannot extract from explicit constructor invocation (`this(...)` / `super(...)`)",
        });
    }
    reject_unsafe_extract_variable_context(&expr, &stmt)?;

    // Be conservative: extracting from loop conditions changes evaluation frequency.
    match stmt {
        ast::Statement::WhileStatement(_) => {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract from while condition",
            })
        }
        ast::Statement::DoWhileStatement(_) => {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract from do-while condition",
            })
        }
        ast::Statement::ForStatement(_) => {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract from for statement header",
            })
        }
        _ => {}
    }

    let stmt_range = syntax_range(stmt.syntax());
    if let Some(parent) = stmt.syntax().parent() {
        // Reject labeled statements (`label:\n  stmt;`) where inserting at the start of the line
        // would "steal" the label and change control flow.
        if ast::LabeledStatement::cast(parent.clone()).is_some() {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract into a labeled statement body",
            });
        }

        // Reject switch arrow rules with a single statement body:
        // `case 1 -> stmt;`
        // Inserting a new statement would require rewriting the body to a `{ ... }` block.
        if ast::SwitchRule::cast(parent.clone()).is_some()
            && !matches!(stmt, ast::Statement::Block(_))
        {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract into a single-statement switch rule body without braces",
            });
        }

        // Reject inserting into single-statement control structure bodies without braces. In those
        // contexts we'd have to introduce a `{ ... }` block to preserve control flow.
        if !matches!(stmt, ast::Statement::Block(_))
            && (ast::IfStatement::cast(parent.clone()).is_some()
                || ast::WhileStatement::cast(parent.clone()).is_some()
                || ast::DoWhileStatement::cast(parent.clone()).is_some()
                || ast::ForStatement::cast(parent).is_some())
        {
            return Err(RefactorError::ExtractNotSupported {
                reason:
                    "cannot extract into a single-statement control structure body without braces",
            });
        }
    }

    // Reject statements that start mid-line (`if (cond) foo();`, `case 1: foo();`, ...). Our
    // insertion strategy inserts at the start of the statement's line; doing so for a mid-line
    // statement would either be syntactically invalid or change semantics.
    let insert_pos = line_start(text, stmt_range.start);
    if text[insert_pos..stmt_range.start]
        .chars()
        .any(|c| !c.is_whitespace())
    {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract when the enclosing statement starts mid-line",
        });
    }

    let indent = current_indent(text, insert_pos);

    check_extract_variable_name_conflicts(&stmt, insert_pos, &name)?;

    let ty = if params.use_var {
        "var".to_string()
    } else {
        let mut parser_ty = infer_expr_type(&expr);
        if parser_ty.contains("<>") {
            parser_ty = parser_ty.replace("<>", "");
        }

        let typeck_ty = best_type_at_range_display(db, &params.file, text, expr_range);

        // When emitting an explicit type (instead of `var`), prefer parser-inferred names when
        // they are already meaningful and local (`Foo` instead of `Test.Foo`). Only fall back to
        // typeck when it provides genuinely richer information (e.g. inferred generics) or when
        // the parser couldn't infer anything useful (`Object`).
        match typeck_ty {
            Some(typeck_ty) if typeck_ty != "null" => {
                if parser_ty == "Object" {
                    if typeck_ty != "Object" {
                        typeck_ty
                    } else {
                        parser_ty
                    }
                } else if typeck_ty == "Object" {
                    parser_ty
                } else {
                    let typeck_has_type_args = typeck_ty.contains('<');
                    let parser_has_type_args = parser_ty.contains('<');
                    let typeck_has_arrays = typeck_ty.contains("[]");
                    let parser_has_arrays = parser_ty.contains("[]");

                    // Prefer typeck when it adds generics or array dimensions that parser didn't
                    // capture (e.g. diamond inference).
                    if (typeck_has_type_args && !parser_has_type_args)
                        || (typeck_has_arrays && !parser_has_arrays)
                    {
                        typeck_ty
                    } else if strip_leading_qualifiers(&typeck_ty) == parser_ty {
                        // Avoid over-qualification like `Test.Foo` when `Foo` is already valid in
                        // this scope.
                        parser_ty
                    } else {
                        typeck_ty
                    }
                }
            }
            _ => parser_ty,
        }
    };

    // Special-case: when extracting the whole expression of an expression statement, the usual
    // strategy (insert declaration before the statement + replace the selected expression with the
    // variable name) would leave a bare identifier statement (`name;`), which is not valid Java.
    //
    // In this case, replace the entire expression statement with a local variable declaration.
    if let ast::Statement::ExpressionStatement(expr_stmt) = &stmt {
        if let Some(stmt_expr) = expr_stmt.expression() {
            let stmt_expr_range = trim_range(text, syntax_range(stmt_expr.syntax()));
            if stmt_expr_range.start == selection.start && stmt_expr_range.end == selection.end {
                let stmt_range = syntax_range(expr_stmt.syntax());
                let prefix = &text[stmt_range.start..selection.start];
                let suffix = &text[selection.end..stmt_range.end];
                let replacement = format!("{prefix}{ty} {name} = {expr_text}{suffix}");

                let mut edit = WorkspaceEdit::new(vec![TextEdit::replace(
                    params.file.clone(),
                    stmt_range,
                    replacement,
                )]);
                edit.normalize()?;
                return Ok(edit);
            }
        }
    }

    let newline = NewlineStyle::detect(text).as_str();

    // Special-case: extracting inside a multi-declarator local variable declaration needs to
    // preserve scoping and initializer evaluation order. Naively inserting the extracted binding
    // before the whole statement can be invalid (later declarators can reference earlier ones) and
    // can also reorder side effects relative to earlier declarators.
    //
    // Example:
    //   int a = 1, b = a + 2;
    //
    // Desired:
    //   int a = 1;
    //   var tmp = a + 2;
    //   int b = tmp;
    if let ast::Statement::LocalVariableDeclarationStatement(local) = &stmt {
        if let Some(replacement) = rewrite_multi_declarator_local_variable_declaration(
            text, local, stmt_range, expr_range, &expr_text, &name, &ty, &indent, newline,
        ) {
            let mut edit = WorkspaceEdit::new(vec![TextEdit::replace(
                params.file.clone(),
                stmt_range,
                replacement,
            )]);
            edit.normalize()?;
            return Ok(edit);
        }
    }

    let decl = format!("{indent}{ty} {} = {expr_text};{newline}", &name);

    let occurrences = if params.replace_all {
        find_replace_all_occurrences_same_execution_context(text, root.clone(), &stmt, &expr_text)
    } else {
        vec![expr_range]
    };

    let mut edits = Vec::with_capacity(1 + occurrences.len());
    edits.push(TextEdit::insert(params.file.clone(), insert_pos, decl));
    for range in occurrences {
        edits.push(TextEdit::replace(params.file.clone(), range, name.clone()));
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

pub struct InlineVariableParams {
    pub symbol: SymbolId,
    pub inline_all: bool,
    /// When `inline_all` is false, identifies which usage should be inlined.
    ///
    /// This must match the byte range of a reference returned by `find_references(symbol)`.
    pub usage_range: Option<TextRange>,
}

fn contains_unknown_name_expression(
    db: &dyn RefactorDatabase,
    file: &FileId,
    source: &str,
    name: &str,
    symbol: SymbolId,
    known_refs: &[crate::semantic::Reference],
) -> bool {
    // Some RefactorDatabase implementations may be able to return semantic references for a symbol
    // even when `symbol_at` span tracking is incomplete. Treat any same-name occurrence whose
    // identifier token range exactly matches a known reference range as "known" without requiring
    // an additional `symbol_at` resolution step.
    let known_ranges: HashSet<TextRange> = known_refs
        .iter()
        .filter(|r| &r.file == file)
        .map(|r| r.range)
        .collect();

    let parsed = parse_java(source);
    let root = parsed.syntax();

    for name_expr in root.descendants().filter_map(ast::NameExpression::cast) {
        let mut text = String::new();
        let mut ident_start: Option<usize> = None;
        let mut ident_range: Option<TextRange> = None;

        for tok in name_expr
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
        {
            if tok.kind().is_trivia() {
                continue;
            }
            text.push_str(tok.text());
            if ident_start.is_none() && tok.kind().is_identifier_like() && tok.text() == name {
                let start = u32::from(tok.text_range().start()) as usize;
                let end = u32::from(tok.text_range().end()) as usize;
                ident_start = Some(start);
                ident_range = Some(TextRange::new(start, end));
            }
        }

        if text != name {
            continue;
        }

        let Some(ident_start) = ident_start else {
            continue;
        };

        if let Some(ident_range) = ident_range {
            if known_ranges.contains(&ident_range) {
                continue;
            }
        }

        match db.symbol_at(file, ident_start) {
            Some(resolved) if resolved == symbol => {}
            Some(_) => {}
            None => return true,
        }
    }

    false
}

pub fn inline_variable(
    db: &dyn RefactorDatabase,
    params: InlineVariableParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let def = db
        .symbol_definition(params.symbol)
        .ok_or(RefactorError::InlineNotSupported)?;

    if inline_variable_has_writes(db, params.symbol, &def)? {
        return Err(RefactorError::InlineNotSupported);
    }

    let text = db
        .file_text(&def.file)
        .ok_or_else(|| RefactorError::UnknownFile(def.file.clone()))?;

    let parsed = parse_java(text);
    // `parse_java` may produce recoverable errors even for source we can still refactor
    // correctly (e.g. some switch/case layouts). Inline-variable only relies on a small subset of
    // the syntax tree (the declaration statement and usage sites), so proceed as long as we can
    // find the nodes we need.

    let root = parsed.syntax();

    // Variables declared in `for` headers or as try-with-resources bindings have special lifetime
    // semantics (loop-init evaluation / resource closing). Conservatively refuse to inline them.
    if let Some(declarator) = root
        .descendants()
        .filter_map(ast::VariableDeclarator::cast)
        .find(|decl| {
            decl.name_token()
                .map(|tok| syntax_token_range(&tok) == def.name_range)
                .unwrap_or(false)
        })
    {
        if declarator
            .syntax()
            .ancestors()
            .any(|n| matches!(n.kind(), SyntaxKind::ForHeader | SyntaxKind::ForInit))
        {
            return Err(RefactorError::InlineNotSupported);
        }

        if declarator.syntax().ancestors().any(|n| {
            matches!(
                n.kind(),
                SyntaxKind::ResourceSpecification | SyntaxKind::Resource
            )
        }) {
            return Err(RefactorError::InlineNotSupported);
        }
    }

    let decl = find_local_variable_declaration(&root, def.name_range)
        .ok_or(RefactorError::InlineNotSupported)?;

    let decl_stmt = decl.statement.clone();
    let init_expr = decl.initializer;
    // Array initializers (`int[] xs = {1,2};`) are not expressions in Java; they cannot be inlined
    // at arbitrary use sites.
    if matches!(init_expr, ast::Expression::ArrayInitializer(_)) {
        return Err(RefactorError::InlineNotSupported);
    }
    let init_range = syntax_range(init_expr.syntax());
    let init_text = text
        .get(init_range.start..init_range.end)
        .unwrap_or_default()
        .trim();
    if init_text.is_empty() {
        return Err(RefactorError::InlineNotSupported);
    }

    let init_has_side_effects = has_side_effects(init_expr.syntax());
    let init_replacement = parenthesize_initializer(init_text, &init_expr);

    let all_refs = db.find_references(params.symbol);

    let targets = if params.inline_all {
        all_refs.clone()
    } else {
        let Some(usage_range) = params.usage_range else {
            return Err(RefactorError::InlineNoUsageAtCursor);
        };
        let Some(reference) = all_refs
            .iter()
            .find(|r| r.range.start == usage_range.start && r.range.end == usage_range.end)
            .cloned()
        else {
            return Err(RefactorError::InlineNoUsageAtCursor);
        };
        vec![reference]
    };

    if targets.is_empty() {
        return Err(RefactorError::InlineNotSupported);
    }

    // Reject inlining across lambda execution-context boundaries. Inlining a local into a lambda
    // body (or out of it) can change evaluation timing and captured-variable semantics.
    //
    // Example:
    // ```
    // int x = 1;
    // int a = x;
    // Runnable r = () -> System.out.println(a);
    // x = 2;
    // r.run(); // prints 1
    // ```
    //
    // Inlining `a` inside the lambda would become `() -> println(x)`, printing 2 instead.
    {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct LambdaContext {
            file: FileId,
            range: TextRange,
        }

        fn lambda_context_at(
            db: &dyn RefactorDatabase,
            cache: &mut HashMap<FileId, nova_syntax::SyntaxNode>,
            file: &FileId,
            token_range: TextRange,
        ) -> Result<Option<LambdaContext>, RefactorError> {
            let root = if let Some(root) = cache.get(file) {
                root.clone()
            } else {
                let text = db
                    .file_text(file)
                    .ok_or_else(|| RefactorError::UnknownFile(file.clone()))?;
                let parsed = parse_java(text);
                let root = parsed.syntax();
                cache.insert(file.clone(), root.clone());
                root
            };

            let Some(tok) = root
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| syntax_token_range(tok) == token_range)
            else {
                return Err(RefactorError::InlineNotSupported);
            };

            let Some(parent) = tok.parent() else {
                return Err(RefactorError::InlineNotSupported);
            };

            let Some(lambda) = parent.ancestors().find_map(ast::LambdaExpression::cast) else {
                return Ok(None);
            };

            Ok(Some(LambdaContext {
                file: file.clone(),
                range: syntax_range(lambda.syntax()),
            }))
        }

        let mut cache: HashMap<FileId, nova_syntax::SyntaxNode> = HashMap::new();
        cache.insert(def.file.clone(), root.clone());

        let decl_ctx = lambda_context_at(db, &mut cache, &def.file, def.name_range)?;
        for usage in &targets {
            let usage_ctx = lambda_context_at(db, &mut cache, &usage.file, usage.range)?;
            if decl_ctx != usage_ctx {
                return Err(RefactorError::InlineNotSupported);
            }
        }
    }

    ensure_inline_variable_value_stable(
        db,
        &parsed,
        &def.file,
        decl.statement_range.end,
        &init_expr,
        &targets,
    )?;

    let mut remove_decl = params.inline_all || all_refs.len() == 1;
    if remove_decl
        && contains_unknown_name_expression(
            db,
            &def.file,
            text,
            &def.name,
            params.symbol,
            &all_refs,
        )
    {
        // If we cannot prove that our semantic reference index covers every textual occurrence of
        // the variable name, deleting the declaration can produce uncompilable code.
        //
        // When the user explicitly requested "inline all" we reject the refactoring. Otherwise we
        // fall back to keeping the declaration (still safe) even if the indexed reference set
        // makes it look like there is only one usage.
        if params.inline_all {
            return Err(RefactorError::InlineNotSupported);
        }
        remove_decl = false;
    }

    if init_has_side_effects {
        if !(remove_decl && targets.len() == 1) {
            return Err(RefactorError::InlineSideEffects);
        }

        // Prevent reordering side effects by ensuring the inlined usage statement is the
        // immediately-following statement in the same block statement list.
        check_side_effectful_inline_order(&root, &decl_stmt, &targets, &def.file)?;
    }

    let mut edits: Vec<TextEdit> = targets
        .into_iter()
        .map(|usage| TextEdit::replace(usage.file, usage.range, init_replacement.clone()))
        .collect();

    if remove_decl {
        // Delete the declaration statement. Be careful not to delete tokens that precede the
        // statement on the same line (e.g. `case 1: int a = ...;`).
        let stmt_range = decl.statement_range;
        let stmt_start = stmt_range.start;
        let stmt_end = stmt_range.end;
        let line_start = line_start(text, stmt_start);

        let decl_range = if text[line_start..stmt_start]
            .chars()
            .all(|c| c.is_whitespace())
        {
            // Statement begins at line start (only indentation precedes it). Delete indentation and
            // one trailing newline when present.
            let end = statement_end_including_trailing_newline(text, stmt_end);
            TextRange::new(line_start, end)
        } else {
            // Statement begins mid-line. Delete only the statement token range, but avoid leaving
            // awkward whitespace behind.
            let mut end = stmt_end;

            // If a newline immediately follows the statement, consume it too (preserve CRLF as a
            // unit).
            let tail = text.get(end..).unwrap_or_default();
            if tail.starts_with("\r\n") {
                end += 2;
            } else if tail.starts_with('\n') || tail.starts_with('\r') {
                end += 1;
            } else if let Some(comment_end) =
                statement_end_including_trailing_inline_comment(text, end)
            {
                // When deleting a mid-line statement (e.g. after `case 1:`), also delete any trailing
                // inline comments (`// ...` or `/* ... */`) that occur before the line break. This
                // avoids leaving a dangling comment behind after removing the statement.
                end = comment_end;
            } else if matches!(text.as_bytes().get(end), Some(b' ')) {
                // If there is a single space after the statement and another token follows on the
                // same line, delete that one space (e.g. `; System.out...`).
                let after_space = end + 1;
                if after_space < text.len() {
                    let next = text.as_bytes()[after_space];
                    if next != b'\n' && next != b'\r' && next != b' ' && next != b'\t' {
                        end = after_space;
                    }
                }
            }

            TextRange::new(stmt_start, end)
        };

        edits.push(TextEdit::delete(def.file.clone(), decl_range));
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

fn ensure_inline_variable_value_stable(
    db: &dyn RefactorDatabase,
    parsed: &nova_syntax::JavaParseResult,
    file: &FileId,
    decl_stmt_end: usize,
    initializer: &ast::Expression,
    targets: &[crate::semantic::Reference],
) -> Result<(), RefactorError> {
    let mut deps: Vec<(SymbolId, String)> = Vec::new();
    let mut seen: HashSet<SymbolId> = HashSet::new();

    // Collect locals/params referenced by the initializer.
    for tok in initializer
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
    {
        if tok.kind() != SyntaxKind::Identifier {
            continue;
        }

        let range = syntax_token_range(&tok);
        let Some(sym) = db.symbol_at(file, range.start) else {
            continue;
        };

        if !matches!(
            db.symbol_kind(sym),
            Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter)
        ) {
            continue;
        }

        if seen.insert(sym) {
            deps.push((sym, tok.text().to_string()));
        }
    }

    if deps.is_empty() {
        return Ok(());
    }

    for usage in targets {
        let usage_start = usage.range.start;
        if usage_start <= decl_stmt_end {
            continue;
        }

        for (sym, name) in &deps {
            if has_write_to_symbol_between(db, parsed, file, *sym, decl_stmt_end, usage_start)? {
                return Err(RefactorError::InlineWouldChangeValue {
                    reason: format!(
                        "`{name}` is written between the variable declaration and the inlined usage"
                    ),
                });
            }
        }
    }

    Ok(())
}

fn has_write_to_symbol_between(
    db: &dyn RefactorDatabase,
    parsed: &nova_syntax::JavaParseResult,
    file: &FileId,
    symbol: SymbolId,
    start: usize,
    end: usize,
) -> Result<bool, RefactorError> {
    if start >= end {
        return Ok(false);
    }

    for reference in db.find_references(symbol) {
        if reference.file != *file {
            continue;
        }
        if reference.range.start < start || reference.range.start >= end {
            continue;
        }

        if reference_is_write(parsed, reference.range)? {
            return Ok(true);
        }
    }

    Ok(false)
}

fn inline_variable_has_writes(
    db: &dyn RefactorDatabase,
    symbol: SymbolId,
    def: &crate::semantic::SymbolDefinition,
) -> Result<bool, RefactorError> {
    let mut parses: HashMap<FileId, nova_syntax::JavaParseResult> = HashMap::new();

    for reference in db.find_references(symbol) {
        // Some RefactorDatabase implementations may include the definition span in the reference
        // list; ignore it so we only reject on writes after the initializer.
        if reference.file == def.file && reference.range == def.name_range {
            continue;
        }

        let parsed = match parses.get(&reference.file) {
            Some(parsed) => parsed,
            None => {
                let text = db
                    .file_text(&reference.file)
                    .ok_or_else(|| RefactorError::UnknownFile(reference.file.clone()))?;
                let parsed = parse_java(text);
                parses.insert(reference.file.clone(), parsed);
                parses
                    .get(&reference.file)
                    .expect("just inserted parse result")
            }
        };

        if reference_is_write(parsed, reference.range)? {
            return Ok(true);
        }
    }

    Ok(false)
}

fn reference_is_write(
    parsed: &nova_syntax::JavaParseResult,
    range: TextRange,
) -> Result<bool, RefactorError> {
    let syntax_range = to_syntax_range(range).ok_or(RefactorError::InlineNotSupported)?;
    let element = parsed.covering_element(syntax_range);

    let node = match element {
        nova_syntax::SyntaxElement::Node(node) => node,
        nova_syntax::SyntaxElement::Token(token) => {
            token.parent().ok_or(RefactorError::InlineNotSupported)?
        }
    };

    for ancestor in node.ancestors() {
        if let Some(assign) = ast::AssignmentExpression::cast(ancestor.clone()) {
            if let Some(lhs) = assign.lhs() {
                let lhs_range = lhs.syntax().text_range();
                let start = u32::from(lhs_range.start()) as usize;
                let end = u32::from(lhs_range.end()) as usize;
                if start <= range.start && range.end <= end {
                    return Ok(true);
                }
            }
        }

        if let Some(unary) = ast::UnaryExpression::cast(ancestor) {
            if unary_is_inc_or_dec(&unary) {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn unary_is_inc_or_dec(expr: &ast::UnaryExpression) -> bool {
    let mut first: Option<SyntaxKind> = None;
    let mut last: Option<SyntaxKind> = None;

    for tok in expr
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)
    {
        let kind = tok.kind();
        if first.is_none() {
            first = Some(kind);
        }
        last = Some(kind);
    }

    matches!(first, Some(SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
        || matches!(last, Some(SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
}

fn to_syntax_range(range: TextRange) -> Option<nova_syntax::TextRange> {
    Some(nova_syntax::TextRange {
        start: u32::try_from(range.start).ok()?,
        end: u32::try_from(range.end).ok()?,
    })
}

pub struct OrganizeImportsParams {
    pub file: FileId,
}

pub fn organize_imports(
    db: &dyn RefactorDatabase,
    params: OrganizeImportsParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let text = db
        .file_text(&params.file)
        .ok_or_else(|| RefactorError::UnknownFile(params.file.clone()))?;

    let import_block = parse_import_block(text);
    if import_block.imports.is_empty() {
        return Ok(WorkspaceEdit::default());
    }

    let body_start = import_block.range.end;
    let usage = collect_identifier_usage(&text[body_start..]);
    let declared_types = collect_declared_type_names(&text[body_start..]);

    let mut normal = Vec::new();
    let mut static_imports = Vec::new();
    let mut explicit_non_static: HashSet<String> = HashSet::new();

    // First pass: filter explicit (non-wildcard) imports based on unqualified identifier usage.
    // We use unqualified identifiers so that references like `foo.Bar` or `Foo.BAR` do not keep
    // otherwise-unused imports.
    let mut wildcard_candidates = Vec::new();
    for import in &import_block.imports {
        if import.is_wildcard() {
            wildcard_candidates.push(import.clone());
            continue;
        }

        let Some(simple) = import.simple_name() else {
            continue;
        };
        if usage.unqualified.contains(simple) {
            if import.is_static {
                static_imports.push(import.render());
            } else {
                explicit_non_static.insert(simple.to_string());
                normal.push(import.render());
            }
        }
    }

    // Second pass: keep wildcard imports conservatively.
    //
    // We only drop a non-static wildcard import (`foo.bar.*`) when:
    // - there is at least one kept explicit import from the same package, and
    // - all *type-like* identifiers in the file appear to be already covered by explicit imports,
    //   declared types, or common `java.lang` names (heuristic).
    //
    // This avoids deleting `.*` imports in files that likely rely on them.
    let uncovered_type_idents = collect_uncovered_type_identifiers(
        &usage.unqualified,
        &explicit_non_static,
        &declared_types,
    );

    // Precompute whether each package has any explicit imports that survived filtering.
    let mut explicit_by_package: HashMap<String, usize> = HashMap::new();
    for import in &import_block.imports {
        if import.is_static || import.is_wildcard() {
            continue;
        }
        if let Some((pkg, _)) = import.split_package_and_name() {
            if usage
                .unqualified
                .contains(import.simple_name().unwrap_or_default())
            {
                *explicit_by_package.entry(pkg.to_string()).or_default() += 1;
            }
        }
    }

    for import in wildcard_candidates {
        if import.is_static {
            // Static wildcard imports are hard to validate heuristically because they introduce
            // unqualified method and constant names. Keep them.
            static_imports.push(import.render());
            continue;
        }

        let Some(pkg) = import.wildcard_package() else {
            normal.push(import.render());
            continue;
        };

        let has_explicit_cover = explicit_by_package.get(pkg).copied().unwrap_or(0) > 0;
        let can_remove = has_explicit_cover && uncovered_type_idents.is_empty();
        if !can_remove {
            normal.push(import.render());
        }
    }

    normal.sort();
    normal.dedup();
    static_imports.sort();
    static_imports.dedup();

    let mut out = String::new();
    for import in &normal {
        out.push_str(import);
        out.push('\n');
    }

    if !normal.is_empty() && !static_imports.is_empty() {
        out.push('\n');
    }

    for import in &static_imports {
        out.push_str(import);
        out.push('\n');
    }

    // Ensure exactly one blank line after imports when there is any body.
    // If all imports were removed, keep the original header spacing untouched.
    if body_start < text.len() && !(normal.is_empty() && static_imports.is_empty()) {
        out.push('\n');
    }

    // If the computed block is identical, return an empty edit to reduce churn.
    let original_block = &text[import_block.range.start..import_block.range.end];
    if original_block == out {
        return Ok(WorkspaceEdit::default());
    }

    let mut edits = Vec::new();
    edits.push(TextEdit::replace(
        params.file.clone(),
        import_block.range,
        out,
    ));

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

fn trim_range(text: &str, mut range: TextRange) -> TextRange {
    let bytes = text.as_bytes();
    while range.start < range.end && bytes[range.start].is_ascii_whitespace() {
        range.start += 1;
    }
    while range.start < range.end && bytes[range.end - 1].is_ascii_whitespace() {
        range.end -= 1;
    }
    range
}

fn syntax_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

const EXTRACT_VARIABLE_NAME_CONFLICT_REASON: &str =
    "extracted variable name conflicts with an existing binding";

fn check_extract_variable_name_conflicts(
    stmt: &ast::Statement,
    insert_pos: usize,
    name: &str,
) -> Result<(), RefactorError> {
    // The extracted variable's declaration is inserted at `insert_pos` before `stmt`. We need a
    // conservative scope range for name collision checks.
    //
    // Prefer the *nearest* statement-list container:
    // - a `{ ... }` block (local variable scope is the block)
    // - otherwise the surrounding switch block (traditional `case:` groups share switch scope)
    let Some(insertion_scope) = stmt.syntax().ancestors().find_map(|node| {
        if let Some(block) = ast::Block::cast(node.clone()) {
            Some(syntax_range(block.syntax()))
        } else {
            ast::SwitchBlock::cast(node).map(|b| syntax_range(b.syntax()))
        }
    }) else {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot determine scope for extracted variable",
        });
    };

    let new_scope = TextRange::new(insert_pos, insertion_scope.end);

    let Some(enclosing) = find_enclosing_body_owner(stmt) else {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot determine enclosing method/initializer body for extraction",
        });
    };

    // Method/constructor parameters.
    if enclosing.has_parameter_named(name) {
        return Err(RefactorError::ExtractNotSupported {
            reason: EXTRACT_VARIABLE_NAME_CONFLICT_REASON,
        });
    }

    // Local variable declarators.
    for decl in enclosing
        .body()
        .syntax()
        .descendants()
        .filter_map(ast::VariableDeclarator::cast)
    {
        if is_within_nested_type(decl.syntax(), enclosing.body().syntax()) {
            continue;
        }
        let Some(tok) = decl.name_token() else {
            continue;
        };
        if tok.text() != name {
            continue;
        }
        let Some(scope) = local_binding_scope_range(&decl) else {
            continue;
        };
        if ranges_overlap(new_scope, scope) {
            return Err(RefactorError::ExtractNotSupported {
                reason: EXTRACT_VARIABLE_NAME_CONFLICT_REASON,
            });
        }
    }

    // Catch parameters.
    for catch_clause in enclosing
        .body()
        .syntax()
        .descendants()
        .filter_map(ast::CatchClause::cast)
    {
        if is_within_nested_type(catch_clause.syntax(), enclosing.body().syntax()) {
            continue;
        }
        let Some(body) = catch_clause.body() else {
            continue;
        };
        let Some(param_name) = catch_parameter_name(&catch_clause) else {
            continue;
        };
        if param_name != name {
            continue;
        }
        let scope = syntax_range(body.syntax());
        if ranges_overlap(new_scope, scope) {
            return Err(RefactorError::ExtractNotSupported {
                reason: EXTRACT_VARIABLE_NAME_CONFLICT_REASON,
            });
        }
    }

    // Lambda parameters.
    for lambda in enclosing
        .body()
        .syntax()
        .descendants()
        .filter_map(ast::LambdaExpression::cast)
    {
        if is_within_nested_type(lambda.syntax(), enclosing.body().syntax()) {
            continue;
        }

        let Some(body) = lambda.body() else {
            continue;
        };
        let lambda_scope = if let Some(block) = body.block() {
            syntax_range(block.syntax())
        } else if let Some(expr) = body.expression() {
            syntax_range(expr.syntax())
        } else {
            continue;
        };
        if !ranges_overlap(new_scope, lambda_scope) {
            continue;
        }

        let Some(params) = lambda.parameters() else {
            continue;
        };

        let mut has_conflict = false;
        if let Some(list) = params.parameter_list() {
            has_conflict = list
                .parameters()
                .filter_map(|p| p.name_token().map(|t| t.text().to_string()))
                .any(|n| n == name);
        } else if let Some(param) = params.parameter() {
            has_conflict = param.name_token().is_some_and(|t| t.text() == name);
        }

        if has_conflict {
            return Err(RefactorError::ExtractNotSupported {
                reason: EXTRACT_VARIABLE_NAME_CONFLICT_REASON,
            });
        }
    }

    Ok(())
}

fn ranges_overlap(a: TextRange, b: TextRange) -> bool {
    a.start < b.end && b.start < a.end
}

#[derive(Clone, Debug)]
enum EnclosingBodyOwner {
    Method(ast::MethodDeclaration, ast::Block),
    Constructor(ast::ConstructorDeclaration, ast::Block),
    CompactConstructor(ast::CompactConstructorDeclaration, ast::Block),
    Initializer(ast::InitializerBlock, ast::Block),
}

impl EnclosingBodyOwner {
    fn body(&self) -> &ast::Block {
        match self {
            EnclosingBodyOwner::Method(_, body)
            | EnclosingBodyOwner::Constructor(_, body)
            | EnclosingBodyOwner::CompactConstructor(_, body)
            | EnclosingBodyOwner::Initializer(_, body) => body,
        }
    }

    fn has_parameter_named(&self, name: &str) -> bool {
        match self {
            EnclosingBodyOwner::Method(method, _) => method.parameter_list().is_some_and(|list| {
                list.parameters()
                    .any(|p| p.name_token().is_some_and(|t| t.text() == name))
            }),
            EnclosingBodyOwner::Constructor(ctor, _) => ctor.parameter_list().is_some_and(|list| {
                list.parameters()
                    .any(|p| p.name_token().is_some_and(|t| t.text() == name))
            }),
            EnclosingBodyOwner::CompactConstructor(_, _)
            | EnclosingBodyOwner::Initializer(_, _) => false,
        }
    }
}

fn find_enclosing_body_owner(stmt: &ast::Statement) -> Option<EnclosingBodyOwner> {
    for node in stmt.syntax().ancestors() {
        if let Some(method) = ast::MethodDeclaration::cast(node.clone()) {
            let body = method.body()?;
            return Some(EnclosingBodyOwner::Method(method, body));
        }
        if let Some(ctor) = ast::ConstructorDeclaration::cast(node.clone()) {
            let body = ctor.body()?;
            return Some(EnclosingBodyOwner::Constructor(ctor, body));
        }
        if let Some(ctor) = ast::CompactConstructorDeclaration::cast(node.clone()) {
            let body = ctor.body()?;
            return Some(EnclosingBodyOwner::CompactConstructor(ctor, body));
        }
        if let Some(init) = ast::InitializerBlock::cast(node) {
            let body = init.body()?;
            return Some(EnclosingBodyOwner::Initializer(init, body));
        }
    }
    None
}

fn is_within_nested_type(
    node: &nova_syntax::SyntaxNode,
    stop_at: &nova_syntax::SyntaxNode,
) -> bool {
    for anc in node.ancestors() {
        if &anc == stop_at {
            break;
        }
        if ast::ClassDeclaration::can_cast(anc.kind())
            || ast::InterfaceDeclaration::can_cast(anc.kind())
            || ast::EnumDeclaration::can_cast(anc.kind())
            || ast::RecordDeclaration::can_cast(anc.kind())
            || ast::AnnotationTypeDeclaration::can_cast(anc.kind())
            // Anonymous classes have a `ClassBody` without a `ClassDeclaration` wrapper.
            || ast::ClassBody::can_cast(anc.kind())
            || ast::InterfaceBody::can_cast(anc.kind())
            || ast::EnumBody::can_cast(anc.kind())
            || ast::RecordBody::can_cast(anc.kind())
        {
            return true;
        }
    }
    false
}

fn catch_parameter_name(catch_clause: &ast::CatchClause) -> Option<String> {
    let mut last_ident: Option<String> = None;
    for el in catch_clause.syntax().descendants_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        if tok.kind() == SyntaxKind::RParen {
            break;
        }
        if tok.kind().is_identifier_like() {
            last_ident = Some(tok.text().to_string());
        }
    }
    last_ident
}

fn local_binding_scope_range(decl: &ast::VariableDeclarator) -> Option<TextRange> {
    let decl_range = syntax_range(decl.syntax());

    for for_stmt in decl
        .syntax()
        .ancestors()
        .filter_map(ast::ForStatement::cast)
    {
        let header = for_stmt.header()?;
        let header_range = syntax_range(header.syntax());
        if contains_range(header_range, decl_range) {
            return Some(syntax_range(for_stmt.syntax()));
        }
    }

    for try_stmt in decl
        .syntax()
        .ancestors()
        .filter_map(ast::TryStatement::cast)
    {
        let resources = try_stmt.resources()?;
        let resources_range = syntax_range(resources.syntax());
        if contains_range(resources_range, decl_range) {
            return Some(syntax_range(try_stmt.syntax()));
        }
    }

    // Default: nearest enclosing block-like scope.
    if let Some(block) = decl.syntax().ancestors().find_map(ast::Block::cast) {
        return Some(syntax_range(block.syntax()));
    }
    if let Some(block) = decl.syntax().ancestors().find_map(ast::SwitchBlock::cast) {
        return Some(syntax_range(block.syntax()));
    }

    None
}

fn find_expression(
    source: &str,
    root: nova_syntax::SyntaxNode,
    selection: TextRange,
) -> Option<ast::Expression> {
    for expr in root.descendants().filter_map(ast::Expression::cast) {
        // The Java AST may include trivia (whitespace/comments) in node ranges,
        // so compare against a trimmed version to keep selection matching stable
        // even when the user includes incidental whitespace.
        let range = trim_range(source, syntax_range(expr.syntax()));
        if range.start == selection.start && range.end == selection.end {
            return Some(expr);
        }
    }
    None
}

fn rewrite_multi_declarator_local_variable_declaration(
    source: &str,
    stmt: &ast::LocalVariableDeclarationStatement,
    stmt_range: TextRange,
    expr_range: TextRange,
    expr_text: &str,
    extracted_name: &str,
    extracted_ty: &str,
    indent: &str,
    newline: &str,
) -> Option<String> {
    let list = stmt.declarator_list()?;
    let decls: Vec<_> = list.declarators().collect();
    if decls.len() <= 1 {
        return None;
    }

    let mut target_idx: Option<usize> = None;
    for (idx, decl) in decls.iter().enumerate() {
        let Some(init) = decl.initializer() else {
            continue;
        };
        let init_range = syntax_range(init.syntax());
        if init_range.start <= expr_range.start && expr_range.end <= init_range.end {
            target_idx = Some(idx);
            break;
        }
    }
    let target_idx = target_idx?;
    if target_idx == 0 {
        return None;
    }

    let first_decl = decls.first()?;
    let prev_decl = decls.get(target_idx - 1)?;
    let target_decl = decls.get(target_idx)?;
    let last_decl = decls.last()?;

    let first_decl_range = syntax_range(first_decl.syntax());
    let prev_decl_range = syntax_range(prev_decl.syntax());
    let target_decl_range = syntax_range(target_decl.syntax());
    let last_decl_range = syntax_range(last_decl.syntax());

    // The declarator node may include separator whitespace after the comma. When we split the
    // declaration into a new statement we want the declarator to start at the identifier/pattern.
    let after_start = skip_leading_whitespace(source, target_decl_range.start, last_decl_range.end);
    if expr_range.start < after_start || expr_range.end > last_decl_range.end {
        return None;
    }

    let prefix_text = source
        .get(stmt_range.start..first_decl_range.start)?
        .to_string();
    let before_text = source
        .get(first_decl_range.start..prev_decl_range.end)?
        .to_string();
    let after_text = source.get(after_start..last_decl_range.end)?.to_string();
    let stmt_suffix = source.get(last_decl_range.end..stmt_range.end)?.to_string();

    let rel_start = expr_range.start - after_start;
    let rel_end = expr_range.end - after_start;
    let after_replaced = format!(
        "{}{}{}",
        &after_text[..rel_start],
        extracted_name,
        &after_text[rel_end..]
    );

    let mut replacement = String::new();
    replacement.push_str(&prefix_text);
    replacement.push_str(&before_text);
    replacement.push(';');
    replacement.push_str(newline);
    replacement.push_str(indent);
    replacement.push_str(extracted_ty);
    replacement.push(' ');
    replacement.push_str(extracted_name);
    replacement.push_str(" = ");
    replacement.push_str(expr_text);
    replacement.push(';');
    replacement.push_str(newline);
    replacement.push_str(indent);
    replacement.push_str(&prefix_text);
    replacement.push_str(&after_replaced);
    replacement.push_str(&stmt_suffix);

    Some(replacement)
}

fn skip_leading_whitespace(text: &str, mut start: usize, end: usize) -> usize {
    let bytes = text.as_bytes();
    while start < end
        && bytes
            .get(start)
            .copied()
            .is_some_and(|b| b.is_ascii_whitespace())
    {
        start += 1;
    }
    start
}

fn normalize_expr_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn find_replace_all_occurrences_same_execution_context(
    source: &str,
    root: nova_syntax::SyntaxNode,
    insertion_stmt: &ast::Statement,
    selected_text: &str,
) -> Vec<TextRange> {
    let selected_norm = normalize_expr_text(selected_text);
    if selected_norm.is_empty() {
        return Vec::new();
    }

    // Execution context owner: the nearest enclosing lambda (if any).
    let insertion_lambda = insertion_stmt
        .syntax()
        .ancestors()
        .find_map(ast::LambdaExpression::cast);

    // Restrict to the closest enclosing block to avoid replacing occurrences in other methods.
    let search_root = insertion_stmt
        .syntax()
        .ancestors()
        .find_map(ast::Block::cast)
        .map(|b| b.syntax().clone())
        .unwrap_or(root);

    let min_offset = syntax_range(insertion_stmt.syntax()).start;

    let mut ranges = Vec::new();
    for expr in search_root.descendants().filter_map(ast::Expression::cast) {
        let range = syntax_range(expr.syntax());

        // The extracted local is declared immediately before `insertion_stmt`, so we only replace
        // occurrences within that statement and after it.
        if range.start < min_offset {
            continue;
        }

        // Compare against a trimmed version so trailing trivia in expression node ranges does not
        // affect equivalence matching.
        let trimmed = trim_range(source, range);
        let Some(text) = source.get(trimmed.start..trimmed.end) else {
            continue;
        };
        if normalize_expr_text(text) != selected_norm {
            continue;
        }

        let expr_lambda = expr
            .syntax()
            .ancestors()
            .find_map(ast::LambdaExpression::cast);
        if expr_lambda != insertion_lambda {
            continue;
        }

        ranges.push(range);
    }

    ranges.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));
    ranges.dedup();
    ranges
}

fn constant_expression_only_context_reason(expr: &ast::Expression) -> Option<&'static str> {
    for node in expr.syntax().ancestors() {
        if ast::AnnotationElementValue::cast(node.clone()).is_some() {
            return Some(
                "cannot extract from annotation element values (compile-time constant required)",
            );
        }

        if ast::CaseLabelElement::cast(node.clone()).is_some()
            || ast::SwitchLabel::cast(node).is_some()
        {
            return Some("cannot extract from switch case labels (compile-time constant required)");
        }
    }

    None
}

fn extract_variable_crosses_execution_boundary(expr: &ast::Expression) -> Option<&'static str> {
    let expr_range = syntax_range(expr.syntax());

    // Walk up the syntax tree; if we cross into a lambda/switch execution context that cannot
    // contain an inserted statement (without additional wrapping conversions), reject the
    // refactoring.
    for node in expr.syntax().ancestors() {
        if let Some(lambda) = ast::LambdaExpression::cast(node.clone()) {
            if lambda.body().and_then(|body| body.expression()).is_some() {
                return Some("cannot extract from expression-bodied lambda");
            }
        }

        let Some(rule) = ast::SwitchRule::cast(node) else {
            continue;
        };
        let Some(body) = rule.body() else {
            continue;
        };
        if matches!(body, ast::SwitchRuleBody::Block(_)) {
            continue;
        }

        // Only guard when the selection is inside the rule body (not the labels/guard).
        let body_range = syntax_range(body.syntax());
        if !(body_range.start <= expr_range.start && expr_range.end <= body_range.end) {
            continue;
        }

        // Reject when inside a switch *expression* rule body that is not a block, since extracting
        // would either lift evaluation out of the selected case arm or require block/yield
        // conversion (not implemented yet).
        let container = rule.syntax().ancestors().skip(1).find_map(|node| {
            if ast::SwitchExpression::cast(node.clone()).is_some() {
                Some(true)
            } else if ast::SwitchStatement::cast(node).is_some() {
                Some(false)
            } else {
                None
            }
        });
        if container == Some(true) {
            return Some("cannot extract from switch expression rule body");
        }
    }

    None
}

fn infer_expr_type(expr: &ast::Expression) -> String {
    let inferred = match expr {
        ast::Expression::LiteralExpression(lit) => infer_type_from_literal(lit),
        ast::Expression::NewExpression(new_expr) => new_expr
            .ty()
            .map(|ty| render_java_type(ty.syntax()))
            .unwrap_or_else(|| "Object".to_string()),
        ast::Expression::ArrayCreationExpression(array_expr) => {
            let Some(base_ty) = array_expr.ty() else {
                return "Object".to_string();
            };
            let base = render_java_type(base_ty.syntax());

            let mut dims = 0usize;
            if let Some(dim_exprs) = array_expr.dim_exprs() {
                dims += dim_exprs.dims().count();
            }
            if let Some(dims_node) = array_expr.dims() {
                dims += dims_node.dims().count();
            }

            if dims == 0 {
                base
            } else {
                format!("{base}{}", "[]".repeat(dims))
            }
        }
        ast::Expression::CastExpression(cast) => cast
            .ty()
            .map(|ty| render_java_type(ty.syntax()))
            .unwrap_or_else(|| "Object".to_string()),
        ast::Expression::ConditionalExpression(cond) => {
            let Some(then_branch) = cond.then_branch() else {
                return "Object".to_string();
            };
            let Some(else_branch) = cond.else_branch() else {
                return "Object".to_string();
            };

            let then_ty = infer_expr_type(&then_branch);
            let else_ty = infer_expr_type(&else_branch);
            if then_ty == else_ty {
                then_ty
            } else {
                "Object".to_string()
            }
        }
        ast::Expression::ParenthesizedExpression(expr) => expr
            .expression()
            .map(|inner| infer_expr_type(&inner))
            .unwrap_or_else(|| "Object".to_string()),
        ast::Expression::InstanceofExpression(_) => "boolean".to_string(),
        ast::Expression::UnaryExpression(unary) => infer_type_from_unary_expr(unary),
        ast::Expression::BinaryExpression(binary) => infer_type_from_binary_expr(binary),
        ast::Expression::ThisExpression(_)
        | ast::Expression::SuperExpression(_)
        | ast::Expression::NameExpression(_)
        | ast::Expression::ArrayInitializer(_) => "Object".to_string(),
        _ => "Object".to_string(),
    };

    // If we couldn't infer a meaningful type from the expression itself, try a cheap contextual
    // fallback: use the declared type of the variable/field whose initializer contains this
    // expression (when available).
    //
    // This is still parser-only and helps common cases like extracting `null`, `this`, or unknown
    // call expressions from `String x = ...`.
    if inferred == "Object" {
        if let Some(ctx) = infer_type_from_enclosing_declaration(expr) {
            return ctx;
        }
    }

    inferred
}

fn infer_type_from_enclosing_declaration(expr: &ast::Expression) -> Option<String> {
    let expr_range = syntax_range(expr.syntax());

    // Walk up to the nearest variable declarator and check if `expr` is within its initializer.
    for node in expr.syntax().ancestors() {
        let Some(declarator) = ast::VariableDeclarator::cast(node.clone()) else {
            continue;
        };
        let Some(initializer) = declarator.initializer() else {
            continue;
        };

        let init_range = syntax_range(initializer.syntax());
        if !(init_range.start <= expr_range.start && expr_range.end <= init_range.end) {
            continue;
        }

        // Prefer the closest declaration site.
        for ancestor in declarator.syntax().ancestors() {
            if let Some(local) = ast::LocalVariableDeclarationStatement::cast(ancestor.clone()) {
                let ty = local.ty()?;
                let rendered = render_java_type(ty.syntax());
                if rendered == "var" {
                    return None;
                }
                return Some(rendered);
            }

            if let Some(field) = ast::FieldDeclaration::cast(ancestor) {
                let ty = field.ty()?;
                return Some(render_java_type(ty.syntax()));
            }
        }
    }

    None
}

fn first_non_trivia_child_token_kind(node: &nova_syntax::SyntaxNode) -> Option<SyntaxKind> {
    node.children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)
        .map(|tok| tok.kind())
}

fn numeric_rank(ty: &str) -> Option<u8> {
    match ty {
        "double" => Some(4),
        "float" => Some(3),
        "long" => Some(2),
        "int" | "char" => Some(1),
        _ => None,
    }
}

fn numeric_type_for_rank(rank: u8) -> &'static str {
    match rank {
        4 => "double",
        3 => "float",
        2 => "long",
        _ => "int",
    }
}

fn integral_rank(ty: &str) -> Option<u8> {
    match ty {
        "long" => Some(2),
        "int" | "char" => Some(1),
        _ => None,
    }
}

fn integral_type_for_rank(rank: u8) -> &'static str {
    match rank {
        2 => "long",
        _ => "int",
    }
}

fn infer_type_from_unary_expr(unary: &ast::UnaryExpression) -> String {
    let Some(op) = first_non_trivia_child_token_kind(unary.syntax()) else {
        return "Object".to_string();
    };

    match op {
        SyntaxKind::Bang => "boolean".to_string(),
        SyntaxKind::Plus | SyntaxKind::Minus => {
            let Some(operand) = unary.operand() else {
                return "Object".to_string();
            };
            let operand_ty = infer_expr_type(&operand);
            let Some(rank) = numeric_rank(&operand_ty) else {
                return "Object".to_string();
            };
            numeric_type_for_rank(rank).to_string()
        }
        SyntaxKind::Tilde => {
            let Some(operand) = unary.operand() else {
                return "Object".to_string();
            };
            let operand_ty = infer_expr_type(&operand);
            let Some(rank) = integral_rank(&operand_ty) else {
                return "Object".to_string();
            };
            integral_type_for_rank(rank).to_string()
        }
        // We don't know the operand type without typeck, and returning an incorrect primitive type
        // here would make the extracted code not compile. Default to Object (boxing).
        SyntaxKind::PlusPlus | SyntaxKind::MinusMinus => "Object".to_string(),
        _ => "Object".to_string(),
    }
}

fn infer_type_from_binary_expr(binary: &ast::BinaryExpression) -> String {
    let Some(op) = first_non_trivia_child_token_kind(binary.syntax()) else {
        return "Object".to_string();
    };

    match op {
        SyntaxKind::Less
        | SyntaxKind::LessEq
        | SyntaxKind::Greater
        | SyntaxKind::GreaterEq
        | SyntaxKind::EqEq
        | SyntaxKind::BangEq
        | SyntaxKind::AmpAmp
        | SyntaxKind::PipePipe => return "boolean".to_string(),
        _ => {}
    }

    let lhs_ty = binary.lhs().map(|lhs| infer_expr_type(&lhs));
    let rhs_ty = binary.rhs().map(|rhs| infer_expr_type(&rhs));

    match op {
        SyntaxKind::Plus => {
            // The parser doesn't know if a name refers to a `String`, so we fall back to checking
            // for a string literal somewhere in the expression. We also respect cases like
            // `new String()`/casts.
            if syntax_contains_string_literal(binary.syntax())
                || lhs_ty.as_deref() == Some("String")
                || rhs_ty.as_deref() == Some("String")
            {
                return "String".to_string();
            }

            let (Some(lhs_ty), Some(rhs_ty)) = (lhs_ty, rhs_ty) else {
                return "Object".to_string();
            };
            let (Some(lhs_rank), Some(rhs_rank)) = (numeric_rank(&lhs_ty), numeric_rank(&rhs_ty))
            else {
                return "Object".to_string();
            };
            numeric_type_for_rank(lhs_rank.max(rhs_rank)).to_string()
        }
        SyntaxKind::Minus | SyntaxKind::Star | SyntaxKind::Slash | SyntaxKind::Percent => {
            let (Some(lhs_ty), Some(rhs_ty)) = (lhs_ty, rhs_ty) else {
                return "Object".to_string();
            };
            let (Some(lhs_rank), Some(rhs_rank)) = (numeric_rank(&lhs_ty), numeric_rank(&rhs_ty))
            else {
                return "Object".to_string();
            };
            numeric_type_for_rank(lhs_rank.max(rhs_rank)).to_string()
        }
        SyntaxKind::LeftShift | SyntaxKind::RightShift | SyntaxKind::UnsignedRightShift => {
            let Some(lhs_ty) = lhs_ty else {
                return "Object".to_string();
            };
            let Some(rank) = integral_rank(&lhs_ty) else {
                return "Object".to_string();
            };
            integral_type_for_rank(rank).to_string()
        }
        SyntaxKind::Amp | SyntaxKind::Pipe | SyntaxKind::Caret => {
            let (Some(lhs_ty), Some(rhs_ty)) = (lhs_ty, rhs_ty) else {
                return "Object".to_string();
            };

            if lhs_ty == "boolean" && rhs_ty == "boolean" {
                return "boolean".to_string();
            }

            let (Some(lhs_rank), Some(rhs_rank)) = (integral_rank(&lhs_ty), integral_rank(&rhs_ty))
            else {
                return "Object".to_string();
            };
            integral_type_for_rank(lhs_rank.max(rhs_rank)).to_string()
        }
        _ => "Object".to_string(),
    }
}

fn infer_type_from_literal(lit: &ast::LiteralExpression) -> String {
    let tok = lit
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof);
    let Some(tok) = tok else {
        return "Object".to_string();
    };

    match tok.kind() {
        SyntaxKind::IntLiteral => "int".to_string(),
        SyntaxKind::LongLiteral => "long".to_string(),
        SyntaxKind::FloatLiteral => "float".to_string(),
        SyntaxKind::DoubleLiteral => "double".to_string(),
        SyntaxKind::CharLiteral => "char".to_string(),
        SyntaxKind::StringLiteral | SyntaxKind::TextBlock => "String".to_string(),
        SyntaxKind::TrueKw | SyntaxKind::FalseKw => "boolean".to_string(),
        SyntaxKind::NullKw => "Object".to_string(),
        _ => "Object".to_string(),
    }
}

fn syntax_contains_string_literal(node: &nova_syntax::SyntaxNode) -> bool {
    node.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|tok| {
            matches!(
                tok.kind(),
                SyntaxKind::StringLiteral | SyntaxKind::TextBlock
            )
        })
}

fn render_java_type(node: &nova_syntax::SyntaxNode) -> String {
    // We want Java-source-like but stable output. We therefore drop trivia and insert spaces only
    // when necessary for the token stream to remain readable/valid.
    let mut out = String::new();
    let mut prev_kind: Option<SyntaxKind> = None;
    let mut prev_was_word = false;

    for tok in node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
    {
        let kind = tok.kind();
        if kind.is_trivia() || kind == SyntaxKind::Eof {
            continue;
        }

        let is_word = kind.is_keyword() || kind.is_identifier_like();
        let needs_space = !out.is_empty()
            && ((prev_was_word && is_word)
                || (prev_kind == Some(SyntaxKind::Question) && is_word)
                || (kind == SyntaxKind::At
                    && (prev_was_word || prev_kind == Some(SyntaxKind::RBracket))));

        if needs_space {
            out.push(' ');
        }
        out.push_str(tok.text());
        prev_kind = Some(kind);
        prev_was_word = is_word;
    }

    if out.is_empty() {
        "Object".to_string()
    } else {
        out
    }
}

fn current_indent(text: &str, line_start: usize) -> String {
    let line = &text[line_start..];
    let mut indent = String::new();
    for ch in line.chars() {
        if ch == ' ' || ch == '\t' {
            indent.push(ch);
        } else {
            break;
        }
    }
    indent
}

fn best_type_at_range_display(
    db: &dyn RefactorDatabase,
    file: &FileId,
    text: &str,
    range: TextRange,
) -> Option<String> {
    for offset in type_at_range_offset_candidates(text, range) {
        let Some(ty) = db.type_at_offset_display(file, offset) else {
            continue;
        };
        let ty = ty.trim();
        // Filter out Nova-specific placeholders and non-denotable types.
        // These are not valid Java types for variable declarations.
        if ty.is_empty()
            || ty == "<?>"
            || ty == "<error>"
            || ty.eq_ignore_ascii_case("null")
            || ty == "void"
            || ty.starts_with('<')
        {
            continue;
        }
        return Some(ty.to_string());
    }
    None
}

fn strip_leading_qualifiers(ty: &str) -> String {
    // Our typeck display strings drop package names for readability, but they may still include
    // qualification via enclosing types (e.g. `Test.Foo`). For explicit local variable types we
    // prefer minimally qualified names when possible, so provide a cheap "strip outer qualifiers"
    // helper for comparison purposes.
    //
    // Examples:
    // - `Test.Foo` -> `Foo`
    // - `java.util.List<String>` -> `List<String>` (keep generics; drop package/outer prefixes)
    let cut = ty.find(|c| c == '<' || c == '[').unwrap_or(ty.len());
    let (head, tail) = ty.split_at(cut);
    let stripped = head.rsplit_once('.').map(|(_, tail)| tail).unwrap_or(head);
    format!("{stripped}{tail}")
}

fn type_at_range_offset_candidates(text: &str, range: TextRange) -> Vec<usize> {
    let bytes = text.as_bytes();
    if range.start >= range.end || range.start >= bytes.len() {
        return Vec::new();
    }

    let mut start = range.start;
    let mut end = range.end.min(bytes.len());
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }

    if start >= end {
        return Vec::new();
    }

    let mut candidates: Vec<(usize, u8, usize)> = Vec::new();
    let mut depth = 0usize;
    for i in start..end {
        let b = bytes[i];
        if !b.is_ascii_whitespace() && !is_java_ident_byte(b) {
            candidates.push((i, b, depth));
        }

        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    let mut offsets: Vec<usize> = Vec::new();
    if let Some(best) = pick_best_punctuation_offset(&candidates) {
        offsets.push(best);
    }
    offsets.push(start);
    if start + 1 < end {
        offsets.push(start + 1);
    }
    offsets.push(end.saturating_sub(1));

    // De-dup while preserving order.
    let mut seen: HashSet<usize> = HashSet::new();
    offsets.retain(|o| seen.insert(*o));
    offsets
}

fn pick_best_punctuation_offset(candidates: &[(usize, u8, usize)]) -> Option<usize> {
    let min_depth = candidates.iter().map(|(_, _, depth)| *depth).min()?;

    let mut last_any: Option<usize> = None;
    let mut last_non_open: Option<usize> = None;

    for &(idx, b, depth) in candidates {
        if depth != min_depth {
            continue;
        }

        last_any = Some(idx);
        if !matches!(b, b'(' | b'[' | b'{') {
            last_non_open = Some(idx);
        }
    }

    last_non_open.or(last_any)
}

fn is_java_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

#[derive(Debug)]
struct LocalVarDeclInfo {
    statement: ast::LocalVariableDeclarationStatement,
    statement_range: TextRange,
    initializer: ast::Expression,
}

fn find_local_variable_declaration(
    root: &nova_syntax::SyntaxNode,
    name_range: TextRange,
) -> Option<LocalVarDeclInfo> {
    for stmt in root
        .descendants()
        .filter_map(ast::LocalVariableDeclarationStatement::cast)
    {
        let list = stmt.declarator_list()?;
        let declarators: Vec<_> = list.declarators().collect();

        let matches_symbol = declarators.iter().any(|decl| {
            decl.name_token()
                .map(|tok| syntax_token_range(&tok) == name_range)
                .unwrap_or(false)
        });
        if !matches_symbol {
            continue;
        }

        // Reject multi-declarator statements until we properly rewrite them.
        if declarators.len() != 1 {
            return None;
        }

        let decl = declarators.into_iter().next()?;
        let initializer = decl.initializer()?;
        let statement_range = syntax_range(stmt.syntax());

        return Some(LocalVarDeclInfo {
            statement: stmt,
            statement_range,
            initializer,
        });
    }
    None
}

fn syntax_token_range(tok: &nova_syntax::SyntaxToken) -> TextRange {
    let range = tok.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn reject_unsafe_extract_variable_context(
    expr: &ast::Expression,
    enclosing_stmt: &ast::Statement,
) -> Result<(), RefactorError> {
    let expr_range = syntax_range(expr.syntax());
    let enclosing_stmt_syntax = enclosing_stmt.syntax().clone();

    for ancestor in expr.syntax().ancestors() {
        if let Some(while_stmt) = ast::WhileStatement::cast(ancestor.clone()) {
            if let Some(cond) = while_stmt.condition() {
                if contains_range(syntax_range(cond.syntax()), expr_range) {
                    return Err(RefactorError::ExtractNotSupported {
                        reason: "cannot extract from while condition",
                    });
                }
            }
        }

        if let Some(do_while) = ast::DoWhileStatement::cast(ancestor.clone()) {
            if let Some(cond) = do_while.condition() {
                if contains_range(syntax_range(cond.syntax()), expr_range) {
                    return Err(RefactorError::ExtractNotSupported {
                        reason: "cannot extract from do-while condition",
                    });
                }
            }
        }

        if let Some(for_stmt) = ast::ForStatement::cast(ancestor.clone()) {
            if let Some(header) = for_stmt.header() {
                if for_header_has_unsafe_eval_context(&header, expr_range) {
                    return Err(RefactorError::ExtractNotSupported {
                        reason: "cannot extract from for-loop condition or update",
                    });
                }
            }
        }

        if let Some(binary) = ast::BinaryExpression::cast(ancestor.clone()) {
            if let Some(op) = binary_short_circuit_operator_kind(&binary) {
                if matches!(op, SyntaxKind::AmpAmp | SyntaxKind::PipePipe) {
                    if let Some(rhs) = binary.rhs() {
                        if contains_range(syntax_range(rhs.syntax()), expr_range) {
                            return Err(RefactorError::ExtractNotSupported {
                                reason: "cannot extract from right-hand side of `&&` / `||`",
                            });
                        }
                    }
                }
            }
        }

        if let Some(cond_expr) = ast::ConditionalExpression::cast(ancestor.clone()) {
            let cond_range = syntax_range(cond_expr.syntax());
            // Allow extracting the whole conditional expression.
            if cond_range != expr_range {
                if let Some(then_branch) = cond_expr.then_branch() {
                    if contains_range(syntax_range(then_branch.syntax()), expr_range) {
                        return Err(RefactorError::ExtractNotSupported {
                            reason: "cannot extract from conditional (`?:`) branch",
                        });
                    }
                }
                if let Some(else_branch) = cond_expr.else_branch() {
                    if contains_range(syntax_range(else_branch.syntax()), expr_range) {
                        return Err(RefactorError::ExtractNotSupported {
                            reason: "cannot extract from conditional (`?:`) branch",
                        });
                    }
                }
            }
        }

        if ancestor == enclosing_stmt_syntax {
            break;
        }
    }

    Ok(())
}

fn contains_range(outer: TextRange, inner: TextRange) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

fn for_header_has_unsafe_eval_context(header: &ast::ForHeader, expr_range: TextRange) -> bool {
    let mut semicolons = Vec::new();
    let mut r_paren = None;

    for el in header.syntax().children_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        match tok.kind() {
            SyntaxKind::Semicolon => semicolons.push(tok),
            SyntaxKind::RParen => r_paren = Some(tok),
            _ => {}
        }
    }

    // Classic for loops always have two semicolons in the header.
    if semicolons.len() < 2 {
        return false;
    }
    let Some(r_paren) = r_paren else {
        return false;
    };

    let first_semi = syntax_token_range(&semicolons[0]);
    let second_semi = syntax_token_range(&semicolons[1]);
    let r_paren = syntax_token_range(&r_paren);

    let condition_segment = TextRange::new(first_semi.end, second_semi.start);
    let update_segment = TextRange::new(second_semi.end, r_paren.start);

    contains_range(condition_segment, expr_range) || contains_range(update_segment, expr_range)
}

fn binary_short_circuit_operator_kind(binary: &ast::BinaryExpression) -> Option<SyntaxKind> {
    let lhs = binary.lhs()?;
    let rhs = binary.rhs()?;

    let lhs = lhs.syntax().clone();
    let rhs = rhs.syntax().clone();
    let mut seen_lhs = false;
    for el in binary.syntax().children_with_tokens() {
        match el {
            nova_syntax::SyntaxElement::Node(node) => {
                if node == lhs {
                    seen_lhs = true;
                    continue;
                }
                if node == rhs {
                    break;
                }
            }
            nova_syntax::SyntaxElement::Token(tok) => {
                if !seen_lhs {
                    continue;
                }
                if tok.kind().is_trivia() {
                    continue;
                }
                return Some(tok.kind());
            }
        }
    }

    None
}

fn has_side_effects(expr: &nova_syntax::SyntaxNode) -> bool {
    fn node_has_side_effects(node: &nova_syntax::SyntaxNode) -> bool {
        match node.kind() {
            SyntaxKind::MethodCallExpression
            | SyntaxKind::NewExpression
            | SyntaxKind::AssignmentExpression
            | SyntaxKind::LambdaExpression => true,
            _ => false,
        }
    }

    if node_has_side_effects(expr) || expr.descendants().any(|node| node_has_side_effects(&node)) {
        return true;
    }

    // Include ++/-- (both prefix and postfix) as side effects.
    expr.descendants_with_tokens()
        .any(|el| matches!(el.kind(), SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
}

fn parenthesize_initializer(text: &str, expr: &ast::Expression) -> String {
    if matches!(expr, ast::Expression::ParenthesizedExpression(_)) {
        return text.to_string();
    }

    let is_simple_primary = matches!(
        expr,
        ast::Expression::NameExpression(_)
            | ast::Expression::LiteralExpression(_)
            | ast::Expression::ThisExpression(_)
            | ast::Expression::SuperExpression(_)
            | ast::Expression::NewExpression(_)
            | ast::Expression::MethodCallExpression(_)
            | ast::Expression::FieldAccessExpression(_)
            | ast::Expression::ArrayAccessExpression(_)
    );

    if is_simple_primary {
        text.to_string()
    } else {
        format!("({text})")
    }
}

fn statement_end_including_trailing_newline(text: &str, stmt_end: usize) -> usize {
    let mut offset = stmt_end.min(text.len());

    // Consume trailing spaces/tabs at end-of-line so we don't leave whitespace-only lines behind.
    while offset < text.len() {
        match text.as_bytes()[offset] {
            b' ' | b'\t' => offset += 1,
            _ => break,
        }
    }

    let newline = NewlineStyle::detect(text);
    let newline_str = newline.as_str();

    if text
        .get(offset..)
        .unwrap_or_default()
        .starts_with(newline_str)
    {
        return offset + newline_str.len();
    }

    // Mixed-newline fallback.
    if text.get(offset..).unwrap_or_default().starts_with("\r\n") {
        return offset + 2;
    }
    if text.get(offset..).unwrap_or_default().starts_with('\n') {
        return offset + 1;
    }
    if text.get(offset..).unwrap_or_default().starts_with('\r') {
        return offset + 1;
    }

    offset
}

fn statement_end_including_trailing_inline_comment(text: &str, stmt_end: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut cursor = stmt_end.min(bytes.len());
    let line_end = line_break_start(text, cursor);
    let mut end = stmt_end;
    let mut saw_comment = false;

    loop {
        while cursor < line_end && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }

        if cursor + 1 >= line_end {
            break;
        }

        if bytes[cursor] == b'/' && bytes[cursor + 1] == b'/' {
            // Line comment: delete to (but not including) the line break.
            saw_comment = true;
            end = line_end;
            break;
        }

        if bytes[cursor] == b'/' && bytes[cursor + 1] == b'*' {
            let mut i = cursor + 2;
            let mut found = None;
            while i + 1 < line_end {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    found = Some(i + 2);
                    break;
                }
                i += 1;
            }
            let Some(comment_end) = found else {
                break;
            };
            saw_comment = true;
            end = comment_end;
            cursor = comment_end;
            continue;
        }

        break;
    }

    if !saw_comment {
        return None;
    }

    // If the trailing comment reached the end of the line (modulo whitespace), also delete the
    // remaining whitespace so we don't leave a whitespace-only line tail behind.
    let mut i = end.min(bytes.len());
    while i < line_end && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    if i == line_end {
        end = line_end;
    }

    Some(end)
}

fn line_break_start(text: &str, offset: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = offset.min(bytes.len());
    while i < bytes.len() {
        if bytes[i] == b'\n' || bytes[i] == b'\r' {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

fn find_innermost_statement_containing_range(
    root: &nova_syntax::SyntaxNode,
    range: TextRange,
) -> Option<ast::Statement> {
    root.descendants()
        .filter_map(ast::Statement::cast)
        .filter(|stmt| {
            let stmt_range = syntax_range(stmt.syntax());
            stmt_range.start <= range.start && range.end <= stmt_range.end
        })
        .min_by_key(|stmt| syntax_range(stmt.syntax()).len())
}

fn statement_block_and_index(stmt: &ast::Statement) -> Option<(ast::Block, usize)> {
    let block = stmt.syntax().parent().and_then(ast::Block::cast)?;
    let idx = block
        .statements()
        .position(|candidate| candidate.syntax() == stmt.syntax())?;
    Some((block, idx))
}

fn check_side_effectful_inline_order(
    root: &nova_syntax::SyntaxNode,
    decl_stmt: &ast::LocalVariableDeclarationStatement,
    targets: &[crate::semantic::Reference],
    decl_file: &FileId,
) -> Result<(), RefactorError> {
    let decl_block = decl_stmt
        .syntax()
        .parent()
        .and_then(ast::Block::cast)
        .ok_or(RefactorError::InlineSideEffects)?;
    let decl_index = decl_block
        .statements()
        .position(|stmt| stmt.syntax() == decl_stmt.syntax())
        .ok_or(RefactorError::InlineSideEffects)?;

    let mut earliest_usage_index: Option<usize> = None;
    for target in targets {
        // The statement-order check only supports analyzing the declaration file.
        if &target.file != decl_file {
            return Err(RefactorError::InlineSideEffects);
        }

        let usage_stmt = find_innermost_statement_containing_range(root, target.range)
            .ok_or(RefactorError::InlineSideEffects)?;
        let (usage_block, usage_index) =
            statement_block_and_index(&usage_stmt).ok_or(RefactorError::InlineSideEffects)?;

        if usage_block.syntax() != decl_block.syntax() {
            return Err(RefactorError::InlineSideEffects);
        }

        earliest_usage_index = Some(match earliest_usage_index {
            Some(existing) => existing.min(usage_index),
            None => usage_index,
        });
    }

    let Some(earliest) = earliest_usage_index else {
        return Err(RefactorError::InlineSideEffects);
    };

    match decl_index.checked_add(1) {
        Some(expected) if expected == earliest => Ok(()),
        _ => Err(RefactorError::InlineSideEffects),
    }
}
#[derive(Clone, Debug)]
struct ImportDecl {
    is_static: bool,
    path: String,
    trailing_comment: String,
}

impl ImportDecl {
    fn is_wildcard(&self) -> bool {
        self.path.ends_with(".*")
    }

    fn wildcard_package(&self) -> Option<&str> {
        self.path.strip_suffix(".*")
    }

    fn split_package_and_name(&self) -> Option<(&str, &str)> {
        self.path.rsplit_once('.')
    }

    fn simple_name(&self) -> Option<&str> {
        if self.is_wildcard() {
            return None;
        }
        self.split_package_and_name()
            .map(|(_, name)| name)
            .or(Some(self.path.as_str()))
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("import ");
        if self.is_static {
            out.push_str("static ");
        }
        out.push_str(&self.path);
        out.push(';');
        if !self.trailing_comment.is_empty() {
            out.push(' ');
            out.push_str(&self.trailing_comment);
        }
        out
    }
}

#[derive(Clone, Debug)]
struct ImportBlock {
    range: TextRange,
    imports: Vec<ImportDecl>,
}

fn parse_import_block(text: &str) -> ImportBlock {
    let mut scanner = JavaScanner::new(text);
    let mut stage = HeaderStage::BeforePackageOrImport;
    let mut imports: Vec<ImportDecl> = Vec::new();
    let mut first_import_start: Option<usize> = None;
    let mut last_import_line_end: Option<usize> = None;

    while let Some(token) = scanner.next_token() {
        match stage {
            HeaderStage::BeforePackageOrImport => match token.kind {
                TokenKind::Ident("package") => {
                    scanner.consume_until_semicolon();
                    stage = HeaderStage::AfterPackage;
                }
                TokenKind::Ident("import") => {
                    let start = token.start;
                    if first_import_start.is_none() {
                        first_import_start = Some(start);
                    }
                    if let Some((decl, end)) = scanner.parse_import_decl(start) {
                        last_import_line_end = Some(end);
                        imports.push(decl);
                        stage = HeaderStage::InImports;
                    } else {
                        break;
                    }
                }
                TokenKind::Ident(word) if is_declaration_start_keyword(word) => break,
                _ => {}
            },
            HeaderStage::AfterPackage => match token.kind {
                TokenKind::Ident("import") => {
                    let start = token.start;
                    if first_import_start.is_none() {
                        first_import_start = Some(start);
                    }
                    if let Some((decl, end)) = scanner.parse_import_decl(start) {
                        last_import_line_end = Some(end);
                        imports.push(decl);
                        stage = HeaderStage::InImports;
                    } else {
                        break;
                    }
                }
                TokenKind::Symbol('@') => break,
                TokenKind::Ident(word) if is_declaration_start_keyword(word) => break,
                _ => {}
            },
            HeaderStage::InImports => match token.kind {
                TokenKind::Ident("import") => {
                    let start = token.start;
                    if first_import_start.is_none() {
                        first_import_start = Some(start);
                    }
                    if let Some((decl, end)) = scanner.parse_import_decl(start) {
                        last_import_line_end = Some(end);
                        imports.push(decl);
                    } else {
                        break;
                    }
                }
                TokenKind::Symbol('@') => break,
                TokenKind::Ident(word) if is_declaration_start_keyword(word) => break,
                _ => break,
            },
        }
    }

    let Some(start) = first_import_start else {
        return ImportBlock {
            range: TextRange::new(0, 0),
            imports: Vec::new(),
        };
    };
    let last_end = last_import_line_end.unwrap_or(start);
    let end = first_non_whitespace(text, last_end);
    ImportBlock {
        range: TextRange::new(start, end),
        imports,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderStage {
    BeforePackageOrImport,
    AfterPackage,
    InImports,
}

fn is_declaration_start_keyword(keyword: &str) -> bool {
    matches!(
        keyword,
        "class"
            | "interface"
            | "enum"
            | "record"
            | "module"
            | "open"
            | "public"
            | "private"
            | "protected"
            | "abstract"
            | "final"
            | "strictfp"
    )
}

fn first_non_whitespace(text: &str, mut offset: usize) -> usize {
    let bytes = text.as_bytes();
    while offset < bytes.len() && (bytes[offset] as char).is_ascii_whitespace() {
        offset += 1;
    }
    offset
}

#[derive(Clone, Debug)]
struct Token<'a> {
    kind: TokenKind<'a>,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
enum TokenKind<'a> {
    Ident(&'a str),
    Symbol(char),
    DoubleColon,
    StringLiteral,
    CharLiteral,
}

struct JavaScanner<'a> {
    text: &'a str,
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> JavaScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            offset: 0,
        }
    }

    fn next_token(&mut self) -> Option<Token<'a>> {
        self.skip_trivia();
        if self.offset >= self.bytes.len() {
            return None;
        }

        let start = self.offset;
        let b = self.bytes[self.offset];

        if b == b':' && self.offset + 1 < self.bytes.len() && self.bytes[self.offset + 1] == b':' {
            self.offset += 2;
            return Some(Token {
                kind: TokenKind::DoubleColon,
                start,
                end: self.offset,
            });
        }

        let c = b as char;
        if is_ident_start(c) {
            self.offset += 1;
            while self.offset < self.bytes.len()
                && is_ident_continue(self.bytes[self.offset] as char)
            {
                self.offset += 1;
            }
            return Some(Token {
                kind: TokenKind::Ident(&self.text[start..self.offset]),
                start,
                end: self.offset,
            });
        }

        if c == '"' {
            self.consume_string_literal();
            return Some(Token {
                kind: TokenKind::StringLiteral,
                start,
                end: self.offset,
            });
        }

        if c == '\'' {
            self.consume_char_literal();
            return Some(Token {
                kind: TokenKind::CharLiteral,
                start,
                end: self.offset,
            });
        }

        self.offset += 1;
        Some(Token {
            kind: TokenKind::Symbol(c),
            start,
            end: self.offset,
        })
    }

    fn skip_trivia(&mut self) {
        while self.offset < self.bytes.len() {
            let b = self.bytes[self.offset];
            let c = b as char;
            if c.is_ascii_whitespace() {
                self.offset += 1;
                continue;
            }

            if b == b'/' && self.offset + 1 < self.bytes.len() {
                match self.bytes[self.offset + 1] {
                    b'/' => {
                        self.offset += 2;
                        while self.offset < self.bytes.len() && self.bytes[self.offset] != b'\n' {
                            self.offset += 1;
                        }
                        continue;
                    }
                    b'*' => {
                        self.offset += 2;
                        while self.offset + 1 < self.bytes.len() {
                            if self.bytes[self.offset] == b'*'
                                && self.bytes[self.offset + 1] == b'/'
                            {
                                self.offset += 2;
                                break;
                            }
                            self.offset += 1;
                        }
                        continue;
                    }
                    _ => {}
                }
            }

            break;
        }
    }

    fn consume_until_semicolon(&mut self) {
        while let Some(tok) = self.next_token() {
            if matches!(tok.kind, TokenKind::Symbol(';')) {
                break;
            }
        }
    }

    fn parse_import_decl(&mut self, _start: usize) -> Option<(ImportDecl, usize)> {
        let mut is_static = false;

        // The `import` keyword has already been consumed. Parse optional `static`.
        let mut tok = self.next_token()?;
        if matches!(tok.kind, TokenKind::Ident("static")) {
            is_static = true;
            tok = self.next_token()?;
        }

        let TokenKind::Ident(first) = tok.kind else {
            return None;
        };
        let mut path = first.to_string();

        loop {
            let tok = self.next_token()?;
            match tok.kind {
                TokenKind::Symbol('.') => {
                    let tok = self.next_token()?;
                    match tok.kind {
                        TokenKind::Ident(seg) => {
                            path.push('.');
                            path.push_str(seg);
                        }
                        TokenKind::Symbol('*') => {
                            path.push_str(".*");
                        }
                        _ => return None,
                    }
                }
                TokenKind::Symbol(';') => {
                    let (comment, line_end) = scan_trailing_comment(self.text, tok.end);
                    self.offset = line_end;
                    return Some((
                        ImportDecl {
                            is_static,
                            path,
                            trailing_comment: comment,
                        },
                        line_end,
                    ));
                }
                _ => return None,
            }
        }
    }

    fn consume_string_literal(&mut self) {
        // Handles both normal strings and Java text blocks (`"""..."""`).
        if self.offset + 2 < self.bytes.len()
            && self.bytes[self.offset] == b'"'
            && self.bytes[self.offset + 1] == b'"'
            && self.bytes[self.offset + 2] == b'"'
        {
            self.offset += 3;
            while self.offset + 2 < self.bytes.len() {
                if self.bytes[self.offset] == b'"'
                    && self.bytes[self.offset + 1] == b'"'
                    && self.bytes[self.offset + 2] == b'"'
                {
                    self.offset += 3;
                    break;
                }
                self.offset += 1;
            }
            return;
        }

        self.offset += 1;
        while self.offset < self.bytes.len() {
            let b = self.bytes[self.offset];
            if b == b'\\' {
                self.offset = (self.offset + 2).min(self.bytes.len());
                continue;
            }
            self.offset += 1;
            if b == b'"' {
                break;
            }
        }
    }

    fn consume_char_literal(&mut self) {
        self.offset += 1;
        while self.offset < self.bytes.len() {
            let b = self.bytes[self.offset];
            if b == b'\\' {
                self.offset = (self.offset + 2).min(self.bytes.len());
                continue;
            }
            self.offset += 1;
            if b == b'\'' {
                break;
            }
        }
    }
}

fn scan_trailing_comment(text: &str, mut offset: usize) -> (String, usize) {
    let bytes = text.as_bytes();
    let len = bytes.len();

    while offset < len {
        match bytes[offset] {
            b' ' | b'\t' | b'\r' => offset += 1,
            _ => break,
        }
    }

    let mut comment = String::new();
    if offset + 1 < len && bytes[offset] == b'/' {
        match bytes[offset + 1] {
            b'/' => {
                let line_end = bytes[offset..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|o| offset + o)
                    .unwrap_or(len);
                comment = text[offset..line_end].trim_end_matches('\r').to_string();
            }
            b'*' => {
                // Preserve single-line block comments; multi-line ones are uncommon here.
                let line_end = bytes[offset..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|o| offset + o)
                    .unwrap_or(len);
                comment = text[offset..line_end].trim_end_matches('\r').to_string();
            }
            _ => {}
        }
    }

    let line_end = bytes[offset..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|o| offset + o + 1)
        .unwrap_or(len);

    (comment, line_end)
}

#[derive(Default)]
struct IdentifierUsage {
    all: HashSet<String>,
    unqualified: HashSet<String>,
}

fn collect_identifier_usage(text: &str) -> IdentifierUsage {
    let mut usage = IdentifierUsage::default();
    let mut i = 0;
    let bytes = text.as_bytes();

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum PrevSig {
        Dot,
        DoubleColon,
        Other,
    }

    let mut prev = PrevSig::Other;
    while i < bytes.len() {
        let c = bytes[i] as char;

        if c == '"' {
            i = skip_string_literal(text, i);
            continue;
        }

        if c == '\'' {
            i = skip_char_literal(text, i);
            continue;
        }

        if c == '/' && i + 1 < bytes.len() {
            let next = bytes[i + 1] as char;
            if next == '/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if next == '*' {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }

        if c == ':' && i + 1 < bytes.len() && bytes[i + 1] == b':' {
            prev = PrevSig::DoubleColon;
            i += 2;
            continue;
        }

        if c == '.' {
            prev = PrevSig::Dot;
            i += 1;
            continue;
        }

        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = &text[start..i];
            usage.all.insert(ident.to_string());
            if prev != PrevSig::Dot && prev != PrevSig::DoubleColon {
                usage.unqualified.insert(ident.to_string());
            }
            prev = PrevSig::Other;
            continue;
        }

        if !c.is_ascii_whitespace() {
            prev = PrevSig::Other;
        }
        i += 1;
    }

    usage
}

fn skip_string_literal(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    if i + 2 < bytes.len() && bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
        // Java text block.
        i += 3;
        while i + 2 < bytes.len() {
            if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                return i + 3;
            }
            i += 1;
        }
        return bytes.len();
    }

    i += 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        i += 1;
        if b == b'"' {
            break;
        }
    }
    i
}

fn skip_char_literal(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    i += 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        i += 1;
        if b == b'\'' {
            break;
        }
    }
    i
}

fn collect_declared_type_names(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut i = 0;
    let bytes = text.as_bytes();
    let mut prev_was_dot = false;
    let mut expect_name = false;

    while i < bytes.len() {
        let c = bytes[i] as char;

        if c == '"' {
            i = skip_string_literal(text, i);
            continue;
        }

        if c == '\'' {
            i = skip_char_literal(text, i);
            continue;
        }

        if c == '/' && i + 1 < bytes.len() {
            let next = bytes[i + 1] as char;
            if next == '/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if next == '*' {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }

        if c == '.' {
            prev_was_dot = true;
            i += 1;
            continue;
        }

        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = &text[start..i];

            if expect_name {
                out.insert(ident.to_string());
                expect_name = false;
                prev_was_dot = false;
                continue;
            }

            if !prev_was_dot && matches!(ident, "class" | "interface" | "enum" | "record") {
                expect_name = true;
            }

            prev_was_dot = false;
            continue;
        }

        if !c.is_ascii_whitespace() {
            prev_was_dot = false;
        }
        i += 1;
    }

    out
}

fn collect_uncovered_type_identifiers(
    unqualified: &HashSet<String>,
    explicitly_imported: &HashSet<String>,
    declared_types: &HashSet<String>,
) -> HashSet<String> {
    let java_lang: HashSet<&'static str> = [
        "String",
        "Object",
        "Class",
        "Throwable",
        "Exception",
        "RuntimeException",
        "Error",
        "Integer",
        "Long",
        "Short",
        "Byte",
        "Boolean",
        "Character",
        "Double",
        "Float",
        "Void",
        "Math",
        "System",
    ]
    .into_iter()
    .collect();

    unqualified
        .iter()
        .filter(|ident| {
            let Some(first) = ident.chars().next() else {
                return false;
            };
            if !first.is_ascii_uppercase() {
                return false;
            }
            if ident.len() == 1 {
                // Likely a generic type parameter (`T`, `E`, ...).
                return false;
            }
            if java_lang.contains(ident.as_str()) {
                return false;
            }
            if declared_types.contains(*ident) {
                return false;
            }
            if explicitly_imported.contains(*ident) {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

// Keep the public re-exports in lib.rs tidy.
#[allow(dead_code)]
fn _apply_edit_to_file(
    text: &str,
    file: FileId,
    edits: Vec<TextEdit>,
) -> Result<String, RefactorError> {
    Ok(apply_text_edits(
        text,
        &edits
            .into_iter()
            .map(|mut e| {
                e.file = file.clone();
                e
            })
            .collect::<Vec<_>>(),
    )?)
}
