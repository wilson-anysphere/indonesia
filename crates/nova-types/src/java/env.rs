use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::{
    CallKind, ClassId, ClassType, FieldDef, Type, TypeEnv, TypeParamDef, TypeVarId, WildcardBound,
};

/// Per-invocation typing context used by overload resolution and related algorithms.
///
/// This is intentionally side-effect free with respect to the global [`crate::TypeStore`]:
/// capture conversion and other inference helpers can allocate context-local type parameters
/// without mutating shared state.
pub struct TyContext<'env> {
    base: &'env dyn TypeEnv,
    locals: Vec<TypeParamDef>,
}

impl fmt::Debug for TyContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TyContext")
            .field("locals", &self.locals)
            .finish_non_exhaustive()
    }
}

impl<'env> TyContext<'env> {
    pub fn new(base: &'env dyn TypeEnv) -> Self {
        Self {
            base,
            locals: Vec::new(),
        }
    }

    /// Normalize a receiver type for member lookup (field/method resolution).
    ///
    /// Java allows member access on type variables; those accesses are resolved against the
    /// type variable's upper bound(s) (or `Object` if no bound is specified).
    ///
    /// This helper also:
    /// - resolves `Type::Named` into `Type::Class` when possible
    /// - preserves intersection types (best-effort)
    /// - applies capture conversion for wildcard-containing parameterized types
    pub(crate) fn normalize_receiver_for_member_access(&mut self, receiver: &Type) -> Type {
        let object = Type::class(self.well_known().object, vec![]);

        fn normalize_intersection_for_member_access(env: &dyn TypeEnv, types: Vec<Type>) -> Type {
            // Flatten nested intersections without pruning: for member access we want to preserve as
            // much information as possible (e.g. keep interface bounds even when implied by a class),
            // but still canonicalize ordering so overload resolution is deterministic and prefers class
            // members over interface members when signatures collide.
            let mut flat = Vec::new();
            let mut stack = types;
            while let Some(t) = stack.pop() {
                match t {
                    Type::Intersection(parts) => stack.extend(parts),
                    other => flat.push(other),
                }
            }

            let mut seen = HashSet::new();
            let mut uniq = Vec::new();
            for t in flat {
                if seen.insert(t.clone()) {
                    uniq.push(t);
                }
            }

            if uniq.len() == 1 {
                return uniq.into_iter().next().unwrap();
            }

            uniq.sort_by_cached_key(|ty| {
                (
                    crate::intersection_component_rank(env, ty),
                    crate::type_sort_key(env, ty),
                )
            });

            Type::Intersection(uniq)
        }

        fn normalize_inner(ctx: &mut TyContext<'_>, ty: Type, depth: u8, object: &Type) -> Type {
            if depth == 0 {
                return ty;
            }

            match ty {
                Type::Named(name) => match ctx.lookup_class_by_source_name(&name) {
                    Some(id) => normalize_inner(ctx, Type::class(id, vec![]), depth - 1, object),
                    None => Type::Named(name),
                },
                Type::TypeVar(id) => {
                    let bounds = ctx
                        .type_param(id)
                        .map(|tp| tp.upper_bounds.clone())
                        .unwrap_or_default();
                    let replacement = match bounds.len() {
                        0 => object.clone(),
                        1 => bounds[0].clone(),
                        // Preserve *all* bounds so member lookup can see methods from any bound.
                        //
                        // Note: avoid `make_intersection` here since it intentionally prunes
                        // redundant supertypes based on Nova's best-effort `is_subtype`
                        // relation. That pruning treats error-ish types (`Unknown`/`Error`) as
                        // subtypes of everything, which can accidentally erase informative bounds
                        // for member access (e.g. `Unknown & I` collapsing to `Unknown`).
                        _ => normalize_intersection_for_member_access(ctx, bounds),
                    };
                    normalize_inner(ctx, replacement, depth - 1, object)
                }
                Type::Intersection(types) => {
                    let types = types
                        .into_iter()
                        .map(|t| normalize_inner(ctx, t, depth - 1, object))
                        .collect();
                    normalize_intersection_for_member_access(ctx, types)
                }
                Type::Wildcard(bound) => {
                    let replacement = match bound {
                        WildcardBound::Unbounded => object.clone(),
                        WildcardBound::Extends(upper) => *upper,
                        WildcardBound::Super(_) => object.clone(),
                    };
                    normalize_inner(ctx, replacement, depth - 1, object)
                }
                other => other,
            }
        }

        let normalized = normalize_inner(self, receiver.clone(), 8, &object);
        match normalized {
            Type::Intersection(types) => {
                let captured: Vec<Type> = types
                    .into_iter()
                    .map(|t| self.capture_conversion(&t))
                    .collect();
                normalize_intersection_for_member_access(self, captured)
            }
            other => self.capture_conversion(&other),
        }
    }

    /// Clear all context-local allocations.
    ///
    /// Callers that want deterministic IDs across repeated invocations should prefer creating a
    /// fresh context per invocation, but `reset` can be useful when reusing a context object.
    pub fn reset(&mut self) {
        self.locals.clear();
    }

    fn add_capture_type_param(
        &mut self,
        upper_bounds: Vec<Type>,
        lower_bound: Option<Type>,
    ) -> TypeVarId {
        // Context-local ids reserve the MSB as a "local" tag. If we ever exhaust the representable
        // range (or overflow `u32` on 64-bit platforms), degrade gracefully by reusing the last
        // representable id instead of panicking.
        let idx = match u32::try_from(self.locals.len()) {
            Ok(idx) if idx < TypeVarId::CONTEXT_LOCAL_BIT => idx,
            _ => return TypeVarId::new_context_local(TypeVarId::CONTEXT_LOCAL_BIT - 1),
        };
        let id = TypeVarId::new_context_local(idx);
        self.locals.push(TypeParamDef {
            name: format!("CAP#{}", idx),
            upper_bounds,
            lower_bound,
        });
        id
    }

