//! Java-like, deterministic formatting for Nova types and method signatures.
//!
//! This module is designed for user-visible features (diagnostics, hover, and
//! signature help). It intentionally keeps formatting rules simple and stable.

use std::fmt::{self, Write as _};

use crate::{
    ClassId, ClassType, MethodDef, ResolvedMethod, Type, TypeEnv, TypeVarId, WildcardBound,
};

/// Convenience helper to format a [`Type`] into a newly allocated [`String`].
pub fn format_type(env: &dyn TypeEnv, ty: &Type) -> String {
    TypeDisplay { env, ty }.to_string()
}

/// Display wrapper for formatting a [`Type`] with access to a [`TypeEnv`].
pub struct TypeDisplay<'a> {
    pub env: &'a dyn TypeEnv,
    pub ty: &'a Type,
}

impl<'a> TypeDisplay<'a> {
    pub fn new(env: &'a dyn TypeEnv, ty: &'a Type) -> Self {
        Self { env, ty }
    }
}

impl fmt::Display for TypeDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_type(self.env, self.ty, f)
    }
}

/// Convenience helper to format a method or constructor signature (declaration).
pub fn format_method_signature(env: &dyn TypeEnv, owner: ClassId, method: &MethodDef) -> String {
    MethodSignatureDisplay { env, owner, method }.to_string()
}

/// Display wrapper for formatting a [`MethodDef`] signature.
pub struct MethodSignatureDisplay<'a> {
    pub env: &'a dyn TypeEnv,
    pub owner: ClassId,
    pub method: &'a MethodDef,
}

impl fmt::Display for MethodSignatureDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_method_signature(self.env, self.owner, self.method, f)
    }
}

/// Convenience helper to format a resolved method signature (after overload
/// resolution / inference).
pub fn format_resolved_method(env: &dyn TypeEnv, method: &ResolvedMethod) -> String {
    ResolvedMethodDisplay { env, method }.to_string()
}

/// Display wrapper for formatting a [`ResolvedMethod`] signature.
pub struct ResolvedMethodDisplay<'a> {
    pub env: &'a dyn TypeEnv,
    pub method: &'a ResolvedMethod,
}

impl fmt::Display for ResolvedMethodDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_resolved_method(self.env, self.method, f)
    }
}

fn fmt_type(env: &dyn TypeEnv, ty: &Type, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match ty {
        Type::Void => f.write_str("void"),
        Type::Primitive(p) => f.write_str(match p {
            crate::PrimitiveType::Boolean => "boolean",
            crate::PrimitiveType::Byte => "byte",
            crate::PrimitiveType::Short => "short",
            crate::PrimitiveType::Char => "char",
            crate::PrimitiveType::Int => "int",
            crate::PrimitiveType::Long => "long",
            crate::PrimitiveType::Float => "float",
            crate::PrimitiveType::Double => "double",
        }),
        Type::Class(ClassType { def, args }) => {
            fmt_class_id(env, *def, f)?;
            fmt_type_args(env, args, f)
        }
        Type::Array(_) => {
            let (base, dims) = peel_array_dims(ty);
            fmt_type(env, base, f)?;
            for _ in 0..dims {
                f.write_str("[]")?;
            }
            Ok(())
        }
        Type::TypeVar(tv) => fmt_type_var(env, *tv, f),
        Type::Wildcard(bound) => match bound {
            WildcardBound::Unbounded => f.write_str("?"),
            WildcardBound::Extends(upper) => {
                f.write_str("? extends ")?;
                fmt_type(env, upper, f)
            }
            WildcardBound::Super(lower) => {
                f.write_str("? super ")?;
                fmt_type(env, lower, f)
            }
        },
        Type::Intersection(types) => {
            let mut it = types.iter();
            let Some(first) = it.next() else {
                return f.write_str("<?>");
            };
            fmt_type(env, first, f)?;
            for ty in it {
                f.write_str(" & ")?;
                fmt_type(env, ty, f)?;
            }
            Ok(())
        }
        Type::Null => f.write_str("null"),
        Type::Named(name) => f.write_str(name),
        Type::VirtualInner { owner, name } => {
            fmt_class_id(env, *owner, f)?;
            f.write_char('.')?;
            f.write_str(name)
        }
        Type::Unknown => f.write_str("<?>"),
        Type::Error => f.write_str("<error>"),
    }
}

