use std::collections::{HashMap, HashSet};

use nova_format::NewlineStyle;
use nova_syntax::java::{self, ast as jast};

use crate::edit::{FileId, TextEdit, TextRange, WorkspaceEdit};
use crate::java::is_ident_char_byte;

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
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

#[derive(Debug, Clone)]
struct Invocation<'a> {
    name: &'a str,
    args: Vec<&'a jast::Expr>,
    receiver: Option<&'a jast::Expr>,
    call_range: nova_types::Span,
    stmt_range: nova_types::Span,
    stmt_indent: String,
    enclosing_method_locals: HashSet<String>,
}

#[derive(Debug, Clone)]
struct MethodToInline<'a> {
    decl: &'a jast::MethodDecl,
    params: Vec<&'a jast::ParamDecl>,
    body: &'a jast::Block,
    return_expr: &'a jast::Expr,
    locals: Vec<&'a jast::LocalVarStmt>,
}

pub fn inline_method(
    file: &str,
    source: &str,
    cursor_byte_offset: usize,
    options: InlineMethodOptions,
) -> Result<WorkspaceEdit, InlineMethodError> {
    let parsed = java::parse(source);
    let unit = parsed.compilation_unit();

    let invocation = find_invocation_at_offset(unit, source, cursor_byte_offset)
        .ok_or(InlineMethodError::NotOnInvocation)?;
    validate_receiver(invocation.receiver, source)?;

    let method = find_method_to_inline(unit, invocation.name, invocation.args.len())?;
    validate_method(source, method.decl)?;

    if method.decl.return_ty.text.trim() == "void" {
        return Err(InlineMethodError::VoidMethodNotSupported);
    }

    if is_recursive(source, method.decl.name.as_str(), method.body) {
        return Err(InlineMethodError::RecursiveMethod);
    }

    // Collect call sites.
    let call_sites = if options.inline_all {
        find_all_return_call_sites(unit, source, invocation.name, invocation.args.len())?
    } else {
        vec![invocation]
    };

    let mut edits: Vec<TextEdit> = Vec::new();
    let file_id = FileId::new(file.to_string());
    let newline = NewlineStyle::detect(source).as_str().to_string();

    for site in call_sites {
        validate_receiver(site.receiver, source)?;

        if site.args.len() != method.params.len() {
            return Err(InlineMethodError::ArgumentCountMismatch);
        }

        let replacement = inline_at_site(source, &newline, &method, &site);
        let stmt_range_including_indent = TextRange::new(
            line_start(source, site.stmt_range.start),
            site.stmt_range.end,
        );
        edits.push(TextEdit::replace(
            file_id.clone(),
            stmt_range_including_indent,
            replacement,
        ));
    }

    // Best-effort: delete the declaration when inlining all usages.
    if options.inline_all {
        let decl_range = method_decl_deletion_range(source, method.decl);
        edits.push(TextEdit::delete(file_id.clone(), decl_range));
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

fn validate_receiver(receiver: Option<&jast::Expr>, source: &str) -> Result<(), InlineMethodError> {
    let Some(receiver) = receiver else {
        return Ok(());
    };

    match receiver {
        jast::Expr::Missing(_) | jast::Expr::This(_) | jast::Expr::Super(_) => Ok(()),
        jast::Expr::Name(name) => match name.name.as_str() {
            "this" | "super" => Ok(()),
            _ => Err(InlineMethodError::UnsupportedReceiver),
        },
        _ => {
            // Anything more complex (e.g. `foo().bar`) is not supported yet.
            let _ = source;
            Err(InlineMethodError::UnsupportedReceiver)
        }
    }
}

fn validate_method(source: &str, method: &jast::MethodDecl) -> Result<(), InlineMethodError> {
    let prefix = source
        .get(method.range.start..method.name_range.start)
        .unwrap_or("");

    if !contains_word(prefix, "private") {
        return Err(InlineMethodError::MethodNotPrivate);
    }

    if contains_word(prefix, "abstract")
        || contains_word(prefix, "native")
        || contains_word(prefix, "synchronized")
    {
        return Err(InlineMethodError::UnsupportedModifiers);
    }

    if prefix.contains('<') {
        // Extremely conservative: treat any `<` in the prefix as a type parameter list.
        return Err(InlineMethodError::UnsupportedTypeParameters);
    }

    Ok(())
}

fn contains_word(text: &str, needle: &str) -> bool {
    let bytes = text.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut i = 0usize;
    while let Some(pos) = text[i..].find(needle) {
        let start = i + pos;
        let end = start + needle_bytes.len();
        let before_ok = start == 0 || !is_ident_char_byte(bytes[start.saturating_sub(1)]);
        let after_ok = end >= bytes.len() || !is_ident_char_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = end;
    }
    false
}

fn find_method_to_inline<'a>(
    unit: &'a jast::CompilationUnit,
    name: &str,
    arity: usize,
) -> Result<MethodToInline<'a>, InlineMethodError> {
    let mut fallback_error: Option<InlineMethodError> = None;

    for ty in &unit.types {
        for member in ty.members() {
            let jast::MemberDecl::Method(method) = member else {
                continue;
            };
            if method.name != name || method.params.len() != arity {
                continue;
            }
            let Some(body) = method.body.as_ref() else {
                continue;
            };

            let mut locals: Vec<&jast::LocalVarStmt> = Vec::new();
            let mut return_expr: Option<&jast::Expr> = None;
            let mut return_count = 0usize;
            let mut unsupported = false;

            for stmt in &body.statements {
                match stmt {
                    jast::Stmt::LocalVar(local) => locals.push(local),
                    jast::Stmt::Return(ret) => {
                        return_count += 1;
                        if return_count == 1 {
                            return_expr = ret.expr.as_ref();
                        }
                    }
                    jast::Stmt::Empty(_) => {}
                    _ => {
                        unsupported = true;
                        break;
                    }
                }
            }

            if unsupported {
                fallback_error.get_or_insert(InlineMethodError::UnsupportedControlFlow);
                continue;
            }

            if return_count > 1 {
                fallback_error.get_or_insert(InlineMethodError::MultipleReturns);
                continue;
            }

            let Some(return_expr) = return_expr else {
                continue;
            };

            return Ok(MethodToInline {
                decl: method,
                params: method.params.iter().collect(),
                body,
                return_expr,
                locals,
            });
        }
    }

    Err(fallback_error.unwrap_or(InlineMethodError::MethodNotFound))
}

