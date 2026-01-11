use nova_core::Line;
use nova_core::{LineIndex, TextSize};
use nova_syntax::{
    AstNode, ClassDeclaration, ClassMember, CompilationUnit, FieldDeclaration, LambdaBody,
    LambdaExpression, Modifiers, Name, SyntaxKind, SyntaxNode,
};

/// A valid location for a line breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointSite {
    pub line: Line,
    pub enclosing_class: Option<String>,
    pub enclosing_method: Option<String>,
}

/// Collect a conservative set of executable line breakpoint sites from a Java
/// source file.
///
/// This uses Nova's Java syntax tree to find conservative breakpoint sites. The
/// returned sites are stable and sorted in source order.
pub fn collect_breakpoint_sites(java_source: &str) -> Vec<BreakpointSite> {
    let line_index = LineIndex::new(java_source);
    let parse = nova_syntax::parse_java(java_source);
    let root = parse.syntax();
    let Some(unit) = CompilationUnit::cast(root) else {
        return Vec::new();
    };

    let package = unit
        .package()
        .and_then(|pkg| pkg.name())
        .map(|name| name_to_string(&name));

    let mut acc = Vec::new();
    let mut class_stack: Vec<String> = Vec::new();

    for decl in unit.type_declarations() {
        match decl {
            nova_syntax::TypeDeclaration::ClassDeclaration(class) => {
                collect_class_sites(&class, package.as_deref(), &mut class_stack, &mut acc)
            }
            _ => {}
        }
    }

    acc.sort_by_key(|site| site.offset);
    acc.into_iter()
        .map(|site| BreakpointSite {
            line: dap_line(&line_index, site.offset),
            enclosing_class: site.enclosing_class,
            enclosing_method: site.enclosing_method,
        })
        .collect()
}

#[derive(Debug, Clone)]
struct Site {
    offset: TextSize,
    enclosing_class: Option<String>,
    enclosing_method: Option<String>,
}

fn collect_class_sites(
    class: &ClassDeclaration,
    package: Option<&str>,
    class_stack: &mut Vec<String>,
    out: &mut Vec<Site>,
) {
    let Some(name) = class.name_token().map(|tok| tok.text().to_string()) else {
        return;
    };

    class_stack.push(name);
    let enclosing_class = Some(qualify_class_name(package, class_stack));

    let Some(body) = class.body() else {
        class_stack.pop();
        return;
    };

    for member in body.members() {
        match member {
            ClassMember::FieldDeclaration(field) => {
                collect_field_sites(&field, enclosing_class.as_ref(), out);
            }
            ClassMember::MethodDeclaration(method) => collect_method_like_sites(
                enclosing_class.as_ref(),
                method.name_token().map(|tok| tok.text().to_string()),
                method.body().map(|b| b.syntax().clone()),
                out,
            ),
            ClassMember::ConstructorDeclaration(ctor) => collect_method_like_sites(
                enclosing_class.as_ref(),
                Some("<init>".to_string()),
                ctor.body().map(|b| b.syntax().clone()),
                out,
            ),
            ClassMember::InitializerBlock(init) => collect_method_like_sites(
                enclosing_class.as_ref(),
                Some(if is_static(init.modifiers()) {
                    "<clinit>".to_string()
                } else {
                    "<init>".to_string()
                }),
                init.body().map(|b| b.syntax().clone()),
                out,
            ),
            ClassMember::ClassDeclaration(inner) => {
                collect_class_sites(&inner, package, class_stack, out);
            }
            _ => {}
        }
    }

    class_stack.pop();
}

fn collect_field_sites(
    field: &FieldDeclaration,
    enclosing_class: Option<&String>,
    out: &mut Vec<Site>,
) {
    let Some(enclosing_class) = enclosing_class else {
        return;
    };

    let method = if is_static(field.modifiers()) {
        Some("<clinit>".to_string())
    } else {
        Some("<init>".to_string())
    };

    let Some(decls) = field.declarators() else {
        return;
    };

    for decl in decls.declarators() {
        let Some(init) = decl.initializer() else {
            continue;
        };

        // Field initializers execute as part of <clinit> / <init>.
        out.push(Site {
            offset: init.syntax().text_range().start(),
            enclosing_class: Some(enclosing_class.clone()),
            enclosing_method: method.clone(),
        });

        // If the initializer contains a lambda, statements inside the lambda body
        // should not be attributed to the enclosing method.
        collect_executable_sites_in_node(init.syntax(), enclosing_class, method.as_deref(), out);
    }
}

