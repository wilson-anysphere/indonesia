use std::collections::HashSet;

use nova_core::{Line, LineIndex};
use nova_syntax::{parse_java, SyntaxKind, SyntaxNode, SyntaxToken};

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
/// Breakpoint sites are discovered by walking Nova's error-resilient syntax tree
/// (`nova_syntax::parse_java`) and extracting statement-level line starts in a
/// way that is compatible with JDWP's notion of breakable line numbers.
pub fn collect_breakpoint_sites(java_source: &str) -> Vec<BreakpointSite> {
    let parse = parse_java(java_source);
    let root = parse.syntax();
    let line_index = LineIndex::new(java_source);

    let package = extract_package_name(&root);

    let mut sites = Vec::new();
    let mut type_stack = Vec::new();

    for top_level in root.children().filter(is_type_declaration) {
        collect_in_type(
            &top_level,
            &package,
            &line_index,
            &mut type_stack,
            &mut sites,
        );
    }

    // Output ordering: by line (ascending) while preserving source insertion
    // order within a line, then globally de-duplicate identical sites.
    sites.sort_by_key(|site| site.line);

    let mut seen = HashSet::new();
    sites
        .into_iter()
        .filter(|site| {
            seen.insert((
                site.line,
                site.enclosing_class.clone(),
                site.enclosing_method.clone(),
            ))
        })
        .collect()
}

fn collect_in_type(
    type_decl: &SyntaxNode,
    package: &Option<String>,
    line_index: &LineIndex,
    type_stack: &mut Vec<String>,
    sites: &mut Vec<BreakpointSite>,
) {
    let type_name = extract_type_name(type_decl);
    if let Some(name) = &type_name {
        type_stack.push(name.clone());
    }

    let enclosing_class = build_binary_class_name(package, type_stack);

    let Some(body) = type_decl.children().find(is_type_body) else {
        if type_name.is_some() {
            type_stack.pop();
        }
        return;
    };

    for member in body.children() {
        match member.kind() {
            SyntaxKind::MethodDeclaration => {
                let method_name = extract_method_name(&member);
                if let Some(block) = member.children().find(|n| n.kind() == SyntaxKind::Block) {
                    scan_executable_region(
                        &block,
                        line_index,
                        enclosing_class.as_deref(),
                        method_name.as_deref(),
                        sites,
                    );
                }
            }
            SyntaxKind::ConstructorDeclaration => {
                if let Some(block) = member.children().find(|n| n.kind() == SyntaxKind::Block) {
                    scan_executable_region(
                        &block,
                        line_index,
                        enclosing_class.as_deref(),
                        Some("<init>"),
                        sites,
                    );
                }
            }
            SyntaxKind::CompactConstructorDeclaration => {
                if let Some(block) = member.children().find(|n| n.kind() == SyntaxKind::Block) {
                    scan_executable_region(
                        &block,
                        line_index,
                        enclosing_class.as_deref(),
                        Some("<init>"),
                        sites,
                    );
                }
            }
            SyntaxKind::InitializerBlock => {
                let method = if has_static_modifier(&member) {
                    "<clinit>"
                } else {
                    "<init>"
                };
                if let Some(block) = member.children().find(|n| n.kind() == SyntaxKind::Block) {
                    scan_executable_region(
                        &block,
                        line_index,
                        enclosing_class.as_deref(),
                        Some(method),
                        sites,
                    );
                }
            }
            SyntaxKind::FieldDeclaration => {
                let method = if has_static_modifier(&member) {
                    "<clinit>"
                } else {
                    "<init>"
                };
                for declarator in member
                    .descendants()
                    .filter(|n| n.kind() == SyntaxKind::VariableDeclarator)
                {
                    if has_initializer(&declarator) {
                        scan_executable_region(
                            &declarator,
                            line_index,
                            enclosing_class.as_deref(),
                            Some(method),
                            sites,
                        );
                    }
                }
            }
            kind if is_type_declaration(&member) => {
                collect_in_type(&member, package, line_index, type_stack, sites);
            }
            _ => {}
        }
    }

    if type_name.is_some() {
        type_stack.pop();
    }
}

fn extract_package_name(root: &SyntaxNode) -> Option<String> {
    let package_decl = root
        .children()
        .find(|n| n.kind() == SyntaxKind::PackageDeclaration)?;
    let name_node = package_decl
        .children()
        .find(|n| n.kind() == SyntaxKind::Name)?;

    let segments: Vec<String> = name_node
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind().is_identifier_like())
        .map(|t| t.text().to_string())
        .collect();

    match segments.as_slice() {
        [] => None,
        segs => Some(segs.join(".")),
    }
}

fn is_type_declaration(node: &SyntaxNode) -> bool {
    matches!(
        node.kind(),
        SyntaxKind::ClassDeclaration
            | SyntaxKind::InterfaceDeclaration
            | SyntaxKind::EnumDeclaration
            | SyntaxKind::RecordDeclaration
            | SyntaxKind::AnnotationTypeDeclaration
    )
}