fn is_recursive(source: &str, method_name: &str, body: &jast::Block) -> bool {
    fn walk_stmt(source: &str, method_name: &str, stmt: &jast::Stmt) -> bool {
        match stmt {
            jast::Stmt::LocalVar(local) => local
                .initializer
                .as_ref()
                .is_some_and(|e| walk_expr(source, method_name, e)),
            jast::Stmt::Expr(expr) => walk_expr(source, method_name, &expr.expr),
            jast::Stmt::Assert(stmt) => {
                walk_expr(source, method_name, &stmt.condition)
                    || stmt
                        .message
                        .as_ref()
                        .is_some_and(|e| walk_expr(source, method_name, e))
            }
            jast::Stmt::Yield(stmt) => stmt
                .expr
                .as_ref()
                .is_some_and(|e| walk_expr(source, method_name, e)),
            jast::Stmt::Return(ret) => ret
                .expr
                .as_ref()
                .is_some_and(|e| walk_expr(source, method_name, e)),
            jast::Stmt::Block(block) => block
                .statements
                .iter()
                .any(|s| walk_stmt(source, method_name, s)),
            jast::Stmt::If(stmt) => {
                walk_expr(source, method_name, &stmt.condition)
                    || walk_stmt(source, method_name, stmt.then_branch.as_ref())
                    || stmt
                        .else_branch
                        .as_ref()
                        .is_some_and(|s| walk_stmt(source, method_name, s.as_ref()))
            }
            jast::Stmt::While(stmt) => {
                walk_expr(source, method_name, &stmt.condition)
                    || walk_stmt(source, method_name, stmt.body.as_ref())
            }
            jast::Stmt::For(stmt) => {
                stmt.init.iter().any(|s| walk_stmt(source, method_name, s))
                    || stmt
                        .condition
                        .as_ref()
                        .is_some_and(|e| walk_expr(source, method_name, e))
                    || stmt
                        .update
                        .iter()
                        .any(|e| walk_expr(source, method_name, e))
                    || walk_stmt(source, method_name, stmt.body.as_ref())
            }
            jast::Stmt::ForEach(stmt) => {
                stmt.var
                    .initializer
                    .as_ref()
                    .is_some_and(|e| walk_expr(source, method_name, e))
                    || walk_expr(source, method_name, &stmt.iterable)
                    || walk_stmt(source, method_name, stmt.body.as_ref())
            }
            jast::Stmt::Synchronized(stmt) => {
                walk_expr(source, method_name, &stmt.expr)
                    || stmt
                        .body
                        .statements
                        .iter()
                        .any(|s| walk_stmt(source, method_name, s))
            }
            jast::Stmt::Switch(stmt) => {
                walk_expr(source, method_name, &stmt.selector)
                    || stmt
                        .body
                        .statements
                        .iter()
                        .any(|s| walk_stmt(source, method_name, s))
            }
            jast::Stmt::Try(stmt) => {
                stmt.body
                    .statements
                    .iter()
                    .any(|s| walk_stmt(source, method_name, s))
                    || stmt.catches.iter().any(|catch| {
                        catch
                            .body
                            .statements
                            .iter()
                            .any(|s| walk_stmt(source, method_name, s))
                    })
                    || stmt.finally.as_ref().is_some_and(|block| {
                        block
                            .statements
                            .iter()
                            .any(|s| walk_stmt(source, method_name, s))
                    })
            }
            jast::Stmt::Throw(stmt) => walk_expr(source, method_name, &stmt.expr),
            jast::Stmt::Break(_) | jast::Stmt::Continue(_) | jast::Stmt::Empty(_) => false,
        }
    }

    fn walk_expr(source: &str, method_name: &str, expr: &jast::Expr) -> bool {
        match expr {
            jast::Expr::Call(call) => {
                if let Some((name, receiver)) = call_name_and_receiver(call) {
                    if name == method_name {
                        let is_self_receiver = match receiver {
                            None => true,
                            Some(receiver) => {
                                matches!(receiver, jast::Expr::This(_) | jast::Expr::Missing(_))
                                    || matches!(
                                        receiver,
                                        jast::Expr::Name(name) if name.name.as_str() == "this"
                                    )
                            }
                        };
                        if is_self_receiver {
                            return true;
                        }
                    }
                }
                walk_expr(source, method_name, call.callee.as_ref())
                    || call.args.iter().any(|a| walk_expr(source, method_name, a))
            }
            jast::Expr::FieldAccess(field) => walk_expr(source, method_name, &field.receiver),
            jast::Expr::ArrayAccess(access) => {
                walk_expr(source, method_name, access.array.as_ref())
                    || walk_expr(source, method_name, access.index.as_ref())
            }
            jast::Expr::MethodReference(expr) => walk_expr(source, method_name, &expr.receiver),
            jast::Expr::ConstructorReference(expr) => {
                walk_expr(source, method_name, &expr.receiver)
            }
            jast::Expr::ClassLiteral(expr) => walk_expr(source, method_name, &expr.ty),
            jast::Expr::New(expr) => expr
                .args
                .iter()
                .any(|arg| walk_expr(source, method_name, arg)),
            jast::Expr::ArrayCreation(expr) => {
                expr.dim_exprs
                    .iter()
                    .any(|dim| walk_expr(source, method_name, dim))
                    || expr
                        .initializer
                        .as_ref()
                        .is_some_and(|init| walk_expr(source, method_name, init.as_ref()))
            }
            jast::Expr::ArrayInitializer(expr) => expr
                .items
                .iter()
                .any(|item| walk_expr(source, method_name, item)),
            jast::Expr::Unary(expr) => walk_expr(source, method_name, &expr.expr),
            jast::Expr::Cast(expr) => walk_expr(source, method_name, expr.expr.as_ref()),
            jast::Expr::Binary(bin) => {
                walk_expr(source, method_name, &bin.lhs) || walk_expr(source, method_name, &bin.rhs)
            }
            jast::Expr::Instanceof(expr) => walk_expr(source, method_name, expr.expr.as_ref()),
            jast::Expr::Assign(expr) => {
                walk_expr(source, method_name, &expr.lhs)
                    || walk_expr(source, method_name, &expr.rhs)
            }
            jast::Expr::Conditional(expr) => {
                walk_expr(source, method_name, &expr.condition)
                    || walk_expr(source, method_name, &expr.then_expr)
                    || walk_expr(source, method_name, &expr.else_expr)
            }
            jast::Expr::Lambda(expr) => match &expr.body {
                jast::LambdaBody::Expr(expr) => walk_expr(source, method_name, expr.as_ref()),
                jast::LambdaBody::Block(block) => block
                    .statements
                    .iter()
                    .any(|stmt| walk_stmt(source, method_name, stmt)),
            },
            jast::Expr::Switch(expr) => {
                walk_expr(source, method_name, expr.selector.as_ref())
                    || expr
                        .body
                        .statements
                        .iter()
                        .any(|stmt| walk_stmt(source, method_name, stmt))
            }
            jast::Expr::Invalid { children, .. } => children
                .iter()
                .any(|child| walk_expr(source, method_name, child)),
            jast::Expr::Name(_)
            | jast::Expr::IntLiteral(_)
            | jast::Expr::LongLiteral(_)
            | jast::Expr::FloatLiteral(_)
            | jast::Expr::DoubleLiteral(_)
            | jast::Expr::CharLiteral(_)
            | jast::Expr::StringLiteral(_)
            | jast::Expr::TextBlock(_)
            | jast::Expr::BoolLiteral(_)
            | jast::Expr::NullLiteral(_)
            | jast::Expr::This(_)
            | jast::Expr::Super(_)
            | jast::Expr::Missing(_) => false,
        }
    }

    let _ = source;
    body.statements
        .iter()
        .any(|s| walk_stmt(source, method_name, s))
}

