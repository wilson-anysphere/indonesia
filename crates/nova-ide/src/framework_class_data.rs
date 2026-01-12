//! Best-effort extraction of `nova_hir::framework::ClassData` from Java source.
//!
//! Framework analyzers (Lombok, Spring, etc.) often need a lightweight view of a
//! class' annotations and members without requiring the full semantic engine.
//! This module provides a small, resilient parser for that purpose.

use nova_hir::framework::{Annotation, ClassData, ConstructorData, FieldData, MethodData};
use nova_syntax::ast::{self as syntax_ast, AstNode};
use nova_syntax::SyntaxKind;
use nova_types::{Parameter, PrimitiveType, Span, Type};

/// Extract all `ClassData` instances found in `source`.
///
/// This is intentionally best-effort:
/// - malformed input yields an empty/partial list
/// - individual malformed members are skipped
/// - the function never panics
#[must_use]
pub fn extract_classes_from_source(source: &str) -> Vec<ClassData> {
    let mut classes = Vec::new();
    let parse = nova_syntax::parse_java(source);

    for node in parse.syntax().descendants() {
        if let Some(class) = syntax_ast::ClassDeclaration::cast(node.clone()) {
            if let Some(class) = parse_class_declaration(class, source) {
                classes.push(class);
            }
            continue;
        }

        if let Some(interface) = syntax_ast::InterfaceDeclaration::cast(node) {
            if let Some(interface) = parse_interface_declaration(interface, source) {
                classes.push(interface);
            }
        }
    }

    classes
}

fn parse_class_declaration(node: syntax_ast::ClassDeclaration, source: &str) -> Option<ClassData> {
    let modifiers = node.modifiers();
    let annotations = modifiers
        .as_ref()
        .map(collect_annotations)
        .unwrap_or_default();

    let class_name = node.name_token()?.text().to_string();

    let body = node.body()?;
    let mut fields = Vec::new();
    let mut methods = Vec::new();
    let mut constructors = Vec::new();

    for member in body.members() {
        match member {
            syntax_ast::ClassMember::FieldDeclaration(field) => {
                let mut parsed = parse_field_declaration(field, source);
                fields.append(&mut parsed);
            }
            syntax_ast::ClassMember::MethodDeclaration(method) => {
                if let Some(method) = parse_method_declaration(method, source) {
                    methods.push(method);
                }
            }
            syntax_ast::ClassMember::ConstructorDeclaration(ctor) => {
                if let Some(ctor) = parse_constructor_declaration(ctor, source) {
                    constructors.push(ctor);
                }
            }
            _ => {}
        }
    }

    Some(ClassData {
        name: class_name,
        annotations,
        fields,
        methods,
        constructors,
    })
}

fn parse_interface_declaration(
    node: syntax_ast::InterfaceDeclaration,
    source: &str,
) -> Option<ClassData> {
    let modifiers = node.modifiers();
    let annotations = modifiers
        .as_ref()
        .map(collect_annotations)
        .unwrap_or_default();

    let interface_name = node.name_token()?.text().to_string();
    let body = node.body()?;
    let mut fields = Vec::new();
    let mut methods = Vec::new();
    let constructors = Vec::new();

    for member in body.members() {
        match member {
            syntax_ast::ClassMember::FieldDeclaration(field) => {
                let mut parsed = parse_field_declaration(field, source);
                fields.append(&mut parsed);
            }
            syntax_ast::ClassMember::MethodDeclaration(method) => {
                if let Some(method) = parse_method_declaration(method, source) {
                    methods.push(method);
                }
            }
            _ => {}
        }
    }

    Some(ClassData {
        name: interface_name,
        annotations,
        fields,
        methods,
        constructors,
    })
}

fn parse_field_declaration(node: syntax_ast::FieldDeclaration, source: &str) -> Vec<FieldData> {
    let modifiers = node.modifiers();
    let annotations = modifiers
        .as_ref()
        .map(collect_annotations)
        .unwrap_or_default();

    let (is_static, is_final) = modifiers
        .as_ref()
        .map(modifier_flags)
        .unwrap_or((false, false));

    let ty = node
        .ty()
        .map(|n| parse_type(node_text(source, n.syntax())))
        .unwrap_or(Type::Unknown);

    let mut out = Vec::new();
    for declarator in node.declarators() {
        let Some(name_node) = declarator.name_token() else {
            continue;
        };
        let name = name_node.text().to_string();
        out.push(FieldData {
            name,
            ty: ty.clone(),
            is_static,
            is_final,
            annotations: annotations.clone(),
        });
    }
    out
}

