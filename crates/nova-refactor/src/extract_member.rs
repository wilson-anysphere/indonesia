use std::collections::HashSet;

use nova_format::{format_member_insertion_with_newline, NewlineStyle};
use nova_index::TextRange;
use nova_syntax::ast::{self, AstNode};
use nova_syntax::{parse_java, SyntaxKind};
use thiserror::Error;

use crate::edit::{FileId, TextEdit as WorkspaceTextEdit, WorkspaceEdit};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExtractError {
    #[error("failed to parse Java source")]
    ParseError,
    #[error("selection does not resolve to an expression")]
    InvalidSelection,
    #[error("expression kind is not supported for extraction")]
    UnsupportedExpression,
    #[error("expression has side effects and cannot be extracted safely")]
    SideEffectfulExpression,
    #[error("expression depends on method-local variables or parameters")]
    DependsOnLocal,
    #[error(
        "expression is not in an instance context and cannot be extracted to an instance field"
    )]
    NotInstanceContext,
    #[error("expression is not safe to extract to a static context")]
    NotStaticSafe,
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
    pub edit: WorkspaceEdit,
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
    if selection.end > source.len() {
        return Err(ExtractError::InvalidSelection);
    }

    let selection = trim_range(source, selection);
    if selection.len() == 0 {
        return Err(ExtractError::InvalidSelection);
    }

    let parsed = parse_java(source);
    if !parsed.errors.is_empty() {
        return Err(ExtractError::ParseError);
    }

    let root = parsed.syntax();
    let expr = find_expression(root.clone(), selection).ok_or(ExtractError::InvalidSelection)?;
    if !is_supported_expression(&expr) {
        return Err(ExtractError::UnsupportedExpression);
    }
    if has_side_effects(expr.syntax()) {
        return Err(ExtractError::SideEffectfulExpression);
    }

    let class_body = expr
        .syntax()
        .ancestors()
        .find_map(ast::ClassBody::cast)
        .ok_or(ExtractError::NotInClass)?;

    if kind == ExtractKind::Field && is_in_static_context(&expr, &class_body) {
        return Err(ExtractError::NotInstanceContext);
    }
    if depends_on_local(&expr) {
        return Err(ExtractError::DependsOnLocal);
    }
    if kind == ExtractKind::Constant && !is_static_safe(&expr, &class_body) {
        return Err(ExtractError::NotStaticSafe);
    }

    let expr_range = syntax_range(expr.syntax());
    let expr_text = match kind {
        ExtractKind::Field => qualify_field_initializer_expr(source, &expr, &class_body),
        ExtractKind::Constant => qualify_constant_initializer_expr(source, &expr, &class_body)?,
    };
    let expr_type = infer_expr_type(source, &expr).unwrap_or_else(|| "Object".to_string());

    let existing_names = collect_field_names(&class_body);
    let suggested = match kind {
        ExtractKind::Constant => "VALUE".to_string(),
        ExtractKind::Field => "value".to_string(),
    };
    let mut name = options
        .name
        .as_deref()
        .map(|n| sanitize_identifier(n, kind))
        .filter(|n| !n.is_empty())
        .unwrap_or(suggested);
    name = make_unique(name, &existing_names);

    let occurrences = if options.replace_all {
        find_equivalent_expressions(source, &class_body, &expr, kind)
    } else {
        vec![expr_range]
    };

    let (insert_offset, indent, needs_blank_line_after) = insertion_point(source, &class_body);
    let declaration = match kind {
        ExtractKind::Constant => format!(
            "private static final {} {} = {};",
            expr_type, name, expr_text
        ),
        ExtractKind::Field => format!("private final {} {} = {};", expr_type, name, expr_text),
    };

    let insert_text = format_member_insertion_with_newline(
        &indent,
        &declaration,
        needs_blank_line_after,
        NewlineStyle::detect(source),
    );

    let file_id = FileId::new(file.to_string());
    let mut edit = WorkspaceEdit::new({
        let mut edits = Vec::new();
        edits.push(WorkspaceTextEdit::insert(
            file_id.clone(),
            insert_offset,
            insert_text,
        ));
        for range in occurrences {
            edits.push(WorkspaceTextEdit::replace(
                file_id.clone(),
                range,
                name.clone(),
            ));
        }
        edits
    });
    edit.normalize()
        .map_err(|_| ExtractError::InvalidSelection)?;

    Ok(ExtractOutcome { edit, name })
}