fn call_name_and_receiver<'a>(
    call: &'a jast::CallExpr,
) -> Option<(&'a str, Option<&'a jast::Expr>)> {
    match call.callee.as_ref() {
        jast::Expr::Name(name) => Some((name.name.as_str(), None)),
        jast::Expr::FieldAccess(field) => {
            Some((field.name.as_str(), Some(field.receiver.as_ref())))
        }
        _ => None,
    }
}

fn find_invocation_at_offset<'a>(
    unit: &'a jast::CompilationUnit,
    source: &str,
    offset: usize,
) -> Option<Invocation<'a>> {
    let mut best: Option<Invocation<'a>> = None;

    for ty in &unit.types {
        for member in ty.members() {
            let jast::MemberDecl::Method(method) = member else {
                continue;
            };
            let Some(body) = method.body.as_ref() else {
                continue;
            };

            // Collect locals in the enclosing method for collision avoidance.
            let locals = collect_local_names(body);

            for stmt in &body.statements {
                let jast::Stmt::Return(ret) = stmt else {
                    continue;
                };
                let Some(expr) = ret.expr.as_ref() else {
                    continue;
                };
                let jast::Expr::Call(call) = expr else {
                    continue;
                };

                let range = call.range;
                if !(range.start <= offset && offset < range.end) {
                    continue;
                }

                let (name, receiver) = call_name_and_receiver(call)?;
                let args: Vec<&jast::Expr> = call.args.iter().collect();
                let stmt_range = ret.range;
                let stmt_indent = indentation_at(source, stmt_range.start);

                let candidate = Invocation {
                    name,
                    args,
                    receiver,
                    call_range: range,
                    stmt_range,
                    stmt_indent,
                    enclosing_method_locals: locals.clone(),
                };
                best = Some(match best {
                    Some(prev) if span_len(prev.call_range) <= span_len(candidate.call_range) => {
                        prev
                    }
                    _ => candidate,
                });
            }
        }
    }

    best
}

