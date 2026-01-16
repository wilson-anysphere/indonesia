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
    fn inner(
        env: &dyn TypeEnv,
        ty: &Type,
        target: ClassId,
        seen_type_vars: &mut HashSet<TypeVarId>,
    ) -> Option<Type> {
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
                // Deterministically iterate intersection components.
                let mut sorted: Vec<&Type> = parts.iter().collect();
                sorted.sort_by_cached_key(|ty| {
                    (
                        crate::intersection_component_rank(env, ty),
                        crate::type_sort_key(env, ty),
                    )
                });

                // Best-effort: if multiple parts can be viewed as the requested supertype, prefer
                // the most informative (fewest Unknowns) and ensure the result is not ambiguous.
                //
                // If we see multiple incompatible instantiations, treat the result as ambiguous
                // and return `None` (but deterministically, thanks to sorted iteration).
                let mut out: Option<Type> = None;
                for part in sorted {
                    let Some(found) = inner(env, part, target, seen_type_vars) else {
                        continue;
                    };
                    out = match out {
                        None => Some(found),
                        Some(existing) => {
                            Some(merge_instantiated_supertypes(env, existing, found)?)
                        }
                    };
                }

                return out;
            }
            Type::TypeVar(id) => {
                if !seen_type_vars.insert(*id) {
                    return None;
                }

                let mut out: Option<Type> = None;
                if let Some(tp) = env.type_param(*id) {
                    // Deterministically iterate bounds (ordering isn't guaranteed stable during recovery).
                    let mut sorted: Vec<&Type> = tp.upper_bounds.iter().collect();
                    sorted.sort_by_cached_key(|ty| {
                        (
                            crate::intersection_component_rank(env, ty),
                            crate::type_sort_key(env, ty),
                        )
                    });

                    for bound in sorted {
                        let Some(found) = inner(env, bound, target, seen_type_vars) else {
                            continue;
                        };
                        out = match out {
                            None => Some(found),
                            Some(existing) => {
                                match merge_instantiated_supertypes(env, existing, found) {
                                    Some(merged) => Some(merged),
                                    None => {
                                        // Ensure recursion guard is cleared before returning.
                                        seen_type_vars.remove(id);
                                        return None;
                                    }
                                }
                            }
                        };
                    }
                }

                seen_type_vars.remove(id);
                return out;
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

                let mut ifaces: Vec<Type> = class_def
                    .interfaces
                    .iter()
                    .filter_map(|iface| raw_class_type(env, iface))
                    .collect();
                ifaces.sort_by_cached_key(|ty| crate::type_sort_key(env, ty));
                for iface in ifaces {
                    queue.push_back(iface);
                }

                if class_def.kind == ClassKind::Interface {
                    queue.push_back(Type::class(env.well_known().object, vec![]));
                }
                continue;
            }

            // Apply the current instantiation's substitution to its supertypes.
            let mut subst: HashMap<TypeVarId, Type> =
                HashMap::with_capacity(class_def.type_params.len());
            for (idx, formal) in class_def.type_params.iter().copied().enumerate() {
                subst.insert(formal, args.get(idx).cloned().unwrap_or(Type::Unknown));
            }

            if let Some(sc) = &class_def.super_class {
                let sc = crate::canonicalize_named(env, &crate::substitute(sc, &subst));
                queue.push_back(sc);
            }

            let mut ifaces: Vec<Type> = class_def
                .interfaces
                .iter()
                .map(|iface| {
                    let iface = crate::substitute(iface, &subst);
                    crate::canonicalize_named(env, &iface)
                })
                .collect();
            ifaces.sort_by_cached_key(|ty| crate::type_sort_key(env, ty));
            for iface in ifaces {
                queue.push_back(iface);
            }

            // In Java, every interface implicitly has `Object` as a supertype (JLS 4.10.2).
            if class_def.kind == ClassKind::Interface {
                queue.push_back(Type::class(env.well_known().object, vec![]));
            }
        }

        None
    }

    let mut seen_type_vars = HashSet::new();
    inner(env, ty, target, &mut seen_type_vars)
}