fn qualify_field_initializer_expr(
    source: &str,
    expr: &ast::Expression,
    class_body: &ast::ClassBody,
) -> String {
    // We insert extracted fields at the top of the class body. Any unqualified access to an
    // instance field in another field initializer is an "illegal forward reference" in Java, so we
    // proactively qualify such references with `this.` to keep the result compilable.
    //
    // We only qualify known instance-field names; static fields do not have the same forward
    // reference restriction in instance initializers.
    let instance_fields = collect_instance_field_names(class_body);
    if instance_fields.is_empty() {
        let expr_range = syntax_range(expr.syntax());
        return source[expr_range.start..expr_range.end].to_string();
    }

    let mut insertions: Vec<usize> = expr
        .syntax()
        .descendants()
        .filter_map(ast::NameExpression::cast)
        .filter_map(|name_expr| {
            let head = head_name_segment(name_expr.syntax())?;
            instance_fields
                .contains(&head)
                .then_some(syntax_range(name_expr.syntax()).start)
        })
        .collect();
    insertions.sort_unstable();
    insertions.dedup();

    splice_insertions(source, syntax_range(expr.syntax()), &insertions, "this.")
}

fn qualify_constant_initializer_expr(
    source: &str,
    expr: &ast::Expression,
    class_body: &ast::ClassBody,
) -> Result<String, ExtractError> {
    // Like `qualify_field_initializer_expr`, but for `static` field initializers:
    // unqualified access to *static* fields declared later is an illegal forward reference.
    //
    // We qualify those names with the enclosing class name (e.g. `A.FIELD`) which is accepted by
    // javac even when the referenced field is declared later.
    let static_fields = collect_static_field_names(class_body);
    if static_fields.is_empty() {
        let expr_range = syntax_range(expr.syntax());
        return Ok(source[expr_range.start..expr_range.end].to_string());
    }

    let mut insertions: Vec<usize> = expr
        .syntax()
        .descendants()
        .filter_map(ast::NameExpression::cast)
        .filter_map(|name_expr| {
            let head = head_name_segment(name_expr.syntax())?;
            static_fields
                .contains(&head)
                .then_some(syntax_range(name_expr.syntax()).start)
        })
        .collect();
    insertions.sort_unstable();
    insertions.dedup();

    if insertions.is_empty() {
        let expr_range = syntax_range(expr.syntax());
        return Ok(source[expr_range.start..expr_range.end].to_string());
    }

    let Some(class_name) = enclosing_class_name(class_body) else {
        // Anonymous classes have no name to qualify forward references.
        return Err(ExtractError::NotStaticSafe);
    };

    Ok(splice_insertions(
        source,
        syntax_range(expr.syntax()),
        &insertions,
        &format!("{class_name}."),
    ))
}