fn find_all_return_call_sites<'a>(
    unit: &'a jast::CompilationUnit,
    source: &str,
    name: &str,
    arity: usize,
) -> Result<Vec<Invocation<'a>>, InlineMethodError> {
    let mut out = Vec::new();
    for ty in &unit.types {
        for member in ty.members() {
            let jast::MemberDecl::Method(method) = member else {
                continue;
            };
            let Some(body) = method.body.as_ref() else {
                continue;
            };
            let locals = collect_local_names(body);

            for stmt in &body.statements {
                let jast::Stmt::Return(ret) = stmt else {
                    continue;
                };
                let Some(expr) = ret.expr.as_ref() else {
                    continue;
                };
                let jast::Expr::Call(call) = expr else {
                    continue;
                };
                let Some((call_name, receiver)) = call_name_and_receiver(call) else {
                    continue;
                };
                if call_name != name || call.args.len() != arity {
                    continue;
                }

                validate_receiver(receiver, source)?;

                out.push(Invocation {
                    name: call_name,
                    args: call.args.iter().collect(),
                    receiver,
                    call_range: call.range,
                    stmt_range: ret.range,
                    stmt_indent: indentation_at(source, ret.range.start),
                    enclosing_method_locals: locals.clone(),
                });
            }
        }
    }
    Ok(out)
}

