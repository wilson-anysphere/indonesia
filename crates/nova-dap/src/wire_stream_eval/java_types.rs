use nova_classfile::{
    parse_field_signature, BaseType, ClassTypeSignature, TypeArgument, TypeSignature,
};
use std::sync::OnceLock;

/// Formats a JVM field signature (descriptor) and optional generic signature into a Java source
/// type string.
///
/// This is intended for generating Java helper source that can be compiled by `javac`:
/// - class names are fully-qualified (`java.util.List`)
/// - generic signatures are preserved when possible (`java.util.List<java.lang.String>`)
/// - arrays are rendered using Java `[]` syntax
///
/// If the generic signature is present but cannot be formatted (e.g. due to type variables), we
/// fall back to the erased descriptor.
pub fn java_type_from_signatures(signature: &str, generic_signature: Option<&str>) -> String {
    if let Some(generic) = generic_signature {
        let generic = generic.trim();
        if !generic.is_empty() {
            if let Some(ty) = java_type_from_generic_signature(generic) {
                return ty;
            }
        }
    }

    java_type_from_descriptor(signature)
}

fn java_type_from_generic_signature(sig: &str) -> Option<String> {
    static GENERIC_SIGNATURE_PARSE_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    let parsed = match parse_field_signature(sig) {
        Ok(parsed) => parsed,
        Err(err) => {
            if GENERIC_SIGNATURE_PARSE_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.dap",
                    sig_len = sig.len(),
                    error = ?err,
                    "failed to parse generic field signature; falling back to erased descriptor"
                );
            }
            return None;
        }
    };
    Some(fmt_type_signature(&parsed))
}

fn fmt_type_signature(sig: &TypeSignature) -> String {
    match sig {
        TypeSignature::Base(base) => base_type_name(*base).to_string(),
        TypeSignature::Array(inner) => {
            let mut out = fmt_type_signature(inner.as_ref());
            out.push_str("[]");
            out
        }
        TypeSignature::Class(class) => fmt_class_type_signature(class),
        // Generic type variables cannot be referenced from our injected helper without also
        // reproducing the declaring type parameter list. Fall back to `Object` (erasure-ish).
        TypeSignature::TypeVariable(_name) => "java.lang.Object".to_string(),
    }
}

fn fmt_class_type_signature(sig: &ClassTypeSignature) -> String {
    let mut out = String::new();
    if !sig.package.is_empty() {
        out.push_str(&sig.package.join("."));
        out.push('.');
    }

    // Important: when rendering nested types, only include type arguments for the *final*
    // segment. Including type arguments on an outer segment can make the type un-compilable
    // for static nested types (e.g. `Map<String, Integer>.Entry`).
    let last_idx = sig.segments.len().saturating_sub(1);
    for (idx, seg) in sig.segments.iter().enumerate() {
        if idx > 0 {
            out.push('.');
        }
        out.push_str(&seg.name.replace('$', "."));
        if idx == last_idx && !seg.type_arguments.is_empty() {
            out.push('<');
            for (arg_idx, arg) in seg.type_arguments.iter().enumerate() {
                if arg_idx > 0 {
                    out.push_str(", ");
                }
                out.push_str(&fmt_type_argument(arg));
            }
            out.push('>');
        }
    }

    out
}

fn fmt_type_argument(arg: &TypeArgument) -> String {
    match arg {
        TypeArgument::Any => "?".to_string(),
        TypeArgument::Exact(inner) => fmt_type_signature(inner.as_ref()),
        TypeArgument::Extends(inner) => format!("? extends {}", fmt_type_signature(inner.as_ref())),
        TypeArgument::Super(inner) => format!("? super {}", fmt_type_signature(inner.as_ref())),
    }
}

fn java_type_from_descriptor(signature: &str) -> String {
    let mut sig = signature.trim();
    if sig.is_empty() {
        return "java.lang.Object".to_string();
    }

    let mut dims = 0usize;
    while let Some(rest) = sig.strip_prefix('[') {
        dims += 1;
        sig = rest;
    }

    let base = if let Some(class) = sig.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        class.replace('/', ".").replace('$', ".")
    } else {
        match sig.as_bytes().first().copied() {
            Some(b'B') => "byte".to_string(),
            Some(b'C') => "char".to_string(),
            Some(b'D') => "double".to_string(),
            Some(b'F') => "float".to_string(),
            Some(b'I') => "int".to_string(),
            Some(b'J') => "long".to_string(),
            Some(b'S') => "short".to_string(),
            Some(b'Z') => "boolean".to_string(),
            Some(b'V') => "void".to_string(),
            _ => "java.lang.Object".to_string(),
        }
    };

    let mut out = base;
    for _ in 0..dims {
        out.push_str("[]");
    }
    out
}

fn base_type_name(base: BaseType) -> &'static str {
    match base {
        BaseType::Boolean => "boolean",
        BaseType::Byte => "byte",
        BaseType::Short => "short",
        BaseType::Char => "char",
        BaseType::Int => "int",
        BaseType::Long => "long",
        BaseType::Float => "float",
        BaseType::Double => "double",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_erased_descriptors() {
        assert_eq!(java_type_from_signatures("I", None), "int");
        assert_eq!(
            java_type_from_signatures("Ljava/util/List;", None),
            "java.util.List"
        );
        assert_eq!(
            java_type_from_signatures("[Ljava/lang/String;", None),
            "java.lang.String[]"
        );
    }

    #[test]
    fn formats_generic_signatures() {
        assert_eq!(
            java_type_from_signatures(
                "Ljava/util/List;",
                Some("Ljava/util/List<Ljava/lang/String;>;")
            ),
            "java.util.List<java.lang.String>"
        );

        assert_eq!(
            java_type_from_signatures(
                "Ljava/util/List;",
                Some("Ljava/util/List<+Ljava/lang/Number;>;")
            ),
            "java.util.List<? extends java.lang.Number>"
        );

        assert_eq!(
            java_type_from_signatures(
                "Ljava/util/Map$Entry;",
                Some("Ljava/util/Map<Ljava/lang/String;Ljava/lang/Integer;>.Entry<*>;")
            ),
            "java.util.Map.Entry<?>"
        );
    }
}