fn peel_array_dims(mut ty: &Type) -> (&Type, usize) {
    let mut dims = 0;
    while let Type::Array(inner) = ty {
        dims += 1;
        ty = inner;
    }
    (ty, dims)
}

fn fmt_type_args(env: &dyn TypeEnv, args: &[Type], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if args.is_empty() {
        return Ok(());
    }
    f.write_char('<')?;
    for (idx, arg) in args.iter().enumerate() {
        if idx != 0 {
            f.write_str(", ")?;
        }
        fmt_type(env, arg, f)?;
    }
    f.write_char('>')
}

fn fmt_class_id(env: &dyn TypeEnv, id: ClassId, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let Some(class_def) = env.class(id) else {
        return write!(f, "<class#{}>", id.to_raw());
    };
    fmt_class_name(&class_def.name, f)
}

fn fmt_class_name(binary_name: &str, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // The type model stores binary names (`java.util.Map$Entry`). For user display we render a
    // Java source-style name (`Map.Entry`) and drop the package prefix for readability.
    let class_part = binary_name
        .rsplit_once('.')
        .map(|(_, tail)| tail)
        .unwrap_or(binary_name);
    for ch in class_part.chars() {
        if ch == '$' {
            f.write_char('.')?;
        } else {
            f.write_char(ch)?;
        }
    }
    Ok(())
}

fn fmt_type_var(env: &dyn TypeEnv, id: TypeVarId, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if let Some(tp) = env.type_param(id) {
        f.write_str(&tp.name)
    } else {
        write!(f, "<tv#{}>", id.0)
    }
}

fn fmt_method_signature(
    env: &dyn TypeEnv,
    owner: ClassId,
    method: &MethodDef,
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    fmt_type_param_list(env, &method.type_params, f)?;

    if is_constructor_name(&method.name) {
        fmt_class_id(env, owner, f)?;
    } else {
        fmt_type(env, &method.return_type, f)?;
        f.write_char(' ')?;
        f.write_str(&method.name)?;
    }

    fmt_param_list(env, &method.params, method.is_varargs, f)
}

fn fmt_resolved_method(
    env: &dyn TypeEnv,
    method: &ResolvedMethod,
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    if is_constructor_name(&method.name) {
        fmt_class_id(env, method.owner, f)?;
    } else {
        fmt_type(env, &method.return_type, f)?;
        f.write_char(' ')?;
        f.write_str(&method.name)?;
    }
    fmt_param_list(env, &method.params, method.is_varargs, f)
}

fn fmt_type_param_list(
    env: &dyn TypeEnv,
    params: &[TypeVarId],
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    if params.is_empty() {
        return Ok(());
    }
    f.write_char('<')?;
    for (idx, id) in params.iter().enumerate() {
        if idx != 0 {
            f.write_str(", ")?;
        }
        fmt_type_param_decl(env, *id, f)?;
    }
    f.write_str("> ")
}

fn fmt_type_param_decl(
    env: &dyn TypeEnv,
    id: TypeVarId,
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    let Some(tp) = env.type_param(id) else {
        return write!(f, "<tv#{}>", id.0);
    };
    f.write_str(&tp.name)?;

    let bounds = tp.upper_bounds.as_slice();
    if bounds.is_empty() || (bounds.len() == 1 && is_object_bound(env, &bounds[0])) {
        return Ok(());
    }

    f.write_str(" extends ")?;
    for (idx, bound) in bounds.iter().enumerate() {
        if idx != 0 {
            f.write_str(" & ")?;
        }
        fmt_type(env, bound, f)?;
    }
    Ok(())
}

fn is_object_bound(env: &dyn TypeEnv, ty: &Type) -> bool {
    let Type::Class(ClassType { def, args }) = ty else {
        return false;
    };
    *def == env.well_known().object && args.is_empty()
}

fn fmt_param_list(
    env: &dyn TypeEnv,
    params: &[Type],
    is_varargs: bool,
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    f.write_char('(')?;
    for (idx, param) in params.iter().enumerate() {
        if idx != 0 {
            f.write_str(", ")?;
        }

        if is_varargs && idx == params.len().saturating_sub(1) {
            match param {
                Type::Array(elem) => fmt_type(env, elem, f)?,
                other => fmt_type(env, other, f)?,
            }
            f.write_str("...")?;
        } else {
            fmt_type(env, param, f)?;
        }
    }
    f.write_char(')')
}

fn is_constructor_name(name: &str) -> bool {
    name == "<init>"
}
