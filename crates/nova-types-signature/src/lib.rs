//! Best-effort translation from JVM classfile signatures (and erased descriptors) into
//! [`nova_types::Type`].
//!
//! This crate bridges the parsed signature ASTs from [`nova_classfile::TypeSignature`] (and friends)
//! with Nova's semantic
//! type model. The translation is intentionally best-effort (suitable for IDE scenarios):
//! malformed or incomplete inputs are handled without panicking and typically degrade to
//! [`nova_types::Type::Unknown`] or erased descriptor types.
//!
//! ## Nested / inner class type arguments
//!
//! JVM generic signatures encode type arguments per "segment" (`Outer<T>.Inner<U>`). Nova's
//! current [`nova_types::Type::Class`] representation does not track the owner type, so this
//! crate flattens type arguments from *all* segments in outer-to-inner order.
//!
//! When the resolved [`nova_types::ClassDef`] has a known number of type parameters, we apply
//! small heuristics to reconcile mismatches (common for nested classes):
//!
//! 1. If the flattened argument count matches the class definition, use it.
//! 2. Otherwise, if the *last* segment's argument count matches, use only the last segment's
//!    arguments.
//! 3. Otherwise, if there are too many arguments, drop leading arguments (keep the suffix).
//! 4. Otherwise, if there are too few arguments, left-pad with [`nova_types::Type::Unknown`]
//!    until the expected arity is reached.
//!
//! These heuristics preserve as much information as possible but cannot fully model owner-type
//! generics; see JLS 4.8 / JVM signature grammar for the underlying semantics.

use std::collections::HashMap;

use nova_classfile::{
    BaseType, ClassSignature, ClassTypeSignature, FieldType, FieldTypeSignature, MethodDescriptor,
    MethodSignature, ReturnType, TypeArgument, TypeParameter, TypeSignature,
};
use nova_types::{ClassType, PrimitiveType, Type, TypeEnv, TypeStore, TypeVarId, WildcardBound};

/// A stack of type-variable scopes.
///
/// Lookups walk from inner to outer, so later scopes shadow earlier ones.
#[derive(Debug, Clone, Default)]
pub struct TypeVarScope {
    frames: Vec<HashMap<String, TypeVarId>>,
}

impl TypeVarScope {
    /// Creates an empty scope.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a new child scope that shadows bindings from `self`.
    pub fn child(&self) -> Self {
        let mut frames = self.frames.clone();
        frames.push(HashMap::new());
        Self { frames }
    }

    /// Inserts a binding into the current (innermost) scope frame.
    pub fn insert(&mut self, name: impl Into<String>, id: TypeVarId) {
        if self.frames.is_empty() {
            self.frames.push(HashMap::new());
        }
        self.frames
            .last_mut()
            .expect("TypeVarScope::frames is non-empty")
            .insert(name.into(), id);
    }

    /// Looks up a type variable by name, respecting shadowing.
    pub fn lookup(&self, name: &str) -> Option<TypeVarId> {
        self.frames
            .iter()
            .rev()
            .find_map(|frame| frame.get(name).copied())
    }
}

/// A convenience wrapper around translation functions that also manages type parameter
/// allocation into a [`TypeStore`].
pub struct SignatureTranslator<'a> {
    store: &'a mut TypeStore,
}

impl<'a> SignatureTranslator<'a> {
    pub fn new(store: &'a mut TypeStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &TypeStore {
        &*self.store
    }

    pub fn store_mut(&mut self) -> &mut TypeStore {
        self.store
    }

    pub fn ty_from_type_sig(&self, scope: &TypeVarScope, sig: &TypeSignature) -> Type {
        ty_from_type_sig(&*self.store, scope, sig)
    }

    pub fn ty_from_field_sig(&self, scope: &TypeVarScope, sig: &FieldTypeSignature) -> Type {
        ty_from_field_sig(&*self.store, scope, sig)
    }

    pub fn ty_from_descriptor_field(&self, desc: &FieldType) -> Type {
        ty_from_descriptor_field(&*self.store, desc)
    }