fn is_type_body(node: &SyntaxNode) -> bool {
    matches!(
        node.kind(),
        SyntaxKind::ClassBody
            | SyntaxKind::InterfaceBody
            | SyntaxKind::EnumBody
            | SyntaxKind::RecordBody
            | SyntaxKind::AnnotationBody
    )
}

fn extract_type_name(type_decl: &SyntaxNode) -> Option<String> {
    type_decl
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind().is_identifier_like())
        .last()
        .map(|t| t.text().to_string())
}

fn build_binary_class_name(package: &Option<String>, type_stack: &[String]) -> Option<String> {
    let binary = match type_stack {
        [] => return None,
        segs => segs.join("$"),
    };

    match package.as_deref() {
        Some(pkg) if !pkg.is_empty() => Some(format!("{pkg}.{binary}")),
        _ => Some(binary),
    }
}

fn extract_method_name(method_decl: &SyntaxNode) -> Option<String> {
    method_decl
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind().is_identifier_like())
        .map(|t| t.text().to_string())
}

fn has_static_modifier(node: &SyntaxNode) -> bool {
    node.children()
        .find(|n| n.kind() == SyntaxKind::Modifiers)
        .map(|mods| {
            mods.descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .any(|t| t.kind() == SyntaxKind::StaticKw)
        })
        .unwrap_or(false)
}

fn has_initializer(declarator: &SyntaxNode) -> bool {
    declarator
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == SyntaxKind::Eq)
}

fn scan_executable_region(
    region: &SyntaxNode,
    line_index: &LineIndex,
    enclosing_class: Option<&str>,
    enclosing_method: Option<&str>,
    sites: &mut Vec<BreakpointSite>,
) {
    let mut marked_lines = HashSet::<Line>::new();
    scan_node(
        region,
        line_index,
        enclosing_class,
        enclosing_method,
        sites,
        &mut marked_lines,
        None,
    );
}

fn scan_node(
    node: &SyntaxNode,
    line_index: &LineIndex,
    enclosing_class: Option<&str>,
    enclosing_method: Option<&str>,
    sites: &mut Vec<BreakpointSite>,
    marked_lines: &mut HashSet<Line>,
    skip_line: Option<Line>,
) {
    for element in node.children_with_tokens() {
        if let Some(token) = element.as_token() {
            mark_token(
                token,
                line_index,
                enclosing_class,
                enclosing_method,
                sites,
                marked_lines,
                skip_line,
            );
            continue;
        }

        let Some(child) = element.as_node() else {
            continue;
        };

        if child.kind() == SyntaxKind::LambdaExpression {
            scan_lambda_expression(
                child,
                line_index,
                enclosing_class,
                enclosing_method,
                sites,
                marked_lines,
                skip_line,
            );
            continue;
        }

        scan_node(
            child,
            line_index,
            enclosing_class,
            enclosing_method,
            sites,
            marked_lines,
            skip_line,
        );
    }
}

fn scan_lambda_expression(
    lambda: &SyntaxNode,
    line_index: &LineIndex,
    enclosing_class: Option<&str>,
    enclosing_method: Option<&str>,
    sites: &mut Vec<BreakpointSite>,
    marked_lines: &mut HashSet<Line>,
    skip_line: Option<Line>,
) {
    let mut arrow_line: Option<Line> = None;

    // Header: parameters + `->` stay in the current method context.
    //
    // Lambdas are parsed into a structured subtree in `nova-syntax`:
    //
    //   LambdaExpression
    //     LambdaParameters
    //     Arrow
    //     LambdaBody
    //
    // We want tokens in the parameters + arrow to be attributed to the enclosing method, while
    // tokens in the body are attributed to the synthetic lambda method (modeled here as "no
    // enclosing method"). Previously, parameters were tokens directly under `LambdaExpression`,
    // but they're now nested under `LambdaParameters`, so we explicitly scan that node here.
    if let Some(params) = lambda
        .children()
        .find(|node| node.kind() == SyntaxKind::LambdaParameters)
    {
        scan_node(
            &params,
            line_index,
            enclosing_class,
            enclosing_method,
            sites,
            marked_lines,
            skip_line,
        );
    }

    for element in lambda.children_with_tokens() {
        if let Some(token) = element.as_token() {
            if arrow_line.is_none() && token.kind() == SyntaxKind::Arrow {
                arrow_line = Some(token_line(line_index, token));
            }
            mark_token(
                token,
                line_index,
                enclosing_class,
                enclosing_method,
                sites,
                marked_lines,
                skip_line,
            );
        }
    }

    let lambda_body_skip_line = match (enclosing_method, arrow_line) {
        (Some(_), Some(line)) if marked_lines.contains(&line) => Some(line),
        _ => None,
    };
    let lambda_body_skip_line = lambda_body_skip_line.or(skip_line);

    if let Some(body) = lambda
        .children()
        .find(|node| node.kind() == SyntaxKind::LambdaBody)
    {
        let mut lambda_lines = HashSet::<Line>::new();
        scan_node(
            &body,
            line_index,
            enclosing_class,
            None,
            sites,
            &mut lambda_lines,
            lambda_body_skip_line,
        );
    }
}