fn splice_insertions(source: &str, range: TextRange, insertions: &[usize], text: &str) -> String {
    let mut out = String::new();
    let mut last = range.start;
    for &pos in insertions {
        if pos < range.start || pos > range.end {
            continue;
        }
        out.push_str(&source[last..pos]);
        out.push_str(text);
        last = pos;
    }
    out.push_str(&source[last..range.end]);
    out
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

fn syntax_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn find_expression(root: nova_syntax::SyntaxNode, selection: TextRange) -> Option<ast::Expression> {
    for expr in root.descendants().filter_map(ast::Expression::cast) {
        let range = syntax_range(expr.syntax());
        if range.start == selection.start && range.end == selection.end {
            return Some(expr);
        }
    }
    None
}

fn is_supported_expression(expr: &ast::Expression) -> bool {
    // Some Java expressions are only type-correct in the context where they appear (notably lambdas
    // and method references). `extract_member` currently synthesizes a standalone member
    // declaration type from limited syntactic heuristics; for these expressions that can easily
    // produce uncompilable code (e.g. `Object value = () -> {}`), so we reject them.
    !matches!(
        expr,
        ast::Expression::LambdaExpression(_)
            | ast::Expression::MethodReferenceExpression(_)
            | ast::Expression::ConstructorReferenceExpression(_)
            | ast::Expression::ArrayInitializer(_)
    )
}

fn has_side_effects(expr: &nova_syntax::SyntaxNode) -> bool {
    // Expression nodes which are inherently side-effectful or order-dependent.
    if expr.descendants().any(|node| {
        matches!(
            node.kind(),
            SyntaxKind::MethodCallExpression
                | SyntaxKind::NewExpression
                | SyntaxKind::ClassInstanceCreationExpression
                | SyntaxKind::ArrayCreationExpression
                | SyntaxKind::AssignmentExpression
        )
    }) {
        return true;
    }

    // `++i` / `i++` / `--i` / `i--`.
    //
    // The parser's concrete node shapes around increment/decrement have evolved over time, so we
    // detect the tokens directly to stay robust.
    expr.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|tok| matches!(tok.kind(), SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
}

fn depends_on_local(expr: &ast::Expression) -> bool {
    let Some(container) = enclosing_executable_container(expr.syntax()) else {
        return false;
    };
    let local_names = collect_local_names(&container);
    if local_names.is_empty() {
        return false;
    }
    expr_uses_any_name(expr.syntax(), &local_names)
}

fn is_directly_in_class_body(expr: &ast::Expression, class_body: &ast::ClassBody) -> bool {
    expr.syntax()
        .ancestors()
        .find_map(ast::ClassBody::cast)
        .is_some_and(|body| body.syntax().text_range() == class_body.syntax().text_range())
}

fn is_in_static_context(expr: &ast::Expression, class_body: &ast::ClassBody) -> bool {
    // An instance field extracted by `extract_field` must only be used from an instance context.
    // If the selected expression (or a replace-all occurrence) is inside a `static` member, then
    // replacing it with an instance field reference will not compile.
    let body_range = class_body.syntax().text_range();
    for node in expr.syntax().ancestors() {
        if node.text_range() == body_range {
            break;
        }
        if let Some(method) = ast::MethodDeclaration::cast(node.clone()) {
            return has_static_modifier(method.modifiers());
        }
        if let Some(init) = ast::InitializerBlock::cast(node.clone()) {
            return has_static_modifier(init.modifiers());
        }
        if let Some(field) = ast::FieldDeclaration::cast(node.clone()) {
            return has_static_modifier(field.modifiers());
        }
    }
    false
}

fn has_static_modifier(modifiers: Option<ast::Modifiers>) -> bool {
    modifiers.is_some_and(|mods| {
        mods.keywords()
            .any(|tok| tok.kind() == SyntaxKind::StaticKw)
    })
}

fn enclosing_executable_container(
    node: &nova_syntax::SyntaxNode,
) -> Option<nova_syntax::SyntaxNode> {
    node.ancestors().find(|n| {
        matches!(
            n.kind(),
            SyntaxKind::MethodDeclaration
                | SyntaxKind::ConstructorDeclaration
                | SyntaxKind::CompactConstructorDeclaration
                | SyntaxKind::InitializerBlock
        )
    })
}

fn collect_local_names(container: &nova_syntax::SyntaxNode) -> HashSet<String> {
    let mut out = HashSet::new();

    for param in container.descendants().filter_map(ast::Parameter::cast) {
        if let Some(name) = param.name_token() {
            out.insert(name.text().to_string());
        }
    }

    for param in container
        .descendants()
        .filter_map(ast::LambdaParameter::cast)
    {
        if let Some(name) = param.name_token() {
            out.insert(name.text().to_string());
        }
    }

    for var in container
        .descendants()
        .filter_map(ast::VariableDeclarator::cast)
    {
        if let Some(name) = var.name_token() {
            out.insert(name.text().to_string());
        }
    }

    // Pattern variables (e.g. `if (x instanceof Foo f)`).
    for pat in container.descendants().filter_map(ast::TypePattern::cast) {
        if let Some(name) = pat.name_token() {
            out.insert(name.text().to_string());
        }
    }

    // Some binders (enhanced-for loop variables, try-with-resources, etc.) are represented as
    // `VariableDeclaratorId` nodes instead of full `VariableDeclarator`s.
    for node in container
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::VariableDeclaratorId)
    {
        if let Some(tok) = node
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind().is_identifier_like())
        {
            out.insert(tok.text().to_string());
        }
    }

    // Catch parameters are currently parsed as a bare identifier token inside `CatchClause`.
    for catch_clause in container
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::CatchClause)
    {
        let Some(r_paren) = catch_clause
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind() == SyntaxKind::RParen)
        else {
            continue;
        };

        let header_end = u32::from(r_paren.text_range().start());
        let mut last_ident: Option<(u32, String)> = None;
        for tok in catch_clause
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
        {
            let start = u32::from(tok.text_range().start());
            if start >= header_end {
                continue;
            }
            if tok.kind().is_identifier_like() {
                let name = tok.text().to_string();
                match &last_ident {
                    Some((best_start, _)) if start <= *best_start => {}
                    _ => last_ident = Some((start, name)),
                }
            }
        }

        if let Some((_, name)) = last_ident {
            out.insert(name);
        }
    }

    out
}

