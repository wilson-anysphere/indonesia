use std::collections::{HashMap, HashSet, VecDeque};

use crate::{ClassId, ClassKind, ClassType, PrimitiveType, Type, TypeEnv, TypeVarId};

/// Return `ty` viewed as `target` by walking the supertype graph and applying type argument
/// substitution along the way.
///
/// This is a best-effort helper used for IDE-style type recovery. It never panics: missing class
/// metadata simply returns `None`.
///
/// Example: `ArrayList<String>` instantiated as `List` returns `List<String>`.
pub fn instantiate_as_supertype(env: &dyn TypeEnv, ty: &Type, target: ClassId) -> Option<Type> {
    // Handle a few non-class cases up front.
    match ty {
        Type::Array(_) => {
            let wk = env.well_known();
            if target == wk.object || target == wk.cloneable || target == wk.serializable {
                return Some(Type::class(target, vec![]));
            }
            return None;
        }
        Type::Intersection(parts) => {
            // Best-effort: if any component can be viewed as the requested supertype, use it.
            for part in parts {
                if let Some(found) = instantiate_as_supertype(env, part, target) {
                    return Some(found);
                }
            }
            return None;
        }
        Type::TypeVar(id) => {
            // Best-effort: consult the first upper bound that can be instantiated.
            let tp = env.type_param(*id)?;
            for bound in &tp.upper_bounds {
                if let Some(found) = instantiate_as_supertype(env, bound, target) {
                    return Some(found);
                }
            }
            return None;
        }
        _ => {}
    }

    let ty = crate::canonicalize_named(env, ty);

    let Type::Class(ClassType { def, args }) = ty else {
        return None;
    };

    let mut queue: VecDeque<Type> = VecDeque::new();
    let mut seen: HashSet<(ClassId, Vec<Type>)> = HashSet::new();
    queue.push_back(Type::class(def, args));

    while let Some(current) = queue.pop_front() {
        let Type::Class(ClassType { def, args }) = current.clone() else {
            continue;
        };
        if !seen.insert((def, args.clone())) {
            continue;
        }

        if def == target {
            return Some(current);
        }

        let Some(class_def) = env.class(def) else {
            continue;
        };

        // If the current instantiation is raw (e.g. `List` rather than `List<String>`), we can't
        // recover meaningful type arguments for supertypes. Preserve rawness when walking.
        let raw = args.is_empty() && !class_def.type_params.is_empty();

        if raw {
            if let Some(sc) = &class_def.super_class {
                if let Some(raw_sc) = raw_class_type(env, sc) {
                    queue.push_back(raw_sc);
                }
            }
            for iface in &class_def.interfaces {
                if let Some(raw_iface) = raw_class_type(env, iface) {
                    queue.push_back(raw_iface);
                }
            }
            if class_def.kind == ClassKind::Interface {
                queue.push_back(Type::class(env.well_known().object, vec![]));
            }
            continue;
        }

        // Apply the current instantiation's substitution to its supertypes.
        let subst = class_def
            .type_params
            .iter()
            .copied()
            .zip(args.into_iter())
            .collect::<HashMap<_, _>>();

        if let Some(sc) = &class_def.super_class {
            let sc = crate::substitute(sc, &subst);
            queue.push_back(crate::canonicalize_named(env, &sc));
        }
        for iface in &class_def.interfaces {
            let iface = crate::substitute(iface, &subst);
            queue.push_back(crate::canonicalize_named(env, &iface));
        }
        // In Java, every interface implicitly has `Object` as a supertype (JLS 4.10.2).
        if class_def.kind == ClassKind::Interface {
            queue.push_back(Type::class(env.well_known().object, vec![]));
        }
    }

    None
}

