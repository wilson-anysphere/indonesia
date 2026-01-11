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
        let idx: u32 = self
            .locals
            .len()
            .try_into()
            .expect("too many context-local type params");
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

        let Some(class_def) = self.class(*def) else {
            return ty.clone();
        };

        let object = Type::class(self.well_known().object, vec![]);
        let formal_bounds: Vec<Type> = class_def
            .type_params
            .iter()
            .map(|tp| {
                self.type_param(*tp)
                    .and_then(|d| d.upper_bounds.first().cloned())
                    .unwrap_or_else(|| object.clone())
            })
            .collect();

        let mut new_args = Vec::with_capacity(args.len());
        for (idx, arg) in args.iter().enumerate() {
            match arg {
                Type::Wildcard(WildcardBound::Unbounded) => {
                    let upper = formal_bounds
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| object.clone());
                    let cap = self.add_capture_type_param(vec![upper], None);
                    new_args.push(Type::TypeVar(cap));
                }
                Type::Wildcard(WildcardBound::Extends(upper)) => {
                    let formal = formal_bounds
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| object.clone());
                    let glb = crate::glb(self, &formal, upper);
                    let cap = self.add_capture_type_param(vec![glb], None);
                    new_args.push(Type::TypeVar(cap));
                }
                Type::Wildcard(WildcardBound::Super(lower)) => {
                    let upper = formal_bounds
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| object.clone());
                    let cap = self.add_capture_type_param(vec![upper], Some((**lower).clone()));
                    new_args.push(Type::TypeVar(cap));
                }
                other => new_args.push(other.clone()),
            }
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
        let mut receiver = receiver.clone();
        if let Type::Named(n) = &receiver {
            if let Some(id) = self.lookup_class(n) {
                receiver = Type::class(id, vec![]);
            }
        }
        let receiver = self.capture_conversion(&receiver);
        crate::resolve_field(self, &receiver, name, call_kind)
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