fn expr_uses_any_name(expr: &nova_syntax::SyntaxNode, local_names: &HashSet<String>) -> bool {
    // Any name expression rooted in a local/parameter indicates we can't move the expression
    // to a class-level initializer without breaking compilation.
    expr.descendants()
        .filter_map(ast::NameExpression::cast)
        .any(|name_expr| {
            let Some(head) = head_name_segment(name_expr.syntax()) else {
                return false;
            };
            local_names.contains(&head)
        })
}

fn head_name_segment(name_expr: &nova_syntax::SyntaxNode) -> Option<String> {
    name_expr
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind().is_identifier_like())
        .map(|tok| tok.text().to_string())
}

fn is_static_safe(expr: &ast::Expression, class_body: &ast::ClassBody) -> bool {
    // `this` / `super` (or anything rooted in them) cannot appear in a `static` field initializer.
    if expr.syntax().descendants().any(|node| {
        matches!(
            node.kind(),
            SyntaxKind::ThisExpression | SyntaxKind::SuperExpression
        )
    }) {
        return false;
    }

    let instance_fields = collect_instance_field_names(class_body);
    if instance_fields.is_empty() {
        // Continue; we still want to apply the heuristic below for member accesses.
    }

    // Reject unqualified references to instance fields (e.g. `foo + 1` where `foo` is an
    // instance field), and also qualified references rooted in an instance field (e.g. `foo.bar`)
    // which would likewise be illegal in a static context.
    if expr
        .syntax()
        .descendants()
        .filter_map(ast::NameExpression::cast)
        .any(|name_expr| {
            let Some(head) = head_name_segment(name_expr.syntax()) else {
                return false;
            };
            instance_fields.contains(&head)
        })
    {
        return false;
    }

    // Optional conservative heuristic: allow `Type.CONST`-style references (e.g. `Math.PI`) but
    // reject member accesses rooted in a lowercase identifier (likely a local or an instance
    // field from an outer scope that we can't reliably detect).
    if expr_has_lowercase_receiver_member_access(expr.syntax()) {
        return false;
    }

    true
}