fn collect_method_like_sites(
    enclosing_class: Option<&String>,
    enclosing_method: Option<String>,
    body: Option<SyntaxNode>,
    out: &mut Vec<Site>,
) {
    let (Some(enclosing_class), Some(body)) = (enclosing_class, body) else {
        return;
    };

    collect_executable_sites_in_node(&body, enclosing_class, enclosing_method.as_deref(), out);
}

fn collect_executable_sites_in_node(
    node: &SyntaxNode,
    enclosing_class: &str,
    enclosing_method: Option<&str>,
    out: &mut Vec<Site>,
) {
    for child in node.children() {
        match child.kind() {
            SyntaxKind::LambdaExpression => {
                if let Some(lambda) = LambdaExpression::cast(child.clone()) {
                    if let Some(body) = lambda.body() {
                        collect_lambda_body_sites(&body, enclosing_class, out);
                    }
                }
                continue;
            }
            SyntaxKind::ClassDeclaration
            | SyntaxKind::InterfaceDeclaration
            | SyntaxKind::EnumDeclaration
            | SyntaxKind::RecordDeclaration
            | SyntaxKind::AnnotationTypeDeclaration => {
                // Nested type declarations are their own JVM classes.
                continue;
            }
            kind if is_executable_statement_kind(kind) => out.push(Site {
                offset: child.text_range().start(),
                enclosing_class: Some(enclosing_class.to_string()),
                enclosing_method: enclosing_method.map(|s| s.to_string()),
            }),
            _ => {}
        }

        collect_executable_sites_in_node(&child, enclosing_class, enclosing_method, out);
    }
}

fn collect_lambda_body_sites(body: &LambdaBody, enclosing_class: &str, out: &mut Vec<Site>) {
    match body {
        LambdaBody::Block(block) => {
            collect_executable_sites_in_node(block.syntax(), enclosing_class, None, out)
        }
        LambdaBody::Expression(expr) => {
            out.push(Site {
                offset: expr.syntax().text_range().start(),
                enclosing_class: Some(enclosing_class.to_string()),
                enclosing_method: None,
            });
            collect_executable_sites_in_node(expr.syntax(), enclosing_class, None, out);
        }
    }
}

fn is_static(modifiers: Option<Modifiers>) -> bool {
    modifiers.is_some_and(|mods| {
        mods.syntax()
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .any(|tok| tok.kind() == SyntaxKind::StaticKw)
    })
}

fn is_executable_statement_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::LabeledStatement
            | SyntaxKind::IfStatement
            | SyntaxKind::SwitchStatement
            | SyntaxKind::ForStatement
            | SyntaxKind::WhileStatement
            | SyntaxKind::DoWhileStatement
            | SyntaxKind::SynchronizedStatement
            | SyntaxKind::TryStatement
            | SyntaxKind::AssertStatement
            | SyntaxKind::ReturnStatement
            | SyntaxKind::ThrowStatement
            | SyntaxKind::BreakStatement
            | SyntaxKind::ContinueStatement
            | SyntaxKind::LocalVariableDeclarationStatement
            | SyntaxKind::ExpressionStatement
    )
}

fn qualify_class_name(package: Option<&str>, class_stack: &[String]) -> String {
    let nested = class_stack.join("$");
    match package {
        Some(pkg) if !pkg.is_empty() => format!("{pkg}.{nested}"),
        _ => nested,
    }
}

fn name_to_string(name: &Name) -> String {
    name.syntax()
        .children_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|tok| tok.kind() == SyntaxKind::Dot || tok.kind().is_identifier_like())
        .fold(String::new(), |mut acc, tok| {
            acc.push_str(tok.text());
            acc
        })
}

fn dap_line(line_index: &LineIndex, offset: TextSize) -> Line {
    line_index.line_col(offset).line.saturating_add(1)
}