    pub fn class_sig_from_classfile(
        &mut self,
        enclosing_scope: &TypeVarScope,
        sig: &ClassSignature,
    ) -> (Vec<TypeVarId>, Option<Type>, Vec<Type>) {
        class_sig_from_classfile(self.store, enclosing_scope, sig)
    }

    pub fn method_sig_from_classfile(
        &mut self,
        class_scope: &TypeVarScope,
        sig: &MethodSignature,
        desc: &MethodDescriptor,
    ) -> (Vec<TypeVarId>, Vec<Type>, Type) {
        method_sig_from_classfile(self.store, class_scope, sig, desc)
    }
}

/// Converts a parsed classfile [`nova_classfile::TypeSignature`] into a [`Type`].
pub fn ty_from_type_sig(env: &dyn TypeEnv, scope: &TypeVarScope, sig: &TypeSignature) -> Type {
    match sig {
        TypeSignature::Base(base) => Type::Primitive(base_type_to_primitive(*base)),
        TypeSignature::Array(component) => {
            Type::Array(Box::new(ty_from_type_sig(env, scope, component.as_ref())))
        }
        TypeSignature::Class(class_sig) => ty_from_class_type_sig(env, scope, class_sig),
        TypeSignature::TypeVariable(name) => scope
            .lookup(name)
            .map(Type::TypeVar)
            .unwrap_or(Type::Unknown),
    }
}

/// Converts a parsed classfile [`nova_classfile::FieldTypeSignature`] into a [`Type`].
pub fn ty_from_field_sig(
    env: &dyn TypeEnv,
    scope: &TypeVarScope,
    sig: &FieldTypeSignature,
) -> Type {
    ty_from_type_sig(env, scope, sig)
}

/// Converts a field descriptor (erased type) into a [`Type`].
pub fn ty_from_descriptor_field(env: &dyn TypeEnv, desc: &FieldType) -> Type {
    match desc {
        FieldType::Base(base) => Type::Primitive(base_type_to_primitive(*base)),
        FieldType::Object(internal) => class_type_from_internal(env, internal, Vec::new()),
        FieldType::Array(component) => {
            Type::Array(Box::new(ty_from_descriptor_field(env, component.as_ref())))
        }
    }
}

fn ty_from_descriptor_return(env: &dyn TypeEnv, desc: &ReturnType) -> Type {
    match desc {
        ReturnType::Void => Type::Void,
        ReturnType::Type(field) => ty_from_descriptor_field(env, field),
    }
}

/// Converts a class signature attribute into type params and supertypes, allocating fresh
/// [`TypeVarId`]s in `store`.
pub fn class_sig_from_classfile(
    store: &mut TypeStore,
    enclosing_scope: &TypeVarScope,
    sig: &ClassSignature,
) -> (Vec<TypeVarId>, Option<Type>, Vec<Type>) {
    let base = store.type_param_count() as u32;
    let type_param_ids: Vec<TypeVarId> = sig
        .type_parameters
        .iter()
        .enumerate()
        .map(|(idx, _)| TypeVarId(base + idx as u32))
        .collect();

    let mut scope = enclosing_scope.child();
    for (tp, id) in sig
        .type_parameters
        .iter()
        .zip(type_param_ids.iter().copied())
    {
        scope.insert(tp.name.clone(), id);
    }

    let (defs, super_class, interfaces) = {
        let env: &dyn TypeEnv = &*store;
        let object_ty = default_object_type(env);

        let defs: Vec<(String, Vec<Type>)> = sig
            .type_parameters
            .iter()
            .map(|tp| {
                let bounds = upper_bounds_from_type_parameter(env, &scope, tp, &object_ty);
                (tp.name.clone(), bounds)
            })
            .collect();

        let super_ty = ty_from_class_type_sig(env, &scope, &sig.super_class);
        let super_class = if is_java_lang_object(env, &super_ty) {
            None
        } else {
            Some(super_ty)
        };

        let interfaces: Vec<Type> = sig
            .interfaces
            .iter()
            .map(|iface| ty_from_class_type_sig(env, &scope, iface))
            .collect();

        (defs, super_class, interfaces)
    };

    for (idx, (name, upper_bounds)) in defs.into_iter().enumerate() {
        let actual = store.add_type_param(name, upper_bounds);
        debug_assert_eq!(actual, type_param_ids[idx]);
    }

    (type_param_ids, super_class, interfaces)
}