fn collect_instance_field_names(body: &ast::ClassBody) -> HashSet<String> {
    let mut out = HashSet::new();
    for member in body.members() {
        let ast::ClassMember::FieldDeclaration(field) = member else {
            continue;
        };
        let is_static = field
            .modifiers()
            .map(|mods| {
                mods.keywords()
                    .any(|tok| tok.kind() == SyntaxKind::StaticKw)
            })
            .unwrap_or(false);
        if is_static {
            continue;
        }
        let Some(list) = field.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            if let Some(name) = decl.name_token() {
                out.insert(name.text().to_string());
            }
        }
    }
    out
}

fn collect_static_field_names(body: &ast::ClassBody) -> HashSet<String> {
    let mut out = HashSet::new();
    for member in body.members() {
        let ast::ClassMember::FieldDeclaration(field) = member else {
            continue;
        };
        let is_static = field
            .modifiers()
            .map(|mods| {
                mods.keywords()
                    .any(|tok| tok.kind() == SyntaxKind::StaticKw)
            })
            .unwrap_or(false);
        if !is_static {
            continue;
        }
        let Some(list) = field.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            if let Some(name) = decl.name_token() {
                out.insert(name.text().to_string());
            }
        }
    }
    out
}

fn enclosing_class_name(body: &ast::ClassBody) -> Option<String> {
    body.syntax()
        .ancestors()
        .find_map(ast::ClassDeclaration::cast)
        .and_then(|decl| decl.name_token())
        .map(|tok| tok.text().to_string())
}

fn expr_has_lowercase_receiver_member_access(expr: &nova_syntax::SyntaxNode) -> bool {
    // Check both qualified names (`NameExpression` containing a `Name` with dots)
    // and field-access expressions (`FieldAccessExpression`).

    // Qualified names like `java.lang.Math.PI`.
    if expr
        .descendants()
        .filter_map(ast::NameExpression::cast)
        .any(|name_expr| {
            let idents: Vec<_> = name_expr
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|tok| tok.kind().is_identifier_like())
                .map(|tok| tok.text().to_string())
                .collect();
            if idents.len() < 2 {
                return false;
            }
            let receiver = &idents[idents.len() - 2];
            receiver
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_lowercase())
        })
    {
        return true;
    }

    // Field-access chains like `a.b` where `a` is an expression.
    expr.descendants()
        .filter_map(ast::FieldAccessExpression::cast)
        .any(|field_access| {
            let Some(receiver) = field_access.expression() else {
                return false;
            };
            let Some(receiver_name) = receiver_head_name(&receiver) else {
                return false;
            };
            receiver_name
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_lowercase())
        })
}

fn receiver_head_name(receiver: &ast::Expression) -> Option<String> {
    match receiver {
        ast::Expression::NameExpression(name_expr) => {
            let mut last: Option<String> = None;
            for tok in name_expr
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|tok| tok.kind().is_identifier_like())
            {
                last = Some(tok.text().to_string());
            }
            last
        }
        ast::Expression::FieldAccessExpression(field_access) => {
            // For nested field accesses, keep walking left.
            let inner = field_access.expression()?;
            receiver_head_name(&inner)
        }
        _ => None,
    }
}