fn collect_local_names(block: &jast::Block) -> HashSet<String> {
    let mut out = HashSet::new();

    fn walk_stmt(stmt: &jast::Stmt, out: &mut HashSet<String>) {
        match stmt {
            jast::Stmt::LocalVar(local) => {
                out.insert(local.name.clone());
            }
            jast::Stmt::Block(block) => {
                for stmt in &block.statements {
                    walk_stmt(stmt, out);
                }
            }
            jast::Stmt::If(stmt) => {
                walk_stmt(stmt.then_branch.as_ref(), out);
                if let Some(else_branch) = &stmt.else_branch {
                    walk_stmt(else_branch.as_ref(), out);
                }
            }
            jast::Stmt::While(stmt) => walk_stmt(stmt.body.as_ref(), out),
            jast::Stmt::For(stmt) => {
                for init_stmt in &stmt.init {
                    walk_stmt(init_stmt, out);
                }
                walk_stmt(stmt.body.as_ref(), out);
            }
            jast::Stmt::ForEach(stmt) => {
                out.insert(stmt.var.name.clone());
                walk_stmt(stmt.body.as_ref(), out);
            }
            jast::Stmt::Synchronized(stmt) => {
                for stmt in &stmt.body.statements {
                    walk_stmt(stmt, out);
                }
            }
            jast::Stmt::Switch(stmt) => {
                for stmt in &stmt.body.statements {
                    walk_stmt(stmt, out);
                }
            }
            jast::Stmt::Try(stmt) => {
                for stmt in &stmt.body.statements {
                    walk_stmt(stmt, out);
                }
                for catch in &stmt.catches {
                    out.insert(catch.param.name.clone());
                    for stmt in &catch.body.statements {
                        walk_stmt(stmt, out);
                    }
                }
                if let Some(finally) = &stmt.finally {
                    for stmt in &finally.statements {
                        walk_stmt(stmt, out);
                    }
                }
            }
            jast::Stmt::Expr(_)
            | jast::Stmt::Assert(_)
            | jast::Stmt::Yield(_)
            | jast::Stmt::Return(_)
            | jast::Stmt::Throw(_)
            | jast::Stmt::Break(_)
            | jast::Stmt::Continue(_)
            | jast::Stmt::Empty(_) => {}
        }
    }

    for stmt in &block.statements {
        walk_stmt(stmt, &mut out);
    }
    out
}