/// Converts a method signature attribute into type params, parameter types, and return type,
/// allocating fresh [`TypeVarId`]s in `store`.
pub fn method_sig_from_classfile(
    store: &mut TypeStore,
    class_scope: &TypeVarScope,
    sig: &MethodSignature,
    desc: &MethodDescriptor,
) -> (Vec<TypeVarId>, Vec<Type>, Type) {
    let base = store.type_param_count() as u32;
    let type_param_ids: Vec<TypeVarId> = sig
        .type_parameters
        .iter()
        .enumerate()
        .map(|(idx, _)| TypeVarId(base + idx as u32))
        .collect();

    let mut scope = class_scope.child();
    for (tp, id) in sig
        .type_parameters
        .iter()
        .zip(type_param_ids.iter().copied())
    {
        scope.insert(tp.name.clone(), id);
    }

    let (defs, params, return_type) = {
        let env: &dyn TypeEnv = &*store;
        let object_ty = default_object_type(env);

        let defs: Vec<(String, Vec<Type>)> = sig
            .type_parameters
            .iter()
            .map(|tp| {
                let bounds = upper_bounds_from_type_parameter(env, &scope, tp, &object_ty);
                (tp.name.clone(), bounds)
            })
            .collect();

        let mut params = Vec::with_capacity(desc.params.len());
        for (idx, erased) in desc.params.iter().enumerate() {
            let translated = sig
                .parameters
                .get(idx)
                .map(|p| ty_from_type_sig(env, &scope, p))
                .unwrap_or_else(|| ty_from_descriptor_field(env, erased));

            let ty = if translated.is_errorish() {
                ty_from_descriptor_field(env, erased)
            } else {
                translated
            };
            params.push(ty);
        }

        let return_type = match sig.return_type.as_ref() {
            Some(ret_sig) => {
                let translated = ty_from_type_sig(env, &scope, ret_sig);
                if translated.is_errorish() {
                    ty_from_descriptor_return(env, &desc.return_type)
                } else {
                    translated
                }
            }
            None => match &desc.return_type {
                ReturnType::Void => Type::Void,
                ReturnType::Type(field) => ty_from_descriptor_field(env, field),
            },
        };

        (defs, params, return_type)
    };

    for (idx, (name, upper_bounds)) in defs.into_iter().enumerate() {
        let actual = store.add_type_param(name, upper_bounds);
        debug_assert_eq!(actual, type_param_ids[idx]);
    }

    (type_param_ids, params, return_type)
}

fn upper_bounds_from_type_parameter(
    env: &dyn TypeEnv,
    scope: &TypeVarScope,
    tp: &TypeParameter,
    default_object: &Type,
) -> Vec<Type> {
    if let Some(class_bound) = &tp.class_bound {
        let mut bounds = Vec::with_capacity(1 + tp.interface_bounds.len());
        bounds.push(ty_from_field_sig(env, scope, class_bound));
        bounds.extend(
            tp.interface_bounds
                .iter()
                .map(|b| ty_from_field_sig(env, scope, b)),
        );
        return bounds;
    }

    if !tp.interface_bounds.is_empty() {
        return tp
            .interface_bounds
            .iter()
            .map(|b| ty_from_field_sig(env, scope, b))
            .collect();
    }

    vec![default_object.clone()]
}