fn infer_expr_type(_source: &str, expr: &ast::Expression) -> Option<String> {
    fn infer_type_from_tokens(tokens: impl Iterator<Item = nova_syntax::SyntaxToken>) -> String {
        let mut saw_string = false;
        let mut saw_boolean = false;
        let mut saw_double = false;
        let mut saw_float = false;
        let mut saw_long = false;

        for tok in tokens.filter(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof) {
            match tok.kind() {
                SyntaxKind::StringLiteral | SyntaxKind::TextBlock => saw_string = true,
                SyntaxKind::TrueKw
                | SyntaxKind::FalseKw
                | SyntaxKind::AmpAmp
                | SyntaxKind::PipePipe
                | SyntaxKind::Bang
                | SyntaxKind::EqEq
                | SyntaxKind::BangEq
                | SyntaxKind::Less
                | SyntaxKind::LessEq
                | SyntaxKind::Greater
                | SyntaxKind::GreaterEq
                | SyntaxKind::InstanceofKw => saw_boolean = true,
                SyntaxKind::DoubleLiteral => saw_double = true,
                SyntaxKind::FloatLiteral => saw_float = true,
                SyntaxKind::LongLiteral => saw_long = true,
                _ => {}
            }
        }

        if saw_string {
            "String".to_string()
        } else if saw_boolean {
            "boolean".to_string()
        } else if saw_double {
            "double".to_string()
        } else if saw_float {
            "float".to_string()
        } else if saw_long {
            "long".to_string()
        } else {
            "int".to_string()
        }
    }

    match expr {
        ast::Expression::LiteralExpression(lit) => {
            let tok = lit
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)?;
            Some(
                match tok.kind() {
                    SyntaxKind::IntLiteral => "int",
                    SyntaxKind::LongLiteral => "long",
                    SyntaxKind::FloatLiteral => "float",
                    SyntaxKind::DoubleLiteral => "double",
                    SyntaxKind::StringLiteral | SyntaxKind::TextBlock => "String",
                    SyntaxKind::CharLiteral => "char",
                    SyntaxKind::TrueKw | SyntaxKind::FalseKw => "boolean",
                    _ => "Object",
                }
                .to_string(),
            )
        }
        ast::Expression::CastExpression(cast) => {
            let ty = cast.ty()?;
            if let Some(primitive) = ty.primitive() {
                let tok = primitive
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|el| el.into_token())
                    .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)?;
                let ty = match tok.kind() {
                    SyntaxKind::BooleanKw => "boolean",
                    SyntaxKind::ByteKw => "byte",
                    SyntaxKind::ShortKw => "short",
                    SyntaxKind::IntKw => "int",
                    SyntaxKind::LongKw => "long",
                    SyntaxKind::CharKw => "char",
                    SyntaxKind::FloatKw => "float",
                    SyntaxKind::DoubleKw => "double",
                    _ => "Object",
                };
                return Some(ty.to_string());
            }

            // Best-effort: recognize `(String) <expr>` (avoid array casts like `String[]`).
            let has_brackets = ty
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|tok| tok.kind() == SyntaxKind::LBracket);
            if !has_brackets {
                let mut last_ident: Option<String> = None;
                for tok in ty
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|el| el.into_token())
                    .filter(|tok| tok.kind().is_identifier_like())
                {
                    last_ident = Some(tok.text().to_string());
                }

                if last_ident.as_deref() == Some("String") {
                    return Some("String".to_string());
                }
            }

            None
        }
        ast::Expression::ConditionalExpression(cond) => {
            // Conditional expressions (`?:`) are typed based on their then/else branches, not the
            // condition. Scan the branches only so boolean literals in the condition don't
            // incorrectly force a `boolean` inference.
            let then_branch = cond.then_branch()?;
            let else_branch = cond.else_branch()?;
            let then_tokens = then_branch
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token());
            let else_tokens = else_branch
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token());
            Some(infer_type_from_tokens(then_tokens.chain(else_tokens)))
        }
        ast::Expression::BinaryExpression(_)
        | ast::Expression::UnaryExpression(_)
        | ast::Expression::ParenthesizedExpression(_) => {
            // Best-effort: scan descendant tokens and infer a conservative type based on
            // common literal/operator cues.
            //
            // Precedence:
            // - If any String literal/text block appears => String
            // - Else if any boolean literal/operator appears => boolean
            // - Else numeric:
            //   - If any double literal appears => double
            //   - Else if any float literal appears => float
            //   - Else if any long literal appears => long
            //   - Else => int
            Some(infer_type_from_tokens(
                expr.syntax()
                    .descendants_with_tokens()
                    .filter_map(|el| el.into_token()),
            ))
        }
        _ => None,
    }
}