fn merge_instantiated_supertypes(env: &dyn TypeEnv, a: Type, b: Type) -> Option<Type> {
    if a == b {
        return Some(a);
    }

    let a_score = placeholder_score(&a);
    let b_score = placeholder_score(&b);
    if a_score != b_score {
        return Some(if a_score < b_score { a } else { b });
    }

    let a_sub_b = crate::is_subtype(env, &a, &b);
    let b_sub_a = crate::is_subtype(env, &b, &a);

    match (a_sub_b, b_sub_a) {
        (true, false) => Some(a),
        (false, true) => Some(b),
        (true, true) => Some(a),
        (false, false) => None,
    }
}

fn placeholder_score(ty: &Type) -> usize {
    match ty {
        Type::Unknown | Type::Error => 1,
        Type::Array(elem) => placeholder_score(elem),
        Type::Class(ClassType { args, .. }) => args.iter().map(placeholder_score).sum(),
        Type::Wildcard(crate::WildcardBound::Extends(upper))
        | Type::Wildcard(crate::WildcardBound::Super(upper)) => placeholder_score(upper),
        Type::Intersection(parts) => parts.iter().map(placeholder_score).sum(),
        _ => 0,
    }
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
    fn normalize_type(env: &dyn TypeEnv, ty: Type) -> Type {
        let ty = crate::canonicalize_named(env, &ty);
        match ty {
            Type::Intersection(_) => crate::make_intersection(env, vec![ty]),
            other => other,
        }
    }

    fn normalize_sig(env: &dyn TypeEnv, sig: SamSignature) -> SamSignature {
        SamSignature {
            params: sig
                .params
                .into_iter()
                .map(|t| normalize_type(env, t))
                .collect(),
            return_type: normalize_type(env, sig.return_type),
        }
    }

    fn inner(
        env: &dyn TypeEnv,
        ty: &Type,
        seen_type_vars: &mut HashSet<TypeVarId>,
    ) -> Option<SamSignature> {
        match ty {
            Type::TypeVar(id) => {
                if !seen_type_vars.insert(*id) {
                    return None;
                }
                let sig = env.type_param(*id).and_then(|tp| {
                    let mut sig: Option<SamSignature> = None;
                    for bound in &tp.upper_bounds {
                        let Some(bound_sig) = inner(env, bound, seen_type_vars) else {
                            continue;
                        };
                        match &sig {
                            None => sig = Some(bound_sig),
                            Some(existing) if *existing == bound_sig => {}
                            Some(_) => return None,
                        }
                    }
                    sig
                });
                seen_type_vars.remove(id);
                return sig;
            }
            Type::Intersection(parts) => {
                // Best-effort: treat an intersection type as functional if all functional
                // components share the same SAM signature.
                let mut sig: Option<SamSignature> = None;
                for part in parts {
                    let Some(part_sig) = inner(env, part, seen_type_vars) else {
                        continue;
                    };
                    match &sig {
                        None => sig = Some(part_sig),
                        Some(existing) if *existing == part_sig => {}
                        Some(_) => return None,
                    }
                }
                return sig;
            }
            _ => {}
        }

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
            let mut subst: HashMap<TypeVarId, Type> =
                HashMap::with_capacity(class_def.type_params.len());
            for (idx, formal) in class_def.type_params.iter().copied().enumerate() {
                subst.insert(formal, args.get(idx).cloned().unwrap_or(Type::Unknown));
            }

            // Collect abstract instance methods.
            for m in &class_def.methods {
                if m.is_static || !m.is_abstract {
                    continue;
                }

                let params: Vec<Type> = m
                    .params
                    .iter()
                    .map(|p| crate::substitute(p, &subst))
                    .collect();
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
            // Sort interface traversal so we don't depend on source/classfile interface ordering.
            let mut ifaces: Vec<Type> = class_def
                .interfaces
                .iter()
                .map(|iface| crate::canonicalize_named(env, &crate::substitute(iface, &subst)))
                .filter(|iface| matches!(iface, Type::Class(_) | Type::Named(_)))
                .collect();
            ifaces.sort_by_cached_key(|ty| crate::type_sort_key(env, ty));
            for iface in ifaces {
                queue.push_back(iface);
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
        Some(normalize_sig(
            env,
            SamSignature {
                params,
                return_type,
            },
        ))
    }

    let mut seen_type_vars = HashSet::new();
    inner(env, ty, &mut seen_type_vars)
}

fn merge_return_types(env: &dyn TypeEnv, a: Type, b: Type) -> Option<Type> {
    // Canonicalize unresolved `Named` spellings when possible. This avoids order-dependent
    // results when equivalent types are represented differently (e.g. `Named("java.lang.String")`
    // vs `Class(String)`).
    let a = crate::canonicalize_named(env, &a);
    let b = crate::canonicalize_named(env, &b);

    if a == b {
        return Some(a);
    }

    // Prefer non-errorish types when possible.
    if a.is_errorish() && b.is_errorish() {
        return Some(
            if crate::type_sort_key(env, &a) <= crate::type_sort_key(env, &b) {
                a
            } else {
                b
            },
        );
    }
    if a.is_errorish() {
        return Some(b);
    }
    if b.is_errorish() {
        return Some(a);
    }

    let a_sub_b = crate::is_subtype(env, &a, &b);
    let b_sub_a = crate::is_subtype(env, &b, &a);

    match (a_sub_b, b_sub_a) {
        (true, false) => Some(crate::make_intersection(env, vec![a])),
        (false, true) => Some(crate::make_intersection(env, vec![b])),
        (true, true) => {
            // Mutual subtyping can happen for equivalent types (e.g. `Named` vs resolved `Class`,
            // or intersection permutations). Choose a deterministic representative.
            Some(crate::make_intersection(env, vec![a, b]))
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
            let array_list_def = store.class_mut(array_list).expect("ArrayList should exist");
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

    #[test]
    fn sam_signature_is_order_independent_for_equivalent_return_types() {
        let mut store = TypeStore::with_minimal_jdk();
        let object = store.well_known().object;

        let string = Type::class(store.well_known().string, vec![]);

        let i1 = store.add_class(ClassDef {
            name: "com.example.RetNamed".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![MethodDef {
                name: "apply".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: Type::Named("java.lang.String".to_string()),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            }],
        });

        let i2 = store.add_class(ClassDef {
            name: "com.example.RetClass".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![MethodDef {
                name: "apply".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: string.clone(),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            }],
        });

        let root1 = store.add_class(ClassDef {
            name: "com.example.Root1".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![Type::class(i1, vec![]), Type::class(i2, vec![])],
            fields: vec![],
            constructors: vec![],
            methods: vec![],
        });

        let root2 = store.add_class(ClassDef {
            name: "com.example.Root2".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![Type::class(i2, vec![]), Type::class(i1, vec![])],
            fields: vec![],
            constructors: vec![],
            methods: vec![],
        });

        let sig1 = sam_signature(&store, &Type::class(root1, vec![]))
            .expect("interface with a single effective abstract method should be functional");
        let sig2 = sam_signature(&store, &Type::class(root2, vec![]))
            .expect("interface with a single effective abstract method should be functional");

        assert_eq!(sig1, sig2);
        assert_eq!(sig1.params, Vec::<Type>::new());
        assert_eq!(sig1.return_type, string);
    }

    #[test]
    fn sam_signature_accepts_intersection_with_single_functional_interface() {
        let store = TypeStore::with_minimal_jdk();
        let runnable = store
            .class_id("java.lang.Runnable")
            .expect("minimal JDK should define java.lang.Runnable");
        let cloneable = store.well_known().cloneable;

        let ty = Type::Intersection(vec![
            Type::class(runnable, vec![]),
            Type::class(cloneable, vec![]),
        ]);

        let sig = sam_signature(&store, &ty).expect("Runnable & Cloneable should be functional");
        assert_eq!(sig.params, Vec::<Type>::new());
        assert_eq!(sig.return_type, Type::Void);
    }

    #[test]
    fn sam_signature_accepts_type_var_with_functional_bound() {
        let mut store = TypeStore::with_minimal_jdk();
        let function = store
            .class_id("java.util.function.Function")
            .expect("minimal JDK should define java.util.function.Function");

        let string = Type::class(store.well_known().string, vec![]);
        let integer = Type::class(store.well_known().integer, vec![]);
        let bound = Type::class(function, vec![string.clone(), integer.clone()]);
        let tv = store.add_type_param("F", vec![bound]);

        let sig = sam_signature(&store, &Type::TypeVar(tv))
            .expect("type variable with functional interface bound should be functional");
        assert_eq!(sig.params, vec![string]);
        assert_eq!(sig.return_type, integer);
    }

    #[test]
    fn sam_signature_accepts_type_var_with_equivalent_functional_bounds() {
        let mut store = TypeStore::with_minimal_jdk();
        let object = store.well_known().object;

        let string = Type::class(store.well_known().string, vec![]);

        let i_named = store.add_class(ClassDef {
            name: "com.example.FuncNamed".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![MethodDef {
                name: "apply".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: Type::Named("java.lang.String".to_string()),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            }],
        });

        let i_class = store.add_class(ClassDef {
            name: "com.example.FuncClass".to_string(),
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![MethodDef {
                name: "apply".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: string.clone(),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            }],
        });

        let tv = store.add_type_param(
            "F",
            vec![Type::class(i_named, vec![]), Type::class(i_class, vec![])],
        );

        let sig = sam_signature(&store, &Type::TypeVar(tv))
            .expect("equivalent functional bounds should still be treated as functional");
        assert_eq!(sig.params, Vec::<Type>::new());
        assert_eq!(sig.return_type, string);
    }

    #[test]
    fn sam_signature_rejects_type_var_with_conflicting_functional_bounds() {
        let mut store = TypeStore::with_minimal_jdk();
        let runnable = store
            .class_id("java.lang.Runnable")
            .expect("minimal JDK should define java.lang.Runnable");
        let function = store
            .class_id("java.util.function.Function")
            .expect("minimal JDK should define java.util.function.Function");

        let string = Type::class(store.well_known().string, vec![]);
        let integer = Type::class(store.well_known().integer, vec![]);
        let tv = store.add_type_param(
            "F",
            vec![
                Type::class(runnable, vec![]),
                Type::class(function, vec![string, integer]),
            ],
        );

        assert!(
            sam_signature(&store, &Type::TypeVar(tv)).is_none(),
            "conflicting functional bounds should not be treated as a functional interface"
        );
    }

    #[test]
    fn instantiate_as_supertype_preserves_raw_types() {
        let store = TypeStore::with_minimal_jdk();
        let list = store
            .class_id("java.util.List")
            .expect("minimal JDK should define java.util.List");

        let array_list_named = Type::Named("java.util.ArrayList".to_string());
        let instantiated = instantiate_as_supertype(&store, &array_list_named, list)
            .expect("should map supertypes");
        assert_eq!(instantiated, Type::class(list, vec![]));
    }

    #[test]
    fn instantiate_as_supertype_rejects_conflicting_intersection_instantiations() {
        let store = TypeStore::with_minimal_jdk();
        let list = store
            .class_id("java.util.List")
            .expect("minimal JDK should define java.util.List");
        let string = Type::class(store.well_known().string, vec![]);
        let integer = Type::class(store.well_known().integer, vec![]);

        let ty = Type::Intersection(vec![
            Type::class(list, vec![string]),
            Type::class(list, vec![integer]),
        ]);

        assert!(
            instantiate_as_supertype(&store, &ty, list).is_none(),
            "ambiguous intersection instantiations should not pick an arbitrary type argument"
        );
    }

    #[test]
    fn instantiate_as_supertype_rejects_conflicting_type_var_bounds() {
        let mut store = TypeStore::with_minimal_jdk();
        let list = store
            .class_id("java.util.List")
            .expect("minimal JDK should define java.util.List");
        let string = Type::class(store.well_known().string, vec![]);
        let integer = Type::class(store.well_known().integer, vec![]);

        let tv = store.add_type_param(
            "T",
            vec![
                Type::class(list, vec![string]),
                Type::class(list, vec![integer]),
            ],
        );

        assert!(
            instantiate_as_supertype(&store, &Type::TypeVar(tv), list).is_none(),
            "conflicting bounds should not produce an arbitrary instantiation"
        );
    }
}