fn mark_token(
    token: &SyntaxToken,
    line_index: &LineIndex,
    enclosing_class: Option<&str>,
    enclosing_method: Option<&str>,
    sites: &mut Vec<BreakpointSite>,
    marked_lines: &mut HashSet<Line>,
    skip_line: Option<Line>,
) {
    let kind = token.kind();
    if kind.is_trivia() || is_structural_punctuation(kind) {
        return;
    }

    let line = token_line(line_index, token);
    if skip_line == Some(line) {
        return;
    }

    if !marked_lines.insert(line) {
        return;
    }

    sites.push(BreakpointSite {
        line,
        enclosing_class: enclosing_class.map(str::to_string),
        enclosing_method: enclosing_method.map(str::to_string),
    });
}

fn token_line(line_index: &LineIndex, token: &SyntaxToken) -> Line {
    let start = token.text_range().start();
    let lc = line_index.line_col(start);
    lc.line + 1
}

fn is_structural_punctuation(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::LBrace
            | SyntaxKind::RBrace
            | SyntaxKind::LParen
            | SyntaxKind::RParen
            | SyntaxKind::LBracket
            | SyntaxKind::RBracket
            | SyntaxKind::Semicolon
            | SyntaxKind::Comma
            | SyntaxKind::Dot
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_class_uses_binary_name() {
        let java = r#"package p;
class Outer {
  class Inner {
    void m(){
      int x=0;
    }
  }
}"#;

        let sites = collect_breakpoint_sites(java);
        assert!(
            sites.iter().any(|site| {
                site.line == 5
                    && site.enclosing_class.as_deref() == Some("p.Outer$Inner")
                    && site.enclosing_method.as_deref() == Some("m")
            }),
            "expected breakpoint site inside nested method: {sites:?}"
        );
    }

    #[test]
    fn constructor_uses_init_name() {
        let java = r#"class C {
  C(){
    int x=0;
  }
}"#;

        let sites = collect_breakpoint_sites(java);
        assert!(
            sites.iter().any(|site| {
                site.line == 3
                    && site.enclosing_class.as_deref() == Some("C")
                    && site.enclosing_method.as_deref() == Some("<init>")
            }),
            "expected <init> breakpoint site: {sites:?}"
        );
    }

    #[test]
    fn compact_constructor_uses_init_name() {
        let java = r#"record Point(int x, int y) {
  Point {
    int z=0;
  }
}"#;

        let sites = collect_breakpoint_sites(java);
        assert!(
            sites.iter().any(|site| {
                site.line == 3
                    && site.enclosing_class.as_deref() == Some("Point")
                    && site.enclosing_method.as_deref() == Some("<init>")
            }),
            "expected <init> breakpoint site in compact constructor: {sites:?}"
        );
    }

    #[test]
    fn static_initializer_uses_clinit_name() {
        let java = r#"class C {
  static {
    int x=0;
  }
}"#;

        let sites = collect_breakpoint_sites(java);
        assert!(
            sites.iter().any(|site| {
                site.line == 3
                    && site.enclosing_class.as_deref() == Some("C")
                    && site.enclosing_method.as_deref() == Some("<clinit>")
            }),
            "expected <clinit> breakpoint site: {sites:?}"
        );
    }

    #[test]
    fn field_initializers_map_to_clinit_or_init() {
        let java = r#"class C {
  static int X = foo();
  int y = bar();
}"#;

        let sites = collect_breakpoint_sites(java);
        assert!(
            sites.iter().any(|site| {
                site.line == 2
                    && site.enclosing_class.as_deref() == Some("C")
                    && site.enclosing_method.as_deref() == Some("<clinit>")
            }),
            "expected static field initializer in <clinit>: {sites:?}"
        );
        assert!(
            sites.iter().any(|site| {
                site.line == 3
                    && site.enclosing_class.as_deref() == Some("C")
                    && site.enclosing_method.as_deref() == Some("<init>")
            }),
            "expected instance field initializer in <init>: {sites:?}"
        );
    }

    #[test]
    fn lambda_body_has_no_enclosing_method() {
        let java = r#"class C {
  void f(){
    Runnable r = () -> {
      int x=0;
    };
  }
}"#;

        let sites = collect_breakpoint_sites(java);
        assert!(
            sites.iter().any(|site| {
                site.line == 4
                    && site.enclosing_class.as_deref() == Some("C")
                    && site.enclosing_method.is_none()
            }),
            "expected lambda body breakpoint with method None: {sites:?}"
        );
    }

    #[test]
    fn lambda_parameters_keep_enclosing_method_context() {
        let java = r#"class C {
  void f(){
    java.util.function.IntBinaryOperator add =
      (int x,
       int y) -> x + y;
  }
}"#;

        let sites = collect_breakpoint_sites(java);
        assert!(
            sites.iter().any(|site| {
                site.line == 4
                    && site.enclosing_class.as_deref() == Some("C")
                    && site.enclosing_method.as_deref() == Some("f")
            }),
            "expected lambda parameter line to be attributed to enclosing method: {sites:?}"
        );
    }
}