fn collect_field_names(body: &ast::ClassBody) -> HashSet<String> {
    let mut out = HashSet::new();
    for member in body.members() {
        let ast::ClassMember::FieldDeclaration(field) = member else {
            continue;
        };
        let Some(list) = field.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            if let Some(name) = decl.name_token() {
                out.insert(name.text().to_string());
            }
        }
    }
    out
}

fn sanitize_identifier(name: &str, kind: ExtractKind) -> String {
    let mut out = String::new();
    for (idx, ch) in name.chars().enumerate() {
        if idx == 0 {
            if ch.is_ascii_alphabetic() || ch == '_' {
                out.push(ch);
            }
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        }
    }
    match kind {
        ExtractKind::Constant => out.to_ascii_uppercase(),
        ExtractKind::Field => {
            let mut chars = out.chars();
            match chars.next() {
                Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
                None => out,
            }
        }
    }
}

fn make_unique(mut name: String, existing: &HashSet<String>) -> String {
    if !existing.contains(&name) {
        return name;
    }
    let base = name.clone();
    let mut idx = 1usize;
    loop {
        let candidate = format!("{base}{idx}");
        if !existing.contains(&candidate) {
            name = candidate;
            break;
        }
        idx += 1;
    }
    name
}

fn normalize_expr_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn find_equivalent_expressions(
    source: &str,
    class_body: &ast::ClassBody,
    selected: &ast::Expression,
    kind: ExtractKind,
) -> Vec<TextRange> {
    let selected_norm = normalize_expr_text(
        source
            .get(syntax_range(selected.syntax()).start..syntax_range(selected.syntax()).end)
            .unwrap_or_default(),
    );

    let mut ranges = Vec::new();
    for expr in class_body
        .syntax()
        .descendants()
        .filter_map(ast::Expression::cast)
    {
        if !is_directly_in_class_body(&expr, class_body) {
            continue;
        }
        if kind == ExtractKind::Field && is_in_static_context(&expr, class_body) {
            continue;
        }
        if depends_on_local(&expr) {
            continue;
        }
        if has_side_effects(expr.syntax()) {
            continue;
        }
        let range = syntax_range(expr.syntax());
        let Some(text) = source.get(range.start..range.end) else {
            continue;
        };
        if normalize_expr_text(text) == selected_norm {
            ranges.push(range);
        }
    }
    ranges.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));
    ranges.dedup();
    ranges
}

fn insertion_point(source: &str, body: &ast::ClassBody) -> (usize, String, bool) {
    let newline = NewlineStyle::detect(source);
    let newline_str = newline.as_str();
    let brace_end = body
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::LBrace)
        .map(|tok| u32::from(tok.text_range().end()) as usize)
        .unwrap_or_else(|| syntax_range(body.syntax()).start);

    // Insert immediately after the first newline following `{`, so we end up at the indentation
    // whitespace for the first existing member (if any).
    let mut offset = brace_end;
    if let Some(rel) = source[offset..].find('\n') {
        offset += rel + 1;
        // If this is a CRLF file, the `\r` will be before the `\n`. Step past it as well.
        if offset >= 2 && source.as_bytes()[offset - 2] == b'\r' && newline_str == "\r\n" {
            // Offset already includes '\n'; nothing extra to do.
        }
    }

    // Determine existing indentation.
    let mut indent_end = offset;
    while indent_end < source.len() {
        match source.as_bytes()[indent_end] {
            b' ' | b'\t' => indent_end += 1,
            _ => break,
        }
    }
    let indent = source[offset..indent_end].to_string();

    // Blank line after when there are already members in the body.
    let needs_blank_line_after = body.members().next().is_some();

    (offset, indent, needs_blank_line_after)
}