fn parse_method_declaration(
    node: syntax_ast::MethodDeclaration,
    source: &str,
) -> Option<MethodData> {
    let modifiers = node.modifiers();
    let is_static = modifiers
        .as_ref()
        .is_some_and(|m| modifier_contains_keyword(m, SyntaxKind::StaticKw));

    let name = node.name_token()?.text().to_string();

    let return_type = if node
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|tok| tok.kind() == SyntaxKind::VoidKw)
    {
        Type::Void
    } else {
        node.return_type()
            .map(|n| parse_type(node_text(source, n.syntax())))
            .unwrap_or(Type::Unknown)
    };

    let params = parse_formal_parameters(node.parameter_list(), source);

    Some(MethodData {
        name,
        return_type,
        params,
        is_static,
    })
}

fn parse_constructor_declaration(
    node: syntax_ast::ConstructorDeclaration,
    source: &str,
) -> Option<ConstructorData> {
    let params = parse_formal_parameters(node.parameter_list(), source);
    Some(ConstructorData { params })
}

fn parse_formal_parameters(
    node: Option<syntax_ast::ParameterList>,
    source: &str,
) -> Vec<Parameter> {
    let mut out = Vec::new();
    let Some(node) = node else {
        return out;
    };
    for child in node.parameters() {
        let Some(name_node) = child.name_token() else {
            continue;
        };
        let name = name_node.text().to_string();

        let ty = child
            .ty()
            .map(|n| parse_type(node_text(source, n.syntax())))
            .unwrap_or(Type::Unknown);

        out.push(Parameter::new(name, ty));
    }
    out
}

fn node_text<'a>(source: &'a str, node: &nova_syntax::SyntaxNode) -> &'a str {
    let range = node.text_range();
    let start: usize = u32::from(range.start()) as usize;
    let end: usize = u32::from(range.end()) as usize;
    source.get(start..end).unwrap_or("")
}

fn collect_annotations(modifiers: &syntax_ast::Modifiers) -> Vec<Annotation> {
    let mut out = Vec::new();
    for annotation in modifiers.annotations() {
        let Some(name) = annotation.name().map(|name| name.text()) else {
            continue;
        };
        let simple = name.rsplit('.').next().unwrap_or(name.as_str()).trim();
        if simple.is_empty() {
            continue;
        }

        let range = annotation.syntax().text_range();
        let start: usize = u32::from(range.start()) as usize;
        let end: usize = u32::from(range.end()) as usize;
        out.push(Annotation::new_with_span(
            simple.to_string(),
            Span::new(start, end),
        ));
    }
    out
}

fn modifier_flags(modifiers: &syntax_ast::Modifiers) -> (bool, bool) {
    (
        modifier_contains_keyword(modifiers, SyntaxKind::StaticKw),
        modifier_contains_keyword(modifiers, SyntaxKind::FinalKw),
    )
}

fn modifier_contains_keyword(modifiers: &syntax_ast::Modifiers, kind: SyntaxKind) -> bool {
    modifiers.keywords().any(|tok| tok.kind() == kind)
}

fn parse_type(raw: &str) -> Type {
    let mut raw = raw.trim().to_string();
    if raw.is_empty() {
        return Type::Unknown;
    }

    // Drop whitespace (type nodes may include spaces in generics).
    raw.retain(|ch| !ch.is_ascii_whitespace());

    // Count array dimensions.
    let mut dims = 0usize;
    while raw.ends_with("[]") {
        dims += 1;
        raw.truncate(raw.len().saturating_sub(2));
    }

    let base = strip_generic_args(&raw);

    let mut ty = match base.as_str() {
        "void" => Type::Void,
        "boolean" => Type::Primitive(PrimitiveType::Boolean),
        "byte" => Type::Primitive(PrimitiveType::Byte),
        "short" => Type::Primitive(PrimitiveType::Short),
        "char" => Type::Primitive(PrimitiveType::Char),
        "int" => Type::Primitive(PrimitiveType::Int),
        "long" => Type::Primitive(PrimitiveType::Long),
        "float" => Type::Primitive(PrimitiveType::Float),
        "double" => Type::Primitive(PrimitiveType::Double),
        other => Type::Named(other.to_string()),
    };

    for _ in 0..dims {
        ty = Type::Array(Box::new(ty));
    }
    ty
}

fn strip_generic_args(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0u32;
    for ch in raw.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}
