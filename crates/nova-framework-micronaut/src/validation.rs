use crate::parse::{
    clean_type, collect_annotations, infer_field_type_node, infer_param_type_node,
    modifier_node, node_text, parse_java, simple_name, visit_nodes,
};
use crate::FileDiagnostic;
use crate::JavaSource;
use nova_syntax::{SyntaxKind, SyntaxNode};
use nova_types::{Diagnostic, Severity};

pub const MICRONAUT_VALIDATION_PRIMITIVE_NONNULL: &str = "MICRONAUT_VALIDATION_PRIMITIVE_NONNULL";
pub const MICRONAUT_VALIDATION_CONSTRAINT_MISMATCH: &str =
    "MICRONAUT_VALIDATION_CONSTRAINT_MISMATCH";

/// Produce best-effort diagnostics for common Bean Validation mistakes.
///
/// This is intentionally conservative and only covers a handful of high-signal
/// cases (e.g. `@NotNull` on primitives).
pub fn validation_diagnostics(sources: &[JavaSource]) -> Vec<Diagnostic> {
    validation_file_diagnostics(sources)
        .into_iter()
        .map(|d| d.diagnostic)
        .collect()
}

pub(crate) fn validation_file_diagnostics(sources: &[JavaSource]) -> Vec<FileDiagnostic> {
    let mut diags = Vec::new();

    for src in sources {
        let Ok(parsed) = parse_java(&src.text) else {
            continue;
        };
        let root = parsed.syntax();

        visit_nodes(root, &mut |node| match node.kind() {
            SyntaxKind::FieldDeclaration => validate_field_declaration(node, src, &mut diags),
            SyntaxKind::Parameter => validate_formal_parameter(node, src, &mut diags),
            _ => {}
        });
    }

    diags
}

fn validate_field_declaration(node: SyntaxNode, src: &JavaSource, out: &mut Vec<FileDiagnostic>) {
    let Some(modifiers) = modifier_node(&node) else {
        return;
    };

    let annotations = collect_annotations(modifiers, &src.text);
    if annotations.is_empty() {
        return;
    }

    let Some(ty_node) = infer_field_type_node(&node) else {
        return;
    };
    let ty = simple_name(&clean_type(node_text(&src.text, &ty_node)));

    let mut local = Vec::new();
    validate_constraints(&ty, &annotations, &mut local);
    out.extend(
        local
            .into_iter()
            .map(|diagnostic| FileDiagnostic::new(src.path.clone(), diagnostic)),
    );
}

fn validate_formal_parameter(node: SyntaxNode, src: &JavaSource, out: &mut Vec<FileDiagnostic>) {
    let Some(modifiers) = modifier_node(&node) else {
        return;
    };

    let annotations = collect_annotations(modifiers, &src.text);
    if annotations.is_empty() {
        return;
    }

    let Some(type_node) = infer_param_type_node(&node) else {
        return;
    };
    let ty = simple_name(&clean_type(node_text(&src.text, &type_node)));

    let mut local = Vec::new();
    validate_constraints(&ty, &annotations, &mut local);
    out.extend(
        local
            .into_iter()
            .map(|diagnostic| FileDiagnostic::new(src.path.clone(), diagnostic)),
    );
}

fn validate_constraints(
    ty: &str,
    annotations: &[crate::parse::ParsedAnnotation],
    out: &mut Vec<Diagnostic>,
) {
    let is_primitive = is_primitive_type(ty);
    let is_string = is_string_type(ty);
    let is_numeric = is_numeric_type(ty);

    for ann in annotations {
        match ann.simple_name.as_str() {
            "NotNull" if is_primitive => out.push(Diagnostic {
                severity: Severity::Warning,
                code: MICRONAUT_VALIDATION_PRIMITIVE_NONNULL.into(),
                message: format!(
                    "Bean Validation annotation @NotNull has no effect on primitive type `{ty}`"
                ),
                span: Some(ann.span),
            }),
            "NotBlank" | "Email" if !is_string => out.push(Diagnostic {
                severity: Severity::Warning,
                code: MICRONAUT_VALIDATION_CONSTRAINT_MISMATCH.into(),
                message: format!(
                    "Bean Validation annotation @{} is typically only valid on String/CharSequence types (found `{ty}`)",
                    ann.simple_name
                ),
                span: Some(ann.span),
            }),
            "Min" | "Max"
            | "Positive"
            | "PositiveOrZero"
            | "Negative"
            | "NegativeOrZero"
            | "DecimalMin"
            | "DecimalMax"
                if !is_numeric =>
            {
                out.push(Diagnostic {
                    severity: Severity::Warning,
                    code: MICRONAUT_VALIDATION_CONSTRAINT_MISMATCH.into(),
                    message: format!(
                        "Bean Validation annotation @{} is typically only valid on numeric types (found `{ty}`)",
                        ann.simple_name
                    ),
                    span: Some(ann.span),
                })
            }
            _ => {}
        }
    }
}

fn is_primitive_type(ty: &str) -> bool {
    matches!(
        ty,
        "boolean" | "byte" | "short" | "int" | "long" | "float" | "double" | "char"
    )
}

fn is_string_type(ty: &str) -> bool {
    matches!(ty, "String" | "CharSequence")
}

fn is_numeric_type(ty: &str) -> bool {
    matches!(
        ty,
        "byte"
            | "short"
            | "int"
            | "long"
            | "float"
            | "double"
            | "Byte"
            | "Short"
            | "Integer"
            | "Long"
            | "Float"
            | "Double"
            | "BigInteger"
            | "BigDecimal"
            | "Number"
    )
}