fn inline_at_site(
    source: &str,
    newline: &str,
    method: &MethodToInline<'_>,
    site: &Invocation<'_>,
) -> String {
    let mut used_names = site.enclosing_method_locals.clone();

    // 1) Parameter temps (`<param>_arg`) to preserve evaluation order.
    let mut param_map: HashMap<String, String> = HashMap::new();
    let mut arg_lines: Vec<String> = Vec::new();
    for (param, arg) in method.params.iter().zip(site.args.iter()) {
        let base = format!("{}_arg", param.name);
        let temp = make_unique(&base, &mut used_names);
        param_map.insert(param.name.clone(), temp.clone());

        let arg_text = slice_span(source, arg.range());
        arg_lines.push(format!(
            "{indent}{ty} {name} = {expr};",
            indent = site.stmt_indent,
            ty = param.ty.text,
            name = temp,
            expr = arg_text.trim()
        ));
    }

    // 2) Inline local declarations from the callee method, renaming collisions.
    let mut local_map: HashMap<String, String> = HashMap::new();
    let mut inlined_lines: Vec<String> = Vec::new();
    for local in &method.locals {
        let base = local.name.clone();
        let name = if used_names.contains(&base) {
            make_unique(&format!("{base}_inlined"), &mut used_names)
        } else {
            used_names.insert(base.clone());
            base.clone()
        };
        local_map.insert(local.name.clone(), name.clone());

        let init_text = local
            .initializer
            .as_ref()
            .map(|expr| slice_span(source, expr.range()).trim().to_string());
        let init_text = init_text.as_deref().unwrap_or_default().to_string();

        let mut mapping = combined_mapping(&param_map, &local_map);
        let init_text = substitute_idents(&init_text, &mut mapping);

        inlined_lines.push(format!(
            "{indent}{ty} {name} = {init};",
            indent = site.stmt_indent,
            ty = local.ty.text,
            name = name,
            init = init_text
        ));
    }

    // 3) Return statement.
    let return_text = slice_span(source, method.return_expr.range())
        .trim()
        .to_string();
    let mut mapping = combined_mapping(&param_map, &local_map);
    let return_text = substitute_idents(&return_text, &mut mapping);

    let mut lines: Vec<String> = Vec::new();
    lines.extend(arg_lines);
    lines.extend(inlined_lines);
    lines.push(format!(
        "{indent}return {expr};",
        indent = site.stmt_indent,
        expr = return_text
    ));

    lines.join(newline)
}

fn combined_mapping(
    param_map: &HashMap<String, String>,
    local_map: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in param_map {
        out.insert(k.clone(), v.clone());
    }
    for (k, v) in local_map {
        out.insert(k.clone(), v.clone());
    }
    out
}

fn substitute_idents(text: &str, mapping: &mut HashMap<String, String>) -> String {
    if mapping.is_empty() || text.is_empty() {
        return text.to_string();
    }

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if is_ident_char_byte(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_char_byte(bytes[i]) {
                i += 1;
            }
            out.push_str(&text[last..start]);
            let ident = &text[start..i];
            if let Some(repl) = mapping.get(ident) {
                out.push_str(repl);
            } else {
                out.push_str(ident);
            }
            last = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&text[last..]);
    out
}

fn make_unique(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let mut i = 2usize;
    loop {
        let candidate = format!("{base}{i}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        i += 1;
    }
}

fn method_decl_deletion_range(source: &str, method: &jast::MethodDecl) -> TextRange {
    // Include trailing newline if present.
    let start = line_start(source, method.range.start);
    let mut end = method.range.end;
    if source.get(end..).is_some_and(|rest| rest.starts_with('\n')) {
        end += 1;
    }
    TextRange::new(start, end)
}

fn indentation_at(text: &str, offset: usize) -> String {
    let start = line_start(text, offset);
    let mut out = String::new();
    for ch in text[start..].chars() {
        if ch == ' ' || ch == '\t' {
            out.push(ch);
        } else {
            break;
        }
    }
    out
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

fn span_len(span: nova_types::Span) -> usize {
    span.end.saturating_sub(span.start)
}

fn slice_span<'a>(text: &'a str, span: nova_types::Span) -> &'a str {
    text.get(span.start..span.end).unwrap_or("")
}