fn raw_class_type(env: &dyn TypeEnv, ty: &Type) -> Option<Type> {
    let ty = crate::canonicalize_named(env, ty);
    match ty {
        Type::Class(ClassType { def, .. }) => Some(Type::class(def, vec![])),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamSignature {
    pub params: Vec<Type>,
    pub return_type: Type,
}

/// Best-effort extraction of a functional interface's single-abstract-method (SAM) signature.
///
/// Applies type argument substitution so `Function<String, Integer>` yields `(String) -> Integer`.
/// Returns `None` if `ty` is not (obviously) a functional interface.
pub fn sam_signature(env: &dyn TypeEnv, ty: &Type) -> Option<SamSignature> {
    let ty = crate::canonicalize_named(env, ty);
    let Type::Class(ClassType { def, args }) = ty else {
        return None;
    };

    let root_def = env.class(def)?;
    if root_def.kind != ClassKind::Interface {
        return None;
    }

    // Walk the interface inheritance graph, collecting abstract instance methods and applying
    // type argument substitution along the way.
    let mut queue: VecDeque<Type> = VecDeque::new();
    let mut seen: HashSet<(ClassId, Vec<Type>)> = HashSet::new();
    queue.push_back(Type::class(def, args));

    // Map (name, parameter types) to the most specific return type we've seen so far.
    let mut candidates: HashMap<(String, Vec<Type>), Type> = HashMap::new();

    while let Some(current) = queue.pop_front() {
        let Type::Class(ClassType { def, args }) = current.clone() else {
            continue;
        };
        if !seen.insert((def, args.clone())) {
            continue;
        }

        let Some(class_def) = env.class(def) else {
            continue;
        };

        // Build substitution mapping for this interface instantiation.
        //
        // If `args` is missing entries (raw or malformed), fall back to `Unknown` so downstream
        // callers still get a stable shape.
        let mut subst: HashMap<TypeVarId, Type> = HashMap::with_capacity(class_def.type_params.len());
        for (idx, formal) in class_def.type_params.iter().copied().enumerate() {
            subst.insert(formal, args.get(idx).cloned().unwrap_or(Type::Unknown));
        }

        // Collect abstract instance methods.
        for m in &class_def.methods {
            if m.is_static || !m.is_abstract {
                continue;
            }

            let params: Vec<Type> = m.params.iter().map(|p| crate::substitute(p, &subst)).collect();
            let return_type = crate::substitute(&m.return_type, &subst);

            if is_object_method(env, &m.name, &params, &return_type) {
                continue;
            }

            let key = (m.name.clone(), params.clone());
            if let Some(existing) = candidates.get(&key).cloned() {
                let merged = merge_return_types(env, existing, return_type)?;
                candidates.insert(key, merged);
            } else {
                candidates.insert(key, return_type);
            }
        }

        // Visit supertypes with substitution applied.
        if let Some(sc) = &class_def.super_class {
            let sc = crate::canonicalize_named(env, &crate::substitute(sc, &subst));
            if matches!(sc, Type::Class(_) | Type::Named(_)) {
                queue.push_back(sc);
            }
        }
        for iface in &class_def.interfaces {
            let iface = crate::canonicalize_named(env, &crate::substitute(iface, &subst));
            if matches!(iface, Type::Class(_) | Type::Named(_)) {
                queue.push_back(iface);
            }
        }

        // In Java, every interface implicitly has `Object` as a supertype (JLS 4.10.2).
        if class_def.kind == ClassKind::Interface {
            queue.push_back(Type::class(env.well_known().object, vec![]));
        }
    }

    if candidates.len() != 1 {
        return None;
    }
    let ((_name, params), return_type) = candidates.into_iter().next()?;
    Some(SamSignature { params, return_type })
}

fn merge_return_types(env: &dyn TypeEnv, a: Type, b: Type) -> Option<Type> {
    if a == b {
        return Some(a);
    }

    // Prefer non-errorish types when possible.
    if a.is_errorish() {
        return Some(b);
    }
    if b.is_errorish() {
        return Some(a);
    }

    let a_sub_b = crate::is_subtype(env, &a, &b);
    let b_sub_a = crate::is_subtype(env, &b, &a);

    match (a_sub_b, b_sub_a) {
        (true, false) => Some(a),
        (false, true) => Some(b),
        (true, true) => {
            // Mutual subtyping can happen for equal types, but also for `Unknown`/`Error` which are
            // treated as compatible with everything. Prefer the more informative option.
            match (&a, &b) {
                (Type::Unknown, _) => Some(b),
                (_, Type::Unknown) => Some(a),
                _ => Some(a),
            }
        }
        (false, false) => None,
    }
}

fn is_object_method(env: &dyn TypeEnv, name: &str, params: &[Type], return_type: &Type) -> bool {
    let return_type = crate::canonicalize_named(env, return_type);
    match name {
        "equals" => {
            if params.len() != 1 {
                return false;
            }
            let object = Type::class(env.well_known().object, vec![]);
            crate::canonicalize_named(env, &params[0]) == object
                && return_type == Type::Primitive(PrimitiveType::Boolean)
        }
        "hashCode" => params.is_empty() && return_type == Type::Primitive(PrimitiveType::Int),
        "toString" => {
            if !params.is_empty() {
                return false;
            }
            let string = Type::class(env.well_known().string, vec![]);
            return_type == string
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClassDef, MethodDef, TypeEnv, TypeStore};

    #[test]
    fn instantiate_as_supertype_recovers_type_arguments() {
        let mut store = TypeStore::with_minimal_jdk();

        let object = store.well_known().object;

        let list = store
            .class_id("java.util.List")
            .expect("minimal JDK should define java.util.List");
        let array_list = store
            .class_id("java.util.ArrayList")
            .expect("minimal JDK should define java.util.ArrayList");

        // Make the relationship transitive for the test:
        // ArrayList<E> extends AbstractList<E>; AbstractList<E> implements List<E>.
        let abstract_list_e = store.add_type_param("E", vec![Type::class(object, vec![])]);
        let abstract_list = store.add_class(ClassDef {
            name: "java.util.AbstractList".to_string(),
            kind: ClassKind::Class,
            type_params: vec![abstract_list_e],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![Type::class(list, vec![Type::TypeVar(abstract_list_e)])],
            fields: vec![],
            constructors: vec![],
            methods: vec![],
        });

        {
            let array_list_def = store
                .class_mut(array_list)
                .expect("ArrayList should exist");
            assert_eq!(array_list_def.type_params.len(), 1);
            let array_list_e = array_list_def.type_params[0];
            array_list_def.super_class = Some(Type::class(
                abstract_list,
                vec![Type::TypeVar(array_list_e)],
            ));
            array_list_def.interfaces.clear();
        }

        let string = Type::class(store.well_known().string, vec![]);
        let array_list_string = Type::class(array_list, vec![string.clone()]);

        let instantiated = instantiate_as_supertype(&store, &array_list_string, list)
            .expect("should be able to view ArrayList<String> as List");

        assert_eq!(instantiated, Type::class(list, vec![string]));
    }

    #[test]
    fn sam_signature_applies_type_arguments() {
        let store = TypeStore::with_minimal_jdk();

        let function = store
            .class_id("java.util.function.Function")
            .expect("minimal JDK should define java.util.function.Function");

        let string = Type::class(store.well_known().string, vec![]);
        let integer = Type::class(store.well_known().integer, vec![]);
        let function_ty = Type::class(function, vec![string.clone(), integer.clone()]);

        let sig = sam_signature(&store, &function_ty).expect("Function should be functional");
        assert_eq!(sig.params, vec![string]);
        assert_eq!(sig.return_type, integer);
    }

    #[test]
    fn sam_signature_ignores_default_and_static_methods() {
        let mut store = TypeStore::with_minimal_jdk();
        let object = store.well_known().object;

        let iface_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
        let iface = store.add_class(ClassDef {
            name: "com.example.MyFun".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![iface_t],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![
                // Default method (non-abstract) should be ignored.
                MethodDef {
                    name: "defaultMethod".to_string(),
                    type_params: vec![],
                    params: vec![],
                    return_type: Type::Void,
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                // Static method should be ignored.
                MethodDef {
                    name: "staticMethod".to_string(),
                    type_params: vec![],
                    params: vec![],
                    return_type: Type::Void,
                    is_static: true,
                    is_varargs: false,
                    is_abstract: false,
                },
                // Only abstract instance method counts towards SAM.
                MethodDef {
                    name: "apply".to_string(),
                    type_params: vec![],
                    params: vec![Type::TypeVar(iface_t)],
                    return_type: Type::TypeVar(iface_t),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: true,
                },
            ],
        });

        let string = Type::class(store.well_known().string, vec![]);
        let iface_string = Type::class(iface, vec![string.clone()]);
        let sig = sam_signature(&store, &iface_string).expect("should still be functional");
        assert_eq!(sig.params, vec![string.clone()]);
        assert_eq!(sig.return_type, string);
    }
}
