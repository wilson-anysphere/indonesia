use nova_classfile::{parse_field_signature, BaseType, ClassTypeSignature, TypeArgument, TypeSignature};

/// Convert a JVM type signature (descriptor or generic signature) into a Java *source* type.
///
/// This is primarily used when generating Java helper methods for expression evaluation where
/// generic information is required for correct lambda type inference (e.g. `List<Integer>`).
pub(crate) fn signature_to_java_source_type(signature: &str) -> String {
    let sig = signature.trim();
    if sig.is_empty() {
        return "java.lang.Object".to_string();
    }

    // Fast-path for primitive descriptors (these are not accepted by `parse_field_signature`).
    if sig.len() == 1 {
        if let Some(prim) = primitive_descriptor_to_java(sig.as_bytes()[0]) {
            return prim.to_string();
        }
    }

    match parse_field_signature(sig) {
        Ok(ty) => format_type_signature(&ty),
        Err(_) => {
            // Best-effort fallback to a compilable type.
            "java.lang.Object".to_string()
        }
    }
}

fn primitive_descriptor_to_java(b: u8) -> Option<&'static str> {
    Some(match b {
        b'B' => "byte",
        b'C' => "char",
        b'D' => "double",
        b'F' => "float",
        b'I' => "int",
        b'J' => "long",
        b'S' => "short",
        b'Z' => "boolean",
        b'V' => "void",
        _ => return None,
    })
}

fn format_type_signature(sig: &TypeSignature) -> String {
    match sig {
        TypeSignature::Base(base) => base_type_to_java(*base).to_string(),
        TypeSignature::Array(component) => format!("{}[]", format_type_signature(component)),
        TypeSignature::Class(class) => format_class_type_signature(class),
        // The injected helper class is not generic, so type variables cannot be referenced.
        TypeSignature::TypeVariable(_) => "java.lang.Object".to_string(),
    }
}

fn base_type_to_java(base: BaseType) -> &'static str {
    match base {
        BaseType::Byte => "byte",
        BaseType::Char => "char",
        BaseType::Double => "double",
        BaseType::Float => "float",
        BaseType::Int => "int",
        BaseType::Long => "long",
        BaseType::Short => "short",
        BaseType::Boolean => "boolean",
    }
}

fn format_class_type_signature(sig: &ClassTypeSignature) -> String {
    let mut out = String::new();
    if !sig.package.is_empty() {
        out.push_str(&sig.package.join("."));
        out.push('.');
    }

    // Important: when rendering nested types, only include type arguments for the *final*
    // segment. Including type arguments on an outer segment can make the type un-compilable
    // for static nested types (e.g. `Map<String, Integer>.Entry`).
    let last_idx = sig.segments.len().saturating_sub(1);
    for (idx, segment) in sig.segments.iter().enumerate() {
        if idx > 0 {
            out.push('.');
        }

        // Generic signatures use `.` between inner classes, but plain descriptors use `$`.
        // Always render Java source using `.`.
        out.push_str(&segment.name.replace('$', "."));

        if idx == last_idx {
            out.push_str(&format_type_arguments(&segment.type_arguments));
        }
    }

    out
}

fn format_type_arguments(args: &[TypeArgument]) -> String {
    if args.is_empty() {
        return String::new();
    }

    let rendered: Vec<String> = args.iter().map(format_type_argument).collect();
    format!("<{}>", rendered.join(","))
}

fn format_type_argument(arg: &TypeArgument) -> String {
    match arg {
        TypeArgument::Any => "?".to_string(),
        TypeArgument::Exact(ty) => format_type_signature(ty),
        TypeArgument::Extends(ty) => format!("? extends {}", format_type_signature(ty)),
        TypeArgument::Super(ty) => format!("? super {}", format_type_signature(ty)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_simple_generic_list() {
        assert_eq!(
            signature_to_java_source_type("Ljava/util/List<Ljava/lang/Integer;>;"),
            "java.util.List<java.lang.Integer>"
        );
    }

    #[test]
    fn formats_inner_class_with_wildcard() {
        assert_eq!(
            signature_to_java_source_type(
                "Ljava/util/Map<Ljava/lang/String;Ljava/lang/Integer;>.Entry<*>;"
            ),
            "java.util.Map.Entry<?>"
        );
    }

    #[test]
    fn substitutes_type_variables_with_object() {
        assert_eq!(signature_to_java_source_type("TT;"), "java.lang.Object");
        assert_eq!(
            signature_to_java_source_type("Ljava/util/List<TT;>;"),
            "java.util.List<java.lang.Object>"
        );
    }
}

