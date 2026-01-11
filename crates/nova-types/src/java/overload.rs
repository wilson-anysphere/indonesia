use crate::{MethodCall, MethodResolution, Type, TypeEnv};

use super::env::TyContext;

/// Resolve an instance method call against a receiver type using Java overload resolution rules
/// (best-effort).
///
/// This function is side-effect free with respect to the global environment: any capture
/// conversion allocations are performed in the supplied [`TyContext`].
pub fn resolve_method_call(ctx: &mut TyContext<'_>, call: &MethodCall<'_>) -> MethodResolution {
    let mut receiver = call.receiver.clone();
    if let Type::Named(name) = &receiver {
        if let Some(id) = ctx.lookup_class(name) {
            receiver = Type::class(id, vec![]);
        }
    }
    receiver = ctx.capture_conversion(&receiver);

    let env_ro: &dyn TypeEnv = &*ctx;
    crate::resolve_method_call_impl(env_ro, call, receiver)
}