    /// Capture conversion for parameterized types containing wildcards (JLS 5.1.10).
    ///
    /// This is a best-effort implementation intended for common IDE scenarios. It allocates fresh
    /// `TypeVarId`s inside this context (not in the global store) to represent capture variables.
    pub fn capture_conversion(&mut self, ty: &Type) -> Type {
        let Type::Class(ClassType { def, args }) = ty else {
            return ty.clone();
        };

        if args.iter().all(|a| !matches!(a, Type::Wildcard(_))) {
            return ty.clone();
        }

        let type_params = {
            let Some(class_def) = self.class(*def) else {
                return ty.clone();
            };
            if class_def.type_params.len() != args.len() {
                return ty.clone();
            }
            class_def.type_params.clone()
        };

        let object = Type::class(self.well_known().object, vec![]);

        // First pass: allocate capture ids for wildcard arguments and build a substitution mapping
        // from the class's formal type parameters to either the concrete argument or the capture var.
        let mut subst: HashMap<TypeVarId, Type> = HashMap::with_capacity(type_params.len());
        let mut capture_ids: Vec<Option<TypeVarId>> = Vec::with_capacity(args.len());

        for (formal, arg) in type_params.iter().copied().zip(args.iter()) {
            match arg {
                Type::Wildcard(_) => {
                    // Placeholder bounds are populated in the second pass.
                    let cap = self.add_capture_type_param(Vec::new(), None);
                    subst.insert(formal, Type::TypeVar(cap));
                    capture_ids.push(Some(cap));
                }
                other => {
                    subst.insert(formal, other.clone());
                    capture_ids.push(None);
                }
            }
        }

        // Second pass: compute bounds for each capture var, substituting references to the class's
        // formal type parameters with their captured counterparts. This supports common patterns like
        // `E extends Enum<E>` (self-referential bounds).
        for (idx, cap_opt) in capture_ids.iter().enumerate() {
            let Some(cap) = cap_opt else {
                continue;
            };
            let formal = type_params[idx];

            let mut upper_bounds = self
                .type_param(formal)
                .map(|tp| tp.upper_bounds.clone())
                .unwrap_or_default();
            if upper_bounds.is_empty() {
                upper_bounds.push(object.clone());
            }
            upper_bounds = upper_bounds
                .iter()
                .map(|b| crate::substitute(b, &subst))
                .collect();

            let mut lower_bound = None;
            match &args[idx] {
                Type::Wildcard(WildcardBound::Unbounded) => {}
                Type::Wildcard(WildcardBound::Extends(upper)) => {
                    upper_bounds.push(crate::substitute(upper, &subst));
                }
                Type::Wildcard(WildcardBound::Super(lower)) => {
                    lower_bound = Some(crate::substitute(lower, &subst));
                }
                _ => {}
            }

            // Remove redundant bounds (`Integer` implies `Object`, etc) for nicer diagnostics and
            // more stable comparisons.
            upper_bounds = simplify_upper_bounds(self, upper_bounds);

            if let Some(local_idx) = cap.context_local_index() {
                if let Some(def) = self.locals.get_mut(local_idx) {
                    def.upper_bounds = upper_bounds;
                    def.lower_bound = lower_bound;
                }
            }
        }

        // Build the captured type arguments in the class's formal parameter order.
        let mut new_args = Vec::with_capacity(args.len());
        for formal in &type_params {
            let Some(arg) = subst.get(formal) else {
                return ty.clone();
            };
            new_args.push(arg.clone());
        }

        Type::class(*def, new_args)
    }

    /// Resolve a field access against `receiver`, applying capture conversion first.
    pub fn resolve_field(
        &mut self,
        receiver: &Type,
        name: &str,
        call_kind: CallKind,
    ) -> Option<FieldDef> {
        let receiver = self.normalize_receiver_for_member_access(receiver);
        crate::resolve_field(self, &receiver, name, call_kind)
    }
}

fn simplify_upper_bounds(env: &dyn TypeEnv, bounds: Vec<Type>) -> Vec<Type> {
    match crate::make_intersection(env, bounds) {
        Type::Intersection(parts) => parts,
        other => vec![other],
    }
}

impl TypeEnv for TyContext<'_> {
    fn class(&self, id: ClassId) -> Option<&crate::ClassDef> {
        self.base.class(id)
    }

    fn type_param(&self, id: TypeVarId) -> Option<&TypeParamDef> {
        if let Some(idx) = id.context_local_index() {
            return self.locals.get(idx);
        }
        self.base.type_param(id)
    }

    fn lookup_class(&self, name: &str) -> Option<ClassId> {
        self.base.lookup_class(name)
    }

    fn well_known(&self) -> &crate::WellKnownTypes {
        self.base.well_known()
    }
}

impl TypeVarId {
    const CONTEXT_LOCAL_BIT: u32 = 1 << 31;

    pub(crate) fn new_context_local(index: u32) -> Self {
        Self(Self::CONTEXT_LOCAL_BIT | index)
    }

    pub(crate) fn context_local_index(self) -> Option<usize> {
        if (self.0 & Self::CONTEXT_LOCAL_BIT) == 0 {
            return None;
        }
        Some((self.0 & !Self::CONTEXT_LOCAL_BIT) as usize)
    }
}
