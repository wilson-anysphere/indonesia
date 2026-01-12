use std::collections::{HashMap, HashSet};

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
    #[error("rename is only supported for local variables and parameters (got {kind:?})")]
    RenameNotSupported { kind: Option<JavaSymbolKind> },
    #[error(transparent)]
    Materialize(#[from] MaterializeError),
    #[error("unknown file {0:?}")]
    UnknownFile(FileId),
    #[error("expected a variable with initializer for inline")]
    InlineNotSupported,
    #[error("no variable usage at the given cursor/usage range")]
    InlineNoUsageAtCursor,
    #[error("variable initializer has side effects and cannot be inlined safely")]
    InlineSideEffects,
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
    if !matches!(
        kind,
        Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter)
    ) {
        return Err(RefactorError::RenameNotSupported { kind });
    }

    let conflicts = check_rename_conflicts(db, params.symbol, &params.new_name);
    if !conflicts.is_empty() {
        return Err(RefactorError::Conflicts(conflicts));
    }

    let changes = vec![SemanticChange::Rename {
        symbol: params.symbol,
        new_name: params.new_name,
    }];
    Ok(materialize(db, changes)?)
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
}

pub fn extract_variable(
    db: &dyn RefactorDatabase,
    params: ExtractVariableParams,
) -> Result<WorkspaceEdit, RefactorError> {
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
    let expr_range = syntax_range(expr.syntax());
    let expr_text = text
        .get(selection.start..selection.end)
        .ok_or(RefactorError::InvalidSelection)?
        .to_string();

    let stmt = expr
        .syntax()
        .ancestors()
        .find_map(ast::Statement::cast)
        .ok_or(RefactorError::InvalidSelection)?;

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
    let insert_pos = line_start(text, stmt_range.start);
    let indent = current_indent(text, insert_pos);

    let ty = if params.use_var {
        "var".to_string()
    } else {
        infer_expr_type(text, &expr)
            .map(|s| s.to_string())
            .ok_or(RefactorError::TypeInferenceFailed)?
    };

    let newline = NewlineStyle::detect(text).as_str();
    let name = params.name;
    let decl = format!("{indent}{ty} {} = {expr_text};{newline}", &name);

    let mut edit = WorkspaceEdit::new(vec![
        TextEdit::insert(params.file.clone(), insert_pos, decl),
        TextEdit::replace(params.file.clone(), expr_range, name),
    ]);
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

pub fn inline_variable(
    db: &dyn RefactorDatabase,
    params: InlineVariableParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let def = db
        .symbol_definition(params.symbol)
        .ok_or(RefactorError::InlineNotSupported)?;

    let text = db
        .file_text(&def.file)
        .ok_or_else(|| RefactorError::UnknownFile(def.file.clone()))?;

    let parsed = parse_java(text);
    if !parsed.errors.is_empty() {
        return Err(RefactorError::InlineNotSupported);
    }

    let root = parsed.syntax();
    let decl = find_local_variable_declaration(&root, def.name_range)
        .ok_or(RefactorError::InlineNotSupported)?;

    let init_expr = decl.initializer;
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

    // Disallow inlining when the variable is written to after initialization.
    if is_variable_written_to(db, &all_refs) {
        return Err(RefactorError::InlineNotSupported);
    }

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

    let remove_decl = params.inline_all || all_refs.len() == 1;

    if init_has_side_effects && !(remove_decl && targets.len() == 1) {
        return Err(RefactorError::InlineSideEffects);
    }

    let mut edits: Vec<TextEdit> = targets
        .into_iter()
        .map(|usage| TextEdit::replace(usage.file, usage.range, init_replacement.clone()))
        .collect();

    if remove_decl {
        let stmt_range = decl.statement_range;
        let decl_start = line_start(text, stmt_range.start);
        let decl_end = statement_end_including_trailing_newline(text, stmt_range.end);
        edits.push(TextEdit::delete(
            def.file.clone(),
            TextRange::new(decl_start, decl_end),
        ));
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
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

fn infer_expr_type(source: &str, expr: &ast::Expression) -> Option<&'static str> {
    match expr {
        ast::Expression::LiteralExpression(lit) => {
            let tok = lit
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)?;
            match tok.kind() {
                SyntaxKind::IntLiteral => Some("int"),
                SyntaxKind::StringLiteral => Some("String"),
                SyntaxKind::CharLiteral => Some("char"),
                _ => None,
            }
        }
        ast::Expression::BinaryExpression(_)
        | ast::Expression::UnaryExpression(_)
        | ast::Expression::ParenthesizedExpression(_) => {
            let range = syntax_range(expr.syntax());
            let text = source.get(range.start..range.end)?.trim();
            if text.contains('"') {
                Some("String")
            } else {
                Some("int")
            }
        }
        _ => None,
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

#[derive(Debug)]
struct LocalVarDeclInfo {
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

fn has_side_effects(expr: &nova_syntax::SyntaxNode) -> bool {
    if expr.descendants().any(|node| {
        matches!(
            node.kind(),
            SyntaxKind::MethodCallExpression
                | SyntaxKind::NewExpression
                | SyntaxKind::AssignmentExpression
        )
    }) {
        return true;
    }

    // Include ++/-- (both prefix and postfix) as side effects.
    expr.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|tok| matches!(tok.kind(), SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
}

fn parenthesize_initializer(text: &str, expr: &ast::Expression) -> String {
    if matches!(expr, ast::Expression::ParenthesizedExpression(_)) {
        return text.to_string();
    }

    let needs_parens = matches!(
        expr,
        ast::Expression::BinaryExpression(_)
            | ast::Expression::UnaryExpression(_)
            | ast::Expression::ConditionalExpression(_)
            | ast::Expression::AssignmentExpression(_)
    );

    if needs_parens {
        format!("({text})")
    } else {
        text.to_string()
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

    if text.get(offset..).unwrap_or_default().starts_with(newline_str) {
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

fn is_variable_written_to(db: &dyn RefactorDatabase, refs: &[crate::semantic::Reference]) -> bool {
    // Parse each file once.
    let mut parsed_by_file: HashMap<FileId, Option<nova_syntax::SyntaxNode>> = HashMap::new();

    for reference in refs {
        let root = parsed_by_file
            .entry(reference.file.clone())
            .or_insert_with(|| {
                let Some(text) = db.file_text(&reference.file) else {
                    return None;
                };
                let parsed = parse_java(text);
                if !parsed.errors.is_empty() {
                    return None;
                }
                Some(parsed.syntax())
            })
            .clone();

        let Some(root) = root else {
            // If we can't parse, be conservative.
            return true;
        };

        if is_write_reference(&root, reference.range) {
            return true;
        }
    }

    false
}

fn is_write_reference(root: &nova_syntax::SyntaxNode, range: TextRange) -> bool {
    let Some(tok) = root
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::Identifier && syntax_token_range(tok) == range)
    else {
        return false;
    };

    let token_range = range;
    let Some(parent) = tok.parent() else {
        return false;
    };

    // Detect assignment LHS.
    for node in parent.ancestors() {
        if let Some(assign) = ast::AssignmentExpression::cast(node.clone()) {
            if let Some(lhs) = assign.lhs() {
                let lhs_range = syntax_range(lhs.syntax());
                if lhs_range.start <= token_range.start && token_range.end <= lhs_range.end {
                    return true;
                }
            }
        }

        if let Some(unary) = ast::UnaryExpression::cast(node) {
            let has_inc_dec = unary
                .syntax()
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|tok| matches!(tok.kind(), SyntaxKind::PlusPlus | SyntaxKind::MinusMinus));
            if has_inc_dec {
                return true;
            }
        }
    }

    false
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