fn ty_from_class_type_sig(
    env: &dyn TypeEnv,
    scope: &TypeVarScope,
    sig: &ClassTypeSignature,
) -> Type {
    let binary_name = internal_to_binary_name(&sig.internal_name());
    let Some(def) = env.lookup_class(&binary_name) else {
        return Type::Named(binary_name);
    };

    let mut per_segment_args = Vec::with_capacity(sig.segments.len());
    for segment in &sig.segments {
        let mut args = Vec::with_capacity(segment.type_arguments.len());
        for arg in &segment.type_arguments {
            args.push(ty_from_type_argument(env, scope, arg));
        }
        per_segment_args.push(args);
    }

    let flattened_args: Vec<Type> = per_segment_args.iter().flatten().cloned().collect();
    let args = match env.class(def) {
        Some(class_def) => reconcile_class_args(
            class_def.type_params.len(),
            &per_segment_args,
            flattened_args,
        ),
        None => flattened_args,
    };

    Type::Class(ClassType { def, args })
}

fn ty_from_type_argument(env: &dyn TypeEnv, scope: &TypeVarScope, arg: &TypeArgument) -> Type {
    match arg {
        TypeArgument::Any => Type::Wildcard(WildcardBound::Unbounded),
        TypeArgument::Exact(inner) => ty_from_field_sig(env, scope, inner.as_ref()),
        TypeArgument::Extends(inner) => Type::Wildcard(WildcardBound::Extends(Box::new(
            ty_from_field_sig(env, scope, inner.as_ref()),
        ))),
        TypeArgument::Super(inner) => Type::Wildcard(WildcardBound::Super(Box::new(
            ty_from_field_sig(env, scope, inner.as_ref()),
        ))),
    }
}

fn reconcile_class_args(
    expected_len: usize,
    per_segment_args: &[Vec<Type>],
    flattened: Vec<Type>,
) -> Vec<Type> {
    if expected_len == 0 {
        // `expected_len == 0` can mean either:
        // 1) the target class truly has no type parameters, or
        // 2) we're currently looking at a placeholder `ClassDef` (common during
        //    cycle-safe loading), so the real arity is unknown.
        //
        // Dropping type arguments in case (2) loses useful information for IDE
        // scenarios (e.g. `Enum<E extends Enum<E>>`). Preserve the signature's
        // type arguments and let later passes reconcile once full class defs
        // are loaded.
        return flattened;
    }
    if flattened.len() == expected_len {
        return flattened;
    }

    if let Some(last) = per_segment_args.last() {
        if last.len() == expected_len {
            return last.clone();
        }
    }

    if flattened.len() > expected_len {
        let start = flattened.len().saturating_sub(expected_len);
        return flattened[start..].to_vec();
    }

    let missing = expected_len.saturating_sub(flattened.len());
    let mut out = Vec::with_capacity(expected_len);
    out.extend(std::iter::repeat(Type::Unknown).take(missing));
    out.extend(flattened);
    out
}

fn class_type_from_internal(env: &dyn TypeEnv, internal: &str, args: Vec<Type>) -> Type {
    let binary_name = internal_to_binary_name(internal);
    if let Some(id) = env.lookup_class(&binary_name) {
        Type::class(id, args)
    } else {
        Type::Named(binary_name)
    }
}

fn internal_to_binary_name(internal: &str) -> String {
    internal.replace('/', ".")
}

fn is_java_lang_object(env: &dyn TypeEnv, ty: &Type) -> bool {
    match ty {
        Type::Class(ClassType { def, args }) => {
            args.is_empty()
                && env
                    .lookup_class("java.lang.Object")
                    .is_some_and(|obj| obj == *def)
        }
        Type::Named(name) => name == "java.lang.Object",
        _ => false,
    }
}

fn default_object_type(env: &dyn TypeEnv) -> Type {
    env.lookup_class("java.lang.Object")
        .map(|id| Type::class(id, Vec::new()))
        .unwrap_or_else(|| Type::Named("java.lang.Object".to_string()))
}

fn base_type_to_primitive(base: BaseType) -> PrimitiveType {
    match base {
        BaseType::Boolean => PrimitiveType::Boolean,
        BaseType::Byte => PrimitiveType::Byte,
        BaseType::Short => PrimitiveType::Short,
        BaseType::Char => PrimitiveType::Char,
        BaseType::Int => PrimitiveType::Int,
        BaseType::Long => PrimitiveType::Long,
        BaseType::Float => PrimitiveType::Float,
        BaseType::Double => PrimitiveType::Double,
    }
}
