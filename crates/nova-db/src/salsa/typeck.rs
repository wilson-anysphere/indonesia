use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use nova_core::{Name, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_hir::hir::{
    AssignOp, BinaryOp, Body as HirBody, Expr as HirExpr, ExprId as HirExprId, LambdaBody,
    LiteralKind, Stmt as HirStmt, SwitchArmBody, SwitchLabel, UnaryOp,
};
use nova_hir::ids::{FieldId, MethodId};
use nova_hir::item_tree::{FieldKind, Modifiers};
use nova_modules::ModuleName;
use nova_resolve::expr_scopes::{ExprScopes, ResolvedValue as ResolvedLocal};
use nova_resolve::ids::{DefWithBodyId, ParamId};
use nova_resolve::{NameResolution, Resolution, ScopeKind, StaticMemberResolution, TypeResolution};
use nova_syntax::{lex, unescape_char_literal, JavaLanguageLevel, SyntaxKind, Token};
use nova_types::{
    assignment_conversion, assignment_conversion_with_const, binary_numeric_promotion,
    cast_conversion, format_resolved_method, format_type, infer_diamond_type_args, is_subtype, lub,
    CallKind, ClassDef, ClassId, ClassKind, ConstValue, ConstructorDef, Diagnostic, FieldDef,
    MethodCall, MethodCandidateFailureReason, MethodDef, MethodNotFound, MethodResolution,
    PrimitiveType, ResolvedMethod, Span, TyContext, Type, TypeEnv, TypeParamDef, TypeProvider,
    TypeStore, TypeVarId, TypeWarning, UncheckedReason, WildcardBound,
};
use nova_types_bridge::ExternalTypeLoader;

use crate::{FileId, ProjectId};

use super::cancellation as cancel;
use super::jpms::{module_for_file, JpmsProjectIndex, JpmsTypeProvider};
use super::resolve::NovaResolve;
use super::stats::HasQueryStats;
use super::{
    ArcEq, HasClassInterner, TrackedSalsaBodyMemo, TrackedSalsaMemo, TrackedSalsaProjectMemo,
    TrackedSalsaProjectModuleMemo,
};

struct WorkspaceFirstIndex<'a> {
    workspace: &'a nova_resolve::WorkspaceDefMap,
    classpath: Option<&'a dyn TypeIndex>,
}

impl TypeIndex for WorkspaceFirstIndex<'_> {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        self.workspace
            .resolve_type(name)
            .or_else(|| self.classpath.and_then(|cp| cp.resolve_type(name)))
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        self.workspace
            .resolve_type_in_package(package, name)
            .or_else(|| {
                self.classpath
                    .and_then(|cp| cp.resolve_type_in_package(package, name))
            })
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.workspace.package_exists(package)
            || self.classpath.is_some_and(|cp| cp.package_exists(package))
    }

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        self.workspace
            .resolve_static_member(owner, name)
            .or_else(|| {
                self.classpath
                    .and_then(|cp| cp.resolve_static_member(owner, name))
            })
    }
}

/// `TypeProvider` wrapper that prevents classpath/module-path stubs from shadowing `java.*`.
///
/// `nova_resolve::Resolver` intentionally ignores the classpath for `java.*` names to mirror JVM
/// restrictions (user class loaders cannot define classes in `java.*`). Type checking must match
/// this behavior: otherwise an unresolved `java.*` type (represented as `Type::Named`) could be
/// "rescued" by lazily loading a classpath stub via `ExternalTypeLoader::ensure_class`.
#[derive(Clone, Copy)]
struct JavaOnlyJdkTypeProvider<'a> {
    /// Provider chain for non-`java.*` names (e.g. classpath -> jdk).
    inner: &'a dyn TypeProvider,
    /// JDK provider used exclusively for `java.*`.
    jdk: &'a dyn TypeProvider,
}

impl<'a> JavaOnlyJdkTypeProvider<'a> {
    fn new(inner: &'a dyn TypeProvider, jdk: &'a dyn TypeProvider) -> Self {
        Self { inner, jdk }
    }
}

impl TypeProvider for JavaOnlyJdkTypeProvider<'_> {
    fn lookup_type(&self, binary_name: &str) -> Option<nova_types::TypeDefStub> {
        if binary_name.starts_with("java.") {
            self.jdk.lookup_type(binary_name)
        } else {
            self.inner.lookup_type(binary_name)
        }
    }

    fn members(&self, binary_name: &str) -> Vec<nova_types::MemberStub> {
        if binary_name.starts_with("java.") {
            self.jdk.members(binary_name)
        } else {
            self.inner.members(binary_name)
        }
    }

    fn supertypes(&self, binary_name: &str) -> Vec<String> {
        if binary_name.starts_with("java.") {
            self.jdk.supertypes(binary_name)
        } else {
            self.inner.supertypes(binary_name)
        }
    }
}

/// `TypeProvider` wrapper that prevents external stubs from shadowing workspace source types.
///
/// The external type loader (`ExternalTypeLoader`) recursively calls `ensure_class` while building
/// stubs to preload supertypes, interfaces, and referenced signature types. If an external class
/// (e.g. `Bar`) references a workspace-defined class name (e.g. `Foo`), those recursive loads must
/// not overwrite the workspace `ClassDef`.
///
/// This wrapper blocks provider lookups for any non-`java.*` binary name that exists in the
/// workspace definition map, ensuring the workspace definition wins even for *indirect* loads.
#[derive(Clone, Copy)]
struct WorkspaceShadowingTypeProvider<'a> {
    workspace: &'a nova_resolve::WorkspaceDefMap,
    inner: &'a dyn TypeProvider,
}

impl<'a> WorkspaceShadowingTypeProvider<'a> {
    fn new(workspace: &'a nova_resolve::WorkspaceDefMap, inner: &'a dyn TypeProvider) -> Self {
        Self { workspace, inner }
    }

    fn is_shadowed(&self, binary_name: &str) -> bool {
        // Mirror resolver behavior: application class loaders cannot define classes in `java.*`.
        // Even if the workspace contains a `java.*` definition, we should still allow loading the
        // real JDK type information for downstream type checking.
        if binary_name.starts_with("java.") {
            return false;
        }
        self.workspace.item_by_type_name_str(binary_name).is_some()
    }
}

impl TypeProvider for WorkspaceShadowingTypeProvider<'_> {
    fn lookup_type(&self, binary_name: &str) -> Option<nova_types::TypeDefStub> {
        if self.is_shadowed(binary_name) {
            None
        } else {
            self.inner.lookup_type(binary_name)
        }
    }

    fn members(&self, binary_name: &str) -> Vec<nova_types::MemberStub> {
        if self.is_shadowed(binary_name) {
            Vec::new()
        } else {
            self.inner.members(binary_name)
        }
    }

    fn supertypes(&self, binary_name: &str) -> Vec<String> {
        if self.is_shadowed(binary_name) {
            Vec::new()
        } else {
            self.inner.supertypes(binary_name)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileExprId {
    pub owner: DefWithBodyId,
    pub expr: HirExprId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyTypeckResult {
    pub env: ArcEq<TypeStore>,
    pub expr_types: Vec<Type>,
    pub call_resolutions: Vec<Option<ResolvedMethod>>,
    pub diagnostics: Vec<Diagnostic>,
    pub expected_return: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemandExprTypeckResult {
    pub env: ArcEq<TypeStore>,
    pub ty: Type,
    pub diagnostics: Vec<Diagnostic>,
}

fn const_value_for_expr(body: &HirBody, expr: HirExprId) -> Option<ConstValue> {
    match &body.exprs[expr] {
        HirExpr::Literal {
            kind: LiteralKind::Int,
            value,
            ..
        } => nova_syntax::parse_int_literal(value)
            .ok()
            .map(|v| ConstValue::Int(i64::from(v))),
        HirExpr::Literal {
            kind: LiteralKind::Long,
            value,
            ..
        } => nova_syntax::parse_long_literal(value)
            .ok()
            .map(ConstValue::Int),
        HirExpr::Literal {
            kind: LiteralKind::Char,
            value,
            ..
        } => unescape_char_literal(value)
            .ok()
            .map(|ch| ConstValue::Int(i64::from(u32::from(ch)))),
        HirExpr::Literal {
            kind: LiteralKind::Bool,
            value,
            ..
        } => match value.as_str() {
            "true" => Some(ConstValue::Boolean(true)),
            "false" => Some(ConstValue::Boolean(false)),
            _ => None,
        },
        HirExpr::Unary { op, expr, .. } => {
            let inner = const_value_for_expr(body, *expr);
            match (*op, inner) {
                (UnaryOp::Plus, Some(v)) => Some(v),
                (UnaryOp::Minus, Some(nova_types::ConstValue::Int(v))) => {
                    // Java integer constants use 32-bit two's complement arithmetic (JLS 4.2.2).
                    let v32 = i32::try_from(v).ok()?;
                    Some(nova_types::ConstValue::Int(i64::from(v32.wrapping_neg())))
                }
                (UnaryOp::Minus, None) => {
                    // JLS 3.10.1: `2147483648` and `9223372036854775808L` are only legal as the
                    // operand of unary `-`, yielding `Integer.MIN_VALUE`/`Long.MIN_VALUE`.
                    //
                    // `nova_syntax::parse_int_literal`/`parse_long_literal` intentionally reject
                    // the (MAX+1) forms, so we special-case them here for constant folding.
                    fn parse_decimal_u64(text: &str) -> Option<u64> {
                        let text: String = text.chars().filter(|c| *c != '_').collect();
                        if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
                            return None;
                        }
                        text.parse().ok()
                    }

                    match &body.exprs[*expr] {
                        HirExpr::Literal {
                            kind: LiteralKind::Int,
                            value,
                            ..
                        } if parse_decimal_u64(value) == Some((i32::MAX as u64) + 1) => {
                            Some(nova_types::ConstValue::Int(i64::from(i32::MIN)))
                        }
                        HirExpr::Literal {
                            kind: LiteralKind::Long,
                            value,
                            ..
                        } => {
                            let digits = value
                                .strip_suffix('l')
                                .or_else(|| value.strip_suffix('L'))
                                .unwrap_or(value);
                            if parse_decimal_u64(digits) == Some((i64::MAX as u64) + 1) {
                                Some(nova_types::ConstValue::Int(i64::MIN))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
                (UnaryOp::BitNot, Some(nova_types::ConstValue::Int(v))) => {
                    // Java bitwise operators on `int` operate on 32-bit two's complement values.
                    let v32 = i32::try_from(v).ok()?;
                    Some(nova_types::ConstValue::Int(i64::from(!v32)))
                }
                (UnaryOp::Not, Some(nova_types::ConstValue::Boolean(v))) => {
                    Some(nova_types::ConstValue::Boolean(!v))
                }
                _ => None,
            }
        }
        HirExpr::Binary { op, lhs, rhs, .. } => match op {
            // Short-circuit boolean operators.
            BinaryOp::AndAnd => match const_value_for_expr(body, *lhs)? {
                ConstValue::Boolean(false) => Some(ConstValue::Boolean(false)),
                ConstValue::Boolean(true) => match const_value_for_expr(body, *rhs)? {
                    ConstValue::Boolean(v) => Some(ConstValue::Boolean(v)),
                    _ => None,
                },
                _ => None,
            },
            BinaryOp::OrOr => match const_value_for_expr(body, *lhs)? {
                ConstValue::Boolean(true) => Some(ConstValue::Boolean(true)),
                ConstValue::Boolean(false) => match const_value_for_expr(body, *rhs)? {
                    ConstValue::Boolean(v) => Some(ConstValue::Boolean(v)),
                    _ => None,
                },
                _ => None,
            },
            // Non-short-circuit ops: evaluate both sides.
            _ => {
                let lhs = const_value_for_expr(body, *lhs)?;
                let rhs = const_value_for_expr(body, *rhs)?;
                match (*op, lhs, rhs) {
                    (BinaryOp::Add, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Int(i64::from(a32.wrapping_add(b32))))
                    }
                    (BinaryOp::Sub, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Int(i64::from(a32.wrapping_sub(b32))))
                    }
                    (BinaryOp::Mul, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Int(i64::from(a32.wrapping_mul(b32))))
                    }
                    (BinaryOp::Div, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        if b32 == 0 {
                            return None;
                        }
                        // Java defines `Integer.MIN_VALUE / -1 == Integer.MIN_VALUE` (overflow wraps).
                        let out = if a32 == i32::MIN && b32 == -1 {
                            i32::MIN
                        } else {
                            a32 / b32
                        };
                        Some(ConstValue::Int(i64::from(out)))
                    }
                    (BinaryOp::Rem, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        if b32 == 0 {
                            return None;
                        }
                        // Java defines `Integer.MIN_VALUE % -1 == 0` (overflow wraps).
                        let out = if a32 == i32::MIN && b32 == -1 {
                            0
                        } else {
                            a32 % b32
                        };
                        Some(ConstValue::Int(i64::from(out)))
                    }
                    (BinaryOp::BitAnd, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Int(i64::from(a32 & b32)))
                    }
                    (BinaryOp::BitOr, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Int(i64::from(a32 | b32)))
                    }
                    (BinaryOp::BitXor, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Int(i64::from(a32 ^ b32)))
                    }
                    (BinaryOp::BitAnd, ConstValue::Boolean(a), ConstValue::Boolean(b)) => {
                        Some(ConstValue::Boolean(a & b))
                    }
                    (BinaryOp::BitOr, ConstValue::Boolean(a), ConstValue::Boolean(b)) => {
                        Some(ConstValue::Boolean(a | b))
                    }
                    (BinaryOp::BitXor, ConstValue::Boolean(a), ConstValue::Boolean(b)) => {
                        Some(ConstValue::Boolean(a ^ b))
                    }
                    (BinaryOp::Shl, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        let shift = (b32 as u32) & 0x1f;
                        Some(ConstValue::Int(i64::from(a32.wrapping_shl(shift))))
                    }
                    (BinaryOp::Shr, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        let shift = (b32 as u32) & 0x1f;
                        Some(ConstValue::Int(i64::from(a32 >> shift)))
                    }
                    (BinaryOp::UShr, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        let shift = (b32 as u32) & 0x1f;
                        let out = ((a32 as u32) >> shift) as i32;
                        Some(ConstValue::Int(i64::from(out)))
                    }
                    (BinaryOp::EqEq, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Boolean(a32 == b32))
                    }
                    (BinaryOp::NotEq, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Boolean(a32 != b32))
                    }
                    (BinaryOp::EqEq, ConstValue::Boolean(a), ConstValue::Boolean(b)) => {
                        Some(ConstValue::Boolean(a == b))
                    }
                    (BinaryOp::NotEq, ConstValue::Boolean(a), ConstValue::Boolean(b)) => {
                        Some(ConstValue::Boolean(a != b))
                    }
                    (BinaryOp::Less, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Boolean(a32 < b32))
                    }
                    (BinaryOp::LessEq, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Boolean(a32 <= b32))
                    }
                    (BinaryOp::Greater, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Boolean(a32 > b32))
                    }
                    (BinaryOp::GreaterEq, ConstValue::Int(a), ConstValue::Int(b)) => {
                        let a32 = i32::try_from(a).ok()?;
                        let b32 = i32::try_from(b).ok()?;
                        Some(ConstValue::Boolean(a32 >= b32))
                    }
                    _ => None,
                }
            }
        },
        HirExpr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } => match const_value_for_expr(body, *condition)? {
            ConstValue::Boolean(true) => const_value_for_expr(body, *then_expr),
            ConstValue::Boolean(false) => const_value_for_expr(body, *else_expr),
            _ => None,
        },
        _ => None,
    }
}

#[ra_salsa::query_group(NovaTypeckStorage)]
pub trait NovaTypeck: NovaResolve + HasQueryStats + HasClassInterner {
    /// Project-scoped base [`TypeStore`] used as the starting point for per-body type checking.
    ///
    /// This store pre-interns project-local and external types (by name) so cloned body-local
    /// stores allocate stable [`nova_types::ClassId`]s independent of per-body loading order.
    fn project_base_type_store(&self, project: ProjectId) -> ArcEq<TypeStore>;
    /// Project-scoped base type checking metadata computed alongside [`NovaTypeck::project_base_type_store`].
    ///
    /// This is primarily a performance optimization for non-JPMS projects: per-body queries reuse
    /// the cached [`SourceTypes`] instead of re-defining the full workspace in every body-local
    /// `TypeStore`.
    fn project_base_type_store_data(&self, project: ProjectId) -> ProjectBaseTypeStoreData;

    /// Cached workspace-wide source typing metadata for a project.
    ///
    /// This is used by non-JPMS type checking queries to resolve workspace fields/methods and
    /// type parameters without scanning every file per body.
    fn project_source_types(&self, project: ProjectId) -> ArcEq<SourceTypes>;
    /// Per-body expression scope mapping used for lexical name resolution inside bodies.
    ///
    /// This is memoized independently from `typeck_body` so demand-driven, per-expression type
    /// queries can share the same `ExprScopes` without rebuilding it repeatedly.
    fn expr_scopes(&self, owner: DefWithBodyId) -> ArcEq<ExprScopes>;

    fn typeck_body(&self, owner: DefWithBodyId) -> Arc<BodyTypeckResult>;

    /// Like [`NovaTypeck::project_base_type_store`], but scoped to a specific JPMS "from" module.
    ///
    /// In JPMS mode we must avoid pre-interning *inaccessible* external types, otherwise the type
    /// reference parser's best-effort `TypeEnv` fallback can bypass JPMS checks.
    fn project_base_type_store_for_module(
        &self,
        project: ProjectId,
        from: ModuleName,
    ) -> ArcEq<TypeStore>;
    fn type_of_expr(&self, file: FileId, expr: FileExprId) -> Type;
    fn type_of_expr_demand_result(
        &self,
        file: FileId,
        expr: FileExprId,
    ) -> Arc<DemandExprTypeckResult>;
    fn type_of_expr_demand(&self, file: FileId, expr: FileExprId) -> Type;
    fn type_of_def(&self, def: DefWithBodyId) -> Type;

    fn resolve_method_call(&self, file: FileId, call_site: FileExprId) -> Option<ResolvedMethod>;
    fn resolve_method_call_demand(
        &self,
        file: FileId,
        call_site: FileExprId,
    ) -> Option<ResolvedMethod>;
    fn type_diagnostics(&self, file: FileId) -> Vec<Diagnostic>;

    /// Best-effort helper used by IDE features: infer the type of the smallest expression that
    /// encloses `offset` and return a formatted string.
    fn type_at_offset_display(&self, file: FileId, offset: u32) -> Option<String>;
}

fn type_of_expr(db: &dyn NovaTypeck, file: FileId, expr: FileExprId) -> Type {
    // Avoid mismatched keys (shouldn't happen, but this query is used by IDE features).
    if def_file(expr.owner) != file {
        return Type::Unknown;
    }
    // Demand-driven: avoid forcing `typeck_body(owner)` for IDE queries.
    db.type_of_expr_demand(file, expr)
}

fn type_of_expr_demand_result(
    db: &dyn NovaTypeck,
    file: FileId,
    expr: FileExprId,
) -> Arc<DemandExprTypeckResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!(
        "query",
        name = "type_of_expr_demand_result",
        owner = ?expr.owner,
        expr = ?expr.expr
    )
    .entered();

    cancel::check_cancelled(db);

    let owner = expr.owner;
    // Avoid mismatched keys (shouldn't happen, but this query is used by IDE features).
    if def_file(owner) != file {
        let env = ArcEq::new(Arc::new(TypeStore::with_minimal_jdk()));
        let result = Arc::new(DemandExprTypeckResult {
            env,
            ty: Type::Unknown,
            diagnostics: Vec::new(),
        });
        db.record_query_stat("type_of_expr_demand_result", start.elapsed());
        return result;
    }
    let file = def_file(owner);
    let java_level = db.java_language_level(file);
    let file_text_arc = db.file_content(file);
    let file_text = file_text_arc.as_str();
    let file_tokens = lex(file_text);
    let project = db.file_project(file);
    let jdk = db.jdk_index(project);
    let classpath = db.classpath_index(project);
    let workspace = db.workspace_def_map(project);
    let jpms_env = db.jpms_compilation_env(project);

    // JPMS-aware resolver + provider (when available).
    //
    // We keep the backing values (`jpms_ctx`, `workspace_index`, `chain_provider`) alive in this
    // scope so we can hand out references (`&dyn TypeIndex` / `&dyn TypeProvider`) that stay valid
    // for the rest of this demand-driven typeck path.
    let jpms_ctx = jpms_env.as_deref().map(|env| {
        let cfg = db.project_config(project);
        let file_rel = db.file_rel_path(file);
        let from = module_for_file(&cfg, file_rel.as_str());

        let index = JpmsProjectIndex {
            workspace: &workspace,
            graph: &env.env.graph,
            classpath: &env.classpath,
            jdk: &*jdk,
            from: from.clone(),
        };

        let provider = JpmsTypeProvider {
            graph: &env.env.graph,
            classpath: &env.classpath,
            jdk: &*jdk,
            from,
        };

        (index, provider)
    });

    let workspace_index = WorkspaceFirstIndex {
        workspace: &workspace,
        classpath: classpath.as_deref().map(|cp| cp as &dyn TypeIndex),
    };

    let resolver = if let Some((index, _)) = jpms_ctx.as_ref() {
        nova_resolve::Resolver::new(index)
    } else {
        nova_resolve::Resolver::new(&*jdk).with_classpath(&workspace_index)
    }
    .with_workspace(&workspace);

    let scopes = db.scope_graph(file);
    let body_scope = match owner {
        DefWithBodyId::Method(m) => scopes.method_scopes.get(&m).copied(),
        DefWithBodyId::Constructor(c) => scopes.constructor_scopes.get(&c).copied(),
        DefWithBodyId::Initializer(i) => scopes.initializer_scopes.get(&i).copied(),
    }
    .unwrap_or(scopes.file_scope);

    let tree = db.hir_item_tree(file);
    let body = match owner {
        DefWithBodyId::Method(m) => db.hir_body(m),
        DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
        DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
    };

    let expr_scopes = db.expr_scopes(owner);

    // Build an env for this body (same as `typeck_body`, but without whole-body checking).
    let base_store = if jpms_ctx.is_some() {
        let cfg = db.project_config(project);
        let file_rel = db.file_rel_path(file);
        let from = module_for_file(&cfg, file_rel.as_str());
        db.project_base_type_store_for_module(project, from)
    } else {
        db.project_base_type_store(project)
    };
    let mut store = (&*base_store).clone();

    let chain_provider = match classpath.as_deref() {
        Some(cp) => nova_types::ChainTypeProvider::new(vec![
            cp as &dyn TypeProvider,
            &*jdk as &dyn TypeProvider,
        ]),
        None => nova_types::ChainTypeProvider::new(vec![&*jdk as &dyn TypeProvider]),
    };

    let provider: &dyn TypeProvider = if let Some((_, provider)) = jpms_ctx.as_ref() {
        provider
    } else {
        &chain_provider
    };

    // Prevent classpath/module-path stubs from shadowing `java.*`.
    //
    // In JPMS mode we keep using the JPMS-aware provider (so `java.sql.*`/etc is still subject to
    // readability + exports checks), but for non-JPMS we wrap the legacy provider chain so `java.*`
    // loads exclusively from the JDK.
    let jdk_provider: &dyn TypeProvider = &*jdk;
    let java_only_provider = JavaOnlyJdkTypeProvider::new(provider, jdk_provider);
    let provider_for_loader: &dyn TypeProvider = if jpms_ctx.is_some() {
        provider
    } else {
        &java_only_provider
    };

    let shadowing_provider = WorkspaceShadowingTypeProvider::new(&workspace, provider_for_loader);
    let mut loader = ExternalTypeLoader::new(&mut store, &shadowing_provider);

    // Source typing metadata:
    // - In JPMS mode, type accessibility depends on the "from" module, so we must build the
    //   workspace view per query.
    // - In non-JPMS mode, reuse the project-level cached `SourceTypes` computed alongside
    //   `project_base_type_store`.
    let source_types_handle = if jpms_ctx.is_some() {
        SourceTypesHandle::Owned(define_workspace_source_types(
            db,
            project,
            file,
            &resolver,
            &mut loader,
        ))
    } else {
        SourceTypesHandle::Cached(db.project_source_types(project))
    };
    let source_types = source_types_handle.as_ref();
    let type_vars = type_vars_for_owner(
        &resolver,
        owner,
        body_scope,
        &scopes.scopes,
        &tree,
        &mut loader,
        &source_types.source_type_vars,
    );
    let field_types = &source_types.field_types;
    let field_owners = &source_types.field_owners;
    let method_owners = &source_types.method_owners;

    let (expected_return, signature_diags) = resolve_expected_return_type(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &type_vars,
        &mut loader,
    );
    let (param_types, param_diags) = resolve_param_types(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &type_vars,
        &mut loader,
    );

    let mut checker = BodyChecker::new(
        db,
        file_text,
        &file_tokens,
        owner,
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        &body,
        expr_scopes,
        type_vars,
        expected_return,
        param_types,
        field_types,
        field_owners,
        method_owners,
        java_level,
        true,
    );
    extend_type_ref_diagnostics(
        &mut checker.diagnostics,
        &file_tokens,
        file_text,
        signature_diags,
    );
    extend_type_ref_diagnostics(
        &mut checker.diagnostics,
        &file_tokens,
        file_text,
        param_diags,
    );

    // Best-effort expected-type seeding.
    //
    // Full `typeck_body` can infer more precise types by propagating expected types from
    // surrounding statements (`return`, typed local initializers, etc) into the target
    // expression. The demand-driven query doesn't walk every statement, but we still scan the
    // body once to recover expected types for the requested expression and to target-type
    // enclosing lambdas (so lambda parameters have usable types inside the body).
    let mut expected_ty: Option<Type> = None;
    if expr.expr.idx() < body.exprs.len() {
        let target_expr = expr.expr;
        let target_range = body.exprs[target_expr].range();
        let target_offset = target_range.start;
        let can_prune_stmts = !target_range.is_empty();

        let stmt_range = |stmt: nova_hir::hir::StmtId| match &body.stmts[stmt] {
            HirStmt::Block { range, .. }
            | HirStmt::Let { range, .. }
            | HirStmt::Expr { range, .. }
            | HirStmt::Yield { range, .. }
            | HirStmt::Assert { range, .. }
            | HirStmt::Return { range, .. }
            | HirStmt::If { range, .. }
            | HirStmt::While { range, .. }
            | HirStmt::For { range, .. }
            | HirStmt::ForEach { range, .. }
            | HirStmt::Synchronized { range, .. }
            | HirStmt::Switch { range, .. }
            | HirStmt::Try { range, .. }
            | HirStmt::Throw { range, .. }
            | HirStmt::Break { range, .. }
            | HirStmt::Continue { range, .. }
            | HirStmt::Empty { range, .. } => *range,
        };

        let stmt_may_contain_target = |stmt: nova_hir::hir::StmtId| {
            if !can_prune_stmts {
                return true;
            }
            let range = stmt_range(stmt);
            // Keep pruning best-effort: parse recovery can yield empty/degenerate spans.
            range.is_empty() || (range.start <= target_offset && target_offset < range.end)
        };

        // If the target expression is nested inside a larger statement-level expression (e.g.
        // within a lambda argument to a method call), infer that enclosing expression first so
        // context-sensitive typing (target-typed lambdas, assignments, etc) can populate the type
        // environment before we query the leaf expression's type.
        let mut inference_root: Option<(HirExprId, usize, Option<Type>)> = None;
        {
            let mut update_inference_root = |expr_id: HirExprId, expected: Option<Type>| {
                let range = body.exprs[expr_id].range();
                let size = range.end.saturating_sub(range.start);
                let replace = match inference_root {
                    Some((_, best_size, _)) => size < best_size,
                    None => true,
                };
                if replace {
                    inference_root = Some((expr_id, size, expected));
                }
            };

            let mut scan_steps: u32 = 0;
            let mut stack = vec![body.root];
            while let Some(stmt) = stack.pop() {
                cancel::checkpoint_cancelled_every(db, scan_steps, 256);
                scan_steps = scan_steps.wrapping_add(1);
                if !stmt_may_contain_target(stmt) {
                    continue;
                }
                match &body.stmts[stmt] {
                    HirStmt::Block { statements, .. } => {
                        if can_prune_stmts {
                            // Avoid scanning unrelated statements in large blocks (common case for
                            // IDE hovers). If we can identify at least one statement that contains
                            // the target offset, only visit those; otherwise fall back to scanning
                            // the full block to remain resilient under parse recovery.
                            let mut candidates = Vec::new();
                            for &child in statements {
                                cancel::checkpoint_cancelled_every(db, scan_steps, 256);
                                scan_steps = scan_steps.wrapping_add(1);
                                let range = stmt_range(child);
                                if !range.is_empty()
                                    && range.start <= target_offset
                                    && target_offset < range.end
                                {
                                    candidates.push(child);
                                }
                            }
                            if candidates.is_empty() {
                                stack.extend(statements.iter().rev().copied());
                            } else {
                                stack.extend(candidates.into_iter().rev());
                            }
                        } else {
                            stack.extend(statements.iter().rev().copied());
                        }
                    }
                    HirStmt::Let {
                        local, initializer, ..
                    } => {
                        if let Some(init) = initializer {
                            let local_data = &body.locals[*local];
                            let is_infer_var = local_data.ty_text.trim() == "var"
                                && checker.var_inference_enabled();
                            let declared_ty = if is_infer_var {
                                None
                            } else {
                                let decl_ty = checker.resolve_source_type(
                                    &mut loader,
                                    local_data.ty_text.as_str(),
                                    Some(local_data.ty_range),
                                );
                                (!decl_ty.is_errorish() && decl_ty != Type::Void).then_some(decl_ty)
                            };

                            // The initializer expression is a statement-level typing boundary; inferring
                            // it with an expected type (when available) target-types nested lambdas and
                            // improves generic/diamond inference.
                            {
                                let init_range = body.exprs[*init].range();
                                let may_contain = init_range.start <= target_range.start
                                    && target_range.end <= init_range.end;
                                if may_contain && contains_expr_in_expr(&body, *init, target_expr) {
                                    update_inference_root(*init, declared_ty.clone());
                                }
                            }

                            // If the target expression is the initializer of an explicitly-typed local,
                            // use the declared type as the expected type.
                            if *init == target_expr {
                                if let Some(decl_ty) = declared_ty.clone() {
                                    expected_ty = Some(decl_ty);
                                }
                            }

                            // If the target expression is inside a lambda initializer that has an
                            // explicit target type, seed the lambda parameter locals from the SAM
                            // signature without type-checking the entire lambda body.
                            if matches!(body.exprs[*init], HirExpr::Lambda { .. }) {
                                let init_range = body.exprs[*init].range();
                                let may_contain = init_range.start <= target_range.start
                                    && target_range.end <= init_range.end;
                                if may_contain && contains_expr_in_expr(&body, *init, target_expr) {
                                    if let Some(decl_ty) = declared_ty.as_ref() {
                                        seed_lambda_params_from_target(
                                            &mut checker,
                                            &mut loader,
                                            *init,
                                            decl_ty,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    HirStmt::Return { expr, .. } => {
                        if let Some(ret) = expr {
                            // Returning expressions are a statement-level typing boundary; infer the
                            // returned expression using the method's expected return type when
                            // available so nested poly expressions (lambdas/method refs) have a target.
                            {
                                let ret_range = body.exprs[*ret].range();
                                let may_contain = ret_range.start <= target_range.start
                                    && target_range.end <= ret_range.end;
                                if may_contain && contains_expr_in_expr(&body, *ret, target_expr) {
                                    let expected_ret = (!checker.expected_return.is_errorish()
                                        && checker.expected_return != Type::Void)
                                        .then_some(checker.expected_return.clone());
                                    update_inference_root(*ret, expected_ret);
                                }
                            }

                            if *ret == target_expr
                                && !checker.expected_return.is_errorish()
                                && checker.expected_return != Type::Void
                            {
                                expected_ty = Some(checker.expected_return.clone());
                            }

                            // If we are hovering inside a returned lambda, seed the lambda parameters
                            // from the method's return type (target typing) without walking the entire
                            // body.
                            if matches!(body.exprs[*ret], HirExpr::Lambda { .. }) {
                                let ret_range = body.exprs[*ret].range();
                                let may_contain = ret_range.start <= target_range.start
                                    && target_range.end <= ret_range.end;
                                if may_contain
                                    && contains_expr_in_expr(&body, *ret, target_expr)
                                    && !checker.expected_return.is_errorish()
                                {
                                    let expected_return = checker.expected_return.clone();
                                    seed_lambda_params_from_target(
                                        &mut checker,
                                        &mut loader,
                                        *ret,
                                        &expected_return,
                                    );
                                }
                            }
                        }
                    }
                    HirStmt::If {
                        condition,
                        then_branch,
                        else_branch,
                        ..
                    } => {
                        {
                            let cond_range = body.exprs[*condition].range();
                            let may_contain = cond_range.start <= target_range.start
                                && target_range.end <= cond_range.end;
                            if may_contain && contains_expr_in_expr(&body, *condition, target_expr)
                            {
                                update_inference_root(
                                    *condition,
                                    Some(Type::Primitive(PrimitiveType::Boolean)),
                                );
                            }
                        }
                        stack.push(*then_branch);
                        if let Some(else_branch) = else_branch {
                            stack.push(*else_branch);
                        }
                    }
                    HirStmt::While {
                        condition, body: b, ..
                    } => {
                        {
                            let cond_range = body.exprs[*condition].range();
                            let may_contain = cond_range.start <= target_range.start
                                && target_range.end <= cond_range.end;
                            if may_contain && contains_expr_in_expr(&body, *condition, target_expr)
                            {
                                update_inference_root(
                                    *condition,
                                    Some(Type::Primitive(PrimitiveType::Boolean)),
                                );
                            }
                        }
                        stack.push(*b)
                    }
                    HirStmt::For {
                        init,
                        condition,
                        update,
                        body: b,
                        ..
                    } => {
                        stack.extend(init.iter().rev().copied());

                        if let Some(condition) = condition {
                            let cond_range = body.exprs[*condition].range();
                            let may_contain = cond_range.start <= target_range.start
                                && target_range.end <= cond_range.end;
                            if may_contain && contains_expr_in_expr(&body, *condition, target_expr)
                            {
                                update_inference_root(
                                    *condition,
                                    Some(Type::Primitive(PrimitiveType::Boolean)),
                                );
                            }
                        }

                        // The `for` update list can contain multiple expressions; if the target lies
                        // within one of them, infer that expression first.
                        for upd in update {
                            let upd_range = body.exprs[*upd].range();
                            let may_contain = upd_range.start <= target_range.start
                                && target_range.end <= upd_range.end;
                            if may_contain && contains_expr_in_expr(&body, *upd, target_expr) {
                                update_inference_root(*upd, None);
                            }
                        }

                        // Assignment statements inside `for` update clauses should still get expected
                        // type seeding (e.g. `f = s -> s.length()`).
                        for upd in update {
                            let HirExpr::Assign { lhs, rhs, op, .. } = &body.exprs[*upd] else {
                                continue;
                            };
                            if *op != AssignOp::Assign {
                                continue;
                            }

                            let rhs_range = body.exprs[*rhs].range();
                            let may_contain = rhs_range.start <= target_range.start
                                && target_range.end <= rhs_range.end;
                            if !may_contain || !contains_expr_in_expr(&body, *rhs, target_expr) {
                                continue;
                            }

                            let lhs_ty = checker.infer_expr(&mut loader, *lhs).ty;
                            if lhs_ty.is_errorish() {
                                continue;
                            }

                            if *rhs == target_expr {
                                expected_ty = Some(lhs_ty.clone());
                            }

                            if matches!(body.exprs[*rhs], HirExpr::Lambda { .. }) {
                                seed_lambda_params_from_target(
                                    &mut checker,
                                    &mut loader,
                                    *rhs,
                                    &lhs_ty,
                                );
                            }
                        }

                        stack.push(*b);
                    }
                    HirStmt::ForEach {
                        iterable, body: b, ..
                    } => {
                        {
                            let it_range = body.exprs[*iterable].range();
                            let may_contain = it_range.start <= target_range.start
                                && target_range.end <= it_range.end;
                            if may_contain && contains_expr_in_expr(&body, *iterable, target_expr) {
                                update_inference_root(*iterable, None);
                            }
                        }
                        stack.push(*b)
                    }
                    HirStmt::Synchronized { expr, body: b, .. } => {
                        {
                            let lock_range = body.exprs[*expr].range();
                            let may_contain = lock_range.start <= target_range.start
                                && target_range.end <= lock_range.end;
                            if may_contain && contains_expr_in_expr(&body, *expr, target_expr) {
                                update_inference_root(*expr, None);
                            }
                        }
                        stack.push(*b)
                    }
                    HirStmt::Switch {
                        selector, body: b, ..
                    } => {
                        {
                            let sel_range = body.exprs[*selector].range();
                            let may_contain = sel_range.start <= target_range.start
                                && target_range.end <= sel_range.end;
                            if may_contain && contains_expr_in_expr(&body, *selector, target_expr) {
                                update_inference_root(*selector, None);
                            }
                        }
                        stack.push(*b)
                    }
                    HirStmt::Try {
                        body: b,
                        catches,
                        finally,
                        ..
                    } => {
                        stack.push(*b);
                        for catch in catches.iter().rev() {
                            stack.push(catch.body);
                        }
                        if let Some(finally) = finally {
                            stack.push(*finally);
                        }
                    }
                    HirStmt::Yield { expr, .. } => {
                        let Some(stmt_expr) = expr else {
                            continue;
                        };
                        // Yield statements are typing boundaries inside switch expressions; infer the
                        // yield expression first so nested target-typed expressions can see their
                        // context.
                        {
                            let stmt_range = body.exprs[*stmt_expr].range();
                            let may_contain = stmt_range.start <= target_range.start
                                && target_range.end <= stmt_range.end;
                            if may_contain && contains_expr_in_expr(&body, *stmt_expr, target_expr)
                            {
                                update_inference_root(*stmt_expr, None);
                            }
                        }

                        // Best-effort: propagate expected types through simple assignments in yield
                        // expressions, primarily so target-typed lambdas get parameter types.
                        let HirExpr::Assign { lhs, rhs, op, .. } = &body.exprs[*stmt_expr] else {
                            continue;
                        };
                        if *op != AssignOp::Assign {
                            continue;
                        }

                        let rhs_range = body.exprs[*rhs].range();
                        let may_contain = rhs_range.start <= target_range.start
                            && target_range.end <= rhs_range.end;
                        if !may_contain || !contains_expr_in_expr(&body, *rhs, target_expr) {
                            continue;
                        }

                        let lhs_ty = checker.infer_expr(&mut loader, *lhs).ty;
                        if lhs_ty.is_errorish() {
                            continue;
                        }

                        if *rhs == target_expr {
                            expected_ty = Some(lhs_ty.clone());
                        }

                        if matches!(body.exprs[*rhs], HirExpr::Lambda { .. }) {
                            seed_lambda_params_from_target(
                                &mut checker,
                                &mut loader,
                                *rhs,
                                &lhs_ty,
                            );
                        }
                    }
                    HirStmt::Expr {
                        expr: stmt_expr, ..
                    } => {
                        // Expression statements are also typing boundaries (e.g. method calls with
                        // lambda arguments). Infer the full expression statement first so nested
                        // target-typed expressions can see their context.
                        {
                            let stmt_range = body.exprs[*stmt_expr].range();
                            let may_contain = stmt_range.start <= target_range.start
                                && target_range.end <= stmt_range.end;
                            if may_contain && contains_expr_in_expr(&body, *stmt_expr, target_expr)
                            {
                                update_inference_root(*stmt_expr, None);
                            }
                        }

                        // Best-effort: propagate expected types through simple assignments in
                        // expression statements, primarily so target-typed lambdas get parameter types.
                        let HirExpr::Assign { lhs, rhs, op, .. } = &body.exprs[*stmt_expr] else {
                            continue;
                        };
                        if *op != AssignOp::Assign {
                            continue;
                        }

                        let rhs_range = body.exprs[*rhs].range();
                        let may_contain = rhs_range.start <= target_range.start
                            && target_range.end <= rhs_range.end;
                        if !may_contain || !contains_expr_in_expr(&body, *rhs, target_expr) {
                            continue;
                        }

                        let lhs_ty = checker.infer_expr(&mut loader, *lhs).ty;
                        if lhs_ty.is_errorish() {
                            continue;
                        }

                        if *rhs == target_expr {
                            expected_ty = Some(lhs_ty.clone());
                        }

                        if matches!(body.exprs[*rhs], HirExpr::Lambda { .. }) {
                            seed_lambda_params_from_target(
                                &mut checker,
                                &mut loader,
                                *rhs,
                                &lhs_ty,
                            );
                        }
                    }
                    HirStmt::Assert {
                        condition, message, ..
                    } => {
                        {
                            let cond_range = body.exprs[*condition].range();
                            let may_contain = cond_range.start <= target_range.start
                                && target_range.end <= cond_range.end;
                            if may_contain && contains_expr_in_expr(&body, *condition, target_expr)
                            {
                                update_inference_root(
                                    *condition,
                                    Some(Type::Primitive(PrimitiveType::Boolean)),
                                );
                            }
                        }
                        if let Some(message) = message {
                            let msg_range = body.exprs[*message].range();
                            let may_contain = msg_range.start <= target_range.start
                                && target_range.end <= msg_range.end;
                            if may_contain && contains_expr_in_expr(&body, *message, target_expr) {
                                update_inference_root(*message, None);
                            }
                        }
                    }
                    HirStmt::Throw { expr, .. } => {
                        let throw_range = body.exprs[*expr].range();
                        let may_contain = throw_range.start <= target_range.start
                            && target_range.end <= throw_range.end;
                        if may_contain && contains_expr_in_expr(&body, *expr, target_expr) {
                            update_inference_root(*expr, None);
                        }
                    }
                    HirStmt::Break { .. } | HirStmt::Continue { .. } | HirStmt::Empty { .. } => {}
                }
            }
        }

        if let Some((root_expr, _, expected)) = inference_root {
            let _ = checker.infer_expr_with_expected(&mut loader, root_expr, expected.as_ref());
        }
    }

    let ty = if expr.expr.idx() < body.exprs.len() {
        let target_expr = expr.expr;
        let target_typed_expr = if matches!(
            body.exprs[target_expr],
            HirExpr::Lambda { .. }
                | HirExpr::MethodReference { .. }
                | HirExpr::ConstructorReference { .. }
        ) || matches!(
            &body.exprs[target_expr],
            HirExpr::New { class, .. } if is_diamond_type_ref_text(class.as_str())
        ) {
            Some(target_expr)
        } else {
            // Hovering inside a lambda body should still recover target typing from the enclosing
            // argument position so lambda parameters have concrete types.
            let mut best_target_typed: Option<(HirExprId, usize)> = None;
            find_enclosing_target_typed_expr_in_stmt(
                &body,
                body.root,
                target_expr,
                &mut best_target_typed,
            );
            best_target_typed.map(|(expr, _)| expr)
        };

        // Target-typed expressions like lambdas and method references can pick up their type from
        // an enclosing call argument position. In the demand-driven query we don't type-check the
        // whole body, but we can still infer the immediate enclosing call to recover the parameter
        // target type (and seed lambda parameter locals).
        if expected_ty.is_none() {
            if let Some(target_typed_expr) = target_typed_expr {
                let mut best_call: Option<(HirExprId, usize)> = None;
                find_enclosing_call_with_arg_in_stmt(
                    &body,
                    body.root,
                    target_typed_expr,
                    &mut best_call,
                );
                if let Some((call_expr, _)) = best_call {
                    let _ = checker.infer_expr(&mut loader, call_expr);
                }
            }
        }

        match expected_ty.as_ref() {
            Some(expected) => {
                checker
                    .infer_expr_with_expected(&mut loader, target_expr, Some(expected))
                    .ty
            }
            None => checker.infer_expr(&mut loader, target_expr).ty,
        }
    } else {
        Type::Unknown
    };

    let diagnostics = checker.diagnostics;

    drop(loader);
    let env = ArcEq::new(Arc::new(store));

    let result = Arc::new(DemandExprTypeckResult {
        env,
        ty,
        diagnostics,
    });

    db.record_query_stat("type_of_expr_demand_result", start.elapsed());
    result
}

fn type_of_expr_demand(db: &dyn NovaTypeck, file: FileId, expr: FileExprId) -> Type {
    db.type_of_expr_demand_result(file, expr).ty.clone()
}

fn type_of_def(db: &dyn NovaTypeck, def: DefWithBodyId) -> Type {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "type_of_def", ?def).entered();

    cancel::check_cancelled(db);

    let ty = match def {
        DefWithBodyId::Method(m) => {
            let file = m.file;
            let project = db.file_project(file);
            let jdk = db.jdk_index(project);
            let classpath = db.classpath_index(project);
            let workspace = db.workspace_def_map(project);
            let jpms_env = db.jpms_compilation_env(project);

            // JPMS-aware resolver + provider (when available).
            //
            // Keep the backing values (`jpms_ctx`, `workspace_index`, `chain_provider`) alive in this
            // scope so we can hand out references (`&dyn TypeIndex` / `&dyn TypeProvider`) that stay
            // valid for the rest of this signature-only resolution path.
            let jpms_ctx = jpms_env.as_deref().map(|env| {
                let cfg = db.project_config(project);
                let file_rel = db.file_rel_path(file);
                let from = module_for_file(&cfg, file_rel.as_str());

                let index = JpmsProjectIndex {
                    workspace: &workspace,
                    graph: &env.env.graph,
                    classpath: &env.classpath,
                    jdk: &*jdk,
                    from: from.clone(),
                };

                let provider = JpmsTypeProvider {
                    graph: &env.env.graph,
                    classpath: &env.classpath,
                    jdk: &*jdk,
                    from,
                };

                (index, provider)
            });

            let workspace_index = WorkspaceFirstIndex {
                workspace: &workspace,
                classpath: classpath.as_deref().map(|cp| cp as &dyn TypeIndex),
            };

            let resolver = if let Some((index, _)) = jpms_ctx.as_ref() {
                nova_resolve::Resolver::new(index).with_workspace(&workspace)
            } else {
                nova_resolve::Resolver::new(&*jdk)
                    .with_classpath(&workspace_index)
                    .with_workspace(&workspace)
            };

            let scopes = db.scope_graph(file);
            let scope_id = scopes
                .method_scopes
                .get(&m)
                .copied()
                .unwrap_or(scopes.file_scope);

            let tree = db.hir_item_tree(file);

            // Signature-only type resolution: build a minimal type environment and resolve the
            // declared return type without touching the body HIR/typeck.
            let base_store = if jpms_ctx.is_some() {
                let cfg = db.project_config(project);
                let file_rel = db.file_rel_path(file);
                let from = module_for_file(&cfg, file_rel.as_str());
                db.project_base_type_store_for_module(project, from)
            } else {
                db.project_base_type_store(project)
            };
            let mut store = (&*base_store).clone();

            let chain_provider = match classpath.as_deref() {
                Some(cp) => nova_types::ChainTypeProvider::new(vec![
                    cp as &dyn TypeProvider,
                    &*jdk as &dyn TypeProvider,
                ]),
                None => nova_types::ChainTypeProvider::new(vec![&*jdk as &dyn TypeProvider]),
            };

            let provider: &dyn TypeProvider = if let Some((_, provider)) = jpms_ctx.as_ref() {
                provider
            } else {
                &chain_provider
            };

            let jdk_provider: &dyn TypeProvider = &*jdk;
            let java_only_provider = JavaOnlyJdkTypeProvider::new(provider, jdk_provider);
            let provider_for_loader: &dyn TypeProvider = if jpms_ctx.is_some() {
                provider
            } else {
                &java_only_provider
            };

            let shadowing_provider =
                WorkspaceShadowingTypeProvider::new(&workspace, provider_for_loader);
            let mut loader = ExternalTypeLoader::new(&mut store, &shadowing_provider);

            // Define source types in this file so `Type::Class` ids are stable.
            let SourceTypes {
                source_type_vars, ..
            } = define_source_types(&resolver, &scopes, &tree, &mut loader);

            let type_vars = type_vars_for_owner(
                &resolver,
                def,
                scope_id,
                &scopes.scopes,
                &tree,
                &mut loader,
                &source_type_vars,
            );

            let (ty, _diags) = resolve_expected_return_type(
                &resolver,
                &scopes.scopes,
                scope_id,
                &tree,
                def,
                &type_vars,
                &mut loader,
            );
            ty
        }
        DefWithBodyId::Constructor(_) | DefWithBodyId::Initializer(_) => Type::Void,
    };

    db.record_query_stat("type_of_def", start.elapsed());
    ty
}

fn resolve_method_call(
    db: &dyn NovaTypeck,
    file: FileId,
    call_site: FileExprId,
) -> Option<ResolvedMethod> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "resolve_method_call", ?file, ?call_site).entered();

    cancel::check_cancelled(db);

    // Avoid mismatched keys (shouldn't happen, but this query is used by IDE features).
    if def_file(call_site.owner) != file {
        db.record_query_stat("resolve_method_call", start.elapsed());
        return None;
    }

    // Demand-driven call resolution: avoid running `typeck_body` for the entire owner.
    let resolved = db.resolve_method_call_demand(file, call_site);
    db.record_query_stat("resolve_method_call", start.elapsed());
    resolved
}

fn resolve_method_call_demand(
    db: &dyn NovaTypeck,
    file: FileId,
    call_site: FileExprId,
) -> Option<ResolvedMethod> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "resolve_method_call_demand", ?call_site).entered();

    cancel::check_cancelled(db);

    let owner = call_site.owner;
    // Avoid mismatched keys (shouldn't happen, but this query is used by IDE features).
    if def_file(owner) != file {
        db.record_query_stat("resolve_method_call_demand", start.elapsed());
        return None;
    }
    let file = def_file(owner);
    let java_level = db.java_language_level(file);
    let file_text_arc = db.file_content(file);
    let file_text = file_text_arc.as_str();
    let file_tokens = lex(file_text);
    let project = db.file_project(file);
    let jdk = db.jdk_index(project);
    let classpath = db.classpath_index(project);
    let workspace = db.workspace_def_map(project);
    let jpms_env = db.jpms_compilation_env(project);
    // JPMS-aware resolver + provider (when available).
    let jpms_ctx = jpms_env.as_deref().map(|env| {
        let cfg = db.project_config(project);
        let file_rel = db.file_rel_path(file);
        let from = module_for_file(&cfg, file_rel.as_str());

        let index = JpmsProjectIndex {
            workspace: &workspace,
            graph: &env.env.graph,
            classpath: &env.classpath,
            jdk: &*jdk,
            from: from.clone(),
        };

        let provider = JpmsTypeProvider {
            graph: &env.env.graph,
            classpath: &env.classpath,
            jdk: &*jdk,
            from,
        };

        (index, provider)
    });

    let workspace_index = WorkspaceFirstIndex {
        workspace: &workspace,
        classpath: classpath.as_deref().map(|cp| cp as &dyn TypeIndex),
    };

    let resolver = if let Some((index, _)) = jpms_ctx.as_ref() {
        nova_resolve::Resolver::new(index).with_workspace(&workspace)
    } else {
        nova_resolve::Resolver::new(&*jdk)
            .with_classpath(&workspace_index)
            .with_workspace(&workspace)
    };

    let scopes = db.scope_graph(file);
    let body_scope = match owner {
        DefWithBodyId::Method(m) => scopes.method_scopes.get(&m).copied(),
        DefWithBodyId::Constructor(c) => scopes.constructor_scopes.get(&c).copied(),
        DefWithBodyId::Initializer(i) => scopes.initializer_scopes.get(&i).copied(),
    }
    .unwrap_or(scopes.file_scope);

    let tree = db.hir_item_tree(file);
    let body = match owner {
        DefWithBodyId::Method(m) => db.hir_body(m),
        DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
        DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
    };

    if call_site.expr.idx() >= body.exprs.len() {
        return None;
    }

    // Only resolve actual call-site expressions.
    //
    // `call_resolutions` is used for both method calls and constructor calls (from `new`
    // expressions), so allow both HIR variants here.
    if !matches!(
        &body.exprs[call_site.expr],
        HirExpr::Call { .. } | HirExpr::New { .. }
    ) {
        return None;
    }

    let expr_scopes = db.expr_scopes(owner);

    // Build an env for this body (same setup as `typeck_body`, but without running `check_body`).
    let base_store = if jpms_ctx.is_some() {
        let cfg = db.project_config(project);
        let file_rel = db.file_rel_path(file);
        let from = module_for_file(&cfg, file_rel.as_str());
        db.project_base_type_store_for_module(project, from)
    } else {
        db.project_base_type_store(project)
    };
    let mut store = (&*base_store).clone();

    let chain_provider = match classpath.as_deref() {
        Some(cp) => nova_types::ChainTypeProvider::new(vec![
            cp as &dyn TypeProvider,
            &*jdk as &dyn TypeProvider,
        ]),
        None => nova_types::ChainTypeProvider::new(vec![&*jdk as &dyn TypeProvider]),
    };

    let provider: &dyn TypeProvider = if let Some((_, provider)) = jpms_ctx.as_ref() {
        provider
    } else {
        &chain_provider
    };

    let jdk_provider: &dyn TypeProvider = &*jdk;
    let java_only_provider = JavaOnlyJdkTypeProvider::new(provider, jdk_provider);
    let provider_for_loader: &dyn TypeProvider = if jpms_ctx.is_some() {
        provider
    } else {
        &java_only_provider
    };

    let shadowing_provider = WorkspaceShadowingTypeProvider::new(&workspace, provider_for_loader);
    let mut loader = ExternalTypeLoader::new(&mut store, &shadowing_provider);

    // Source typing metadata:
    // - In JPMS mode, type accessibility depends on the "from" module, so we must build the
    //   workspace view per query.
    // - In non-JPMS mode, reuse the project-level cached `SourceTypes` computed alongside
    //   `project_base_type_store`.
    let source_types_handle = if jpms_ctx.is_some() {
        SourceTypesHandle::Owned(define_workspace_source_types(
            db,
            project,
            file,
            &resolver,
            &mut loader,
        ))
    } else {
        SourceTypesHandle::Cached(db.project_source_types(project))
    };
    let source_types = source_types_handle.as_ref();
    let field_types = &source_types.field_types;
    let field_owners = &source_types.field_owners;
    let method_owners = &source_types.method_owners;
    let source_type_vars = &source_types.source_type_vars;

    let type_vars = type_vars_for_owner(
        &resolver,
        owner,
        body_scope,
        &scopes.scopes,
        &tree,
        &mut loader,
        source_type_vars,
    );

    let (expected_return, _) = resolve_expected_return_type(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &type_vars,
        &mut loader,
    );
    let (param_types, _) = resolve_param_types(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &type_vars,
        &mut loader,
    );

    let mut checker = BodyChecker::new(
        db,
        file_text,
        &file_tokens,
        owner,
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        &body,
        expr_scopes,
        type_vars,
        expected_return,
        param_types,
        field_types,
        field_owners,
        method_owners,
        java_level,
        true,
    );

    // Best-effort local type table for locals with explicit types. This improves overload
    // resolution for arguments like `foo(x)` without walking the whole body.
    for (idx, local) in body.locals.iter() {
        if local.ty_text.trim() == "var" && checker.var_inference_enabled() {
            continue;
        }
        let idx = idx as usize;
        if idx >= checker.local_types.len() {
            continue;
        }
        let ty = checker.resolve_source_type(&mut loader, &local.ty_text, Some(local.ty_range));
        checker.local_types[idx] = ty;
        checker.local_ty_states[idx] = LocalTyState::Computed;
    }

    // Best-effort: infer a *minimal enclosing expression* that still propagates target typing.
    //
    // This allows demand-driven method resolution to benefit from expected-type propagation through
    // expressions like `return cond ? foo() : bar();` or `x = cond ? foo() : bar();` without running
    // full-body typeck.
    let (root_expr, expected) = {
        let mut parent_expr: Vec<Option<HirExprId>> = vec![None; body.exprs.len()];
        let mut visited_expr: Vec<bool> = vec![false; body.exprs.len()];

        fn visit_expr(
            body: &HirBody,
            expr: HirExprId,
            parent_expr: &mut [Option<HirExprId>],
            visited_expr: &mut [bool],
        ) {
            if visited_expr.get(expr.idx()).copied().unwrap_or(false) {
                return;
            }
            if let Some(slot) = visited_expr.get_mut(expr.idx()) {
                *slot = true;
            }

            let set_parent = |parent_expr: &mut [Option<HirExprId>], child: HirExprId| {
                if child.idx() < parent_expr.len() && parent_expr[child.idx()].is_none() {
                    parent_expr[child.idx()] = Some(expr);
                }
            };

            match &body.exprs[expr] {
                HirExpr::Invalid { children, .. } => {
                    for child in children {
                        set_parent(parent_expr, *child);
                        visit_expr(body, *child, parent_expr, visited_expr);
                    }
                }
                HirExpr::FieldAccess { receiver, .. }
                | HirExpr::MethodReference { receiver, .. }
                | HirExpr::ConstructorReference { receiver, .. } => {
                    set_parent(parent_expr, *receiver);
                    visit_expr(body, *receiver, parent_expr, visited_expr);
                }
                HirExpr::ArrayAccess { array, index, .. } => {
                    set_parent(parent_expr, *array);
                    set_parent(parent_expr, *index);
                    visit_expr(body, *array, parent_expr, visited_expr);
                    visit_expr(body, *index, parent_expr, visited_expr);
                }
                HirExpr::ClassLiteral { ty, .. } => {
                    set_parent(parent_expr, *ty);
                    visit_expr(body, *ty, parent_expr, visited_expr);
                }
                HirExpr::Call { callee, args, .. } => {
                    set_parent(parent_expr, *callee);
                    visit_expr(body, *callee, parent_expr, visited_expr);
                    for arg in args {
                        set_parent(parent_expr, *arg);
                        visit_expr(body, *arg, parent_expr, visited_expr);
                    }
                }
                HirExpr::New { args, .. } => {
                    for arg in args {
                        set_parent(parent_expr, *arg);
                        visit_expr(body, *arg, parent_expr, visited_expr);
                    }
                }
                HirExpr::ArrayCreation {
                    dim_exprs,
                    initializer,
                    ..
                } => {
                    for dim_expr in dim_exprs {
                        set_parent(parent_expr, *dim_expr);
                        visit_expr(body, *dim_expr, parent_expr, visited_expr);
                    }
                    if let Some(init) = initializer {
                        set_parent(parent_expr, *init);
                        visit_expr(body, *init, parent_expr, visited_expr);
                    }
                }
                HirExpr::ArrayInitializer { items, .. } => {
                    for item in items {
                        set_parent(parent_expr, *item);
                        visit_expr(body, *item, parent_expr, visited_expr);
                    }
                }
                HirExpr::Unary { expr: inner, .. } => {
                    set_parent(parent_expr, *inner);
                    visit_expr(body, *inner, parent_expr, visited_expr);
                }
                HirExpr::Cast { expr: inner, .. } | HirExpr::Instanceof { expr: inner, .. } => {
                    set_parent(parent_expr, *inner);
                    visit_expr(body, *inner, parent_expr, visited_expr);
                }
                HirExpr::Binary { lhs, rhs, .. } | HirExpr::Assign { lhs, rhs, .. } => {
                    set_parent(parent_expr, *lhs);
                    set_parent(parent_expr, *rhs);
                    visit_expr(body, *lhs, parent_expr, visited_expr);
                    visit_expr(body, *rhs, parent_expr, visited_expr);
                }
                HirExpr::Conditional {
                    condition,
                    then_expr,
                    else_expr,
                    ..
                } => {
                    set_parent(parent_expr, *condition);
                    set_parent(parent_expr, *then_expr);
                    set_parent(parent_expr, *else_expr);
                    visit_expr(body, *condition, parent_expr, visited_expr);
                    visit_expr(body, *then_expr, parent_expr, visited_expr);
                    visit_expr(body, *else_expr, parent_expr, visited_expr);
                }
                HirExpr::Switch { selector, arms, .. } => {
                    set_parent(parent_expr, *selector);
                    visit_expr(body, *selector, parent_expr, visited_expr);

                    for arm in arms {
                        for label in &arm.labels {
                            match label {
                                SwitchLabel::Case { values, .. } => {
                                    for value in values {
                                        set_parent(parent_expr, *value);
                                        visit_expr(body, *value, parent_expr, visited_expr);
                                    }
                                }
                                SwitchLabel::Default { .. } => {}
                            }
                        }

                        match &arm.body {
                            SwitchArmBody::Expr(expr_id) => {
                                set_parent(parent_expr, *expr_id);
                                visit_expr(body, *expr_id, parent_expr, visited_expr);
                            }
                            SwitchArmBody::Block(stmt_id) | SwitchArmBody::Stmt(stmt_id) => {
                                visit_stmt(body, *stmt_id, Some(expr), parent_expr, visited_expr);
                            }
                        }
                    }
                }
                HirExpr::Lambda { body: b, .. } => match b {
                    LambdaBody::Expr(expr_id) => {
                        set_parent(parent_expr, *expr_id);
                        visit_expr(body, *expr_id, parent_expr, visited_expr);
                    }
                    LambdaBody::Block(stmt_id) => {
                        visit_stmt(body, *stmt_id, Some(expr), parent_expr, visited_expr);
                    }
                },
                HirExpr::Name { .. }
                | HirExpr::Literal { .. }
                | HirExpr::Null { .. }
                | HirExpr::This { .. }
                | HirExpr::Super { .. }
                | HirExpr::Missing { .. } => {}
            }
        }

        fn visit_stmt(
            body: &HirBody,
            stmt: nova_hir::hir::StmtId,
            enclosing_expr: Option<HirExprId>,
            parent_expr: &mut [Option<HirExprId>],
            visited_expr: &mut [bool],
        ) {
            let set_expr_parent = |parent_expr: &mut [Option<HirExprId>],
                                   enclosing_expr: Option<HirExprId>,
                                   child: HirExprId| {
                let Some(enclosing) = enclosing_expr else {
                    return;
                };
                if child.idx() < parent_expr.len() && parent_expr[child.idx()].is_none() {
                    parent_expr[child.idx()] = Some(enclosing);
                }
            };

            match &body.stmts[stmt] {
                HirStmt::Block { statements, .. } => {
                    for stmt in statements {
                        visit_stmt(body, *stmt, enclosing_expr, parent_expr, visited_expr);
                    }
                }
                HirStmt::Let { initializer, .. } => {
                    if let Some(expr) = initializer {
                        set_expr_parent(parent_expr, enclosing_expr, *expr);
                        visit_expr(body, *expr, parent_expr, visited_expr);
                    }
                }
                HirStmt::Expr { expr, .. } => {
                    set_expr_parent(parent_expr, enclosing_expr, *expr);
                    visit_expr(body, *expr, parent_expr, visited_expr)
                }
                HirStmt::Assert {
                    condition, message, ..
                } => {
                    set_expr_parent(parent_expr, enclosing_expr, *condition);
                    visit_expr(body, *condition, parent_expr, visited_expr);
                    if let Some(expr) = message {
                        set_expr_parent(parent_expr, enclosing_expr, *expr);
                        visit_expr(body, *expr, parent_expr, visited_expr);
                    }
                }
                HirStmt::Yield { expr, .. } => {
                    if let Some(expr) = expr {
                        set_expr_parent(parent_expr, enclosing_expr, *expr);
                        visit_expr(body, *expr, parent_expr, visited_expr);
                    }
                }
                HirStmt::Return { expr, .. } => {
                    if let Some(expr) = expr {
                        set_expr_parent(parent_expr, enclosing_expr, *expr);
                        visit_expr(body, *expr, parent_expr, visited_expr);
                    }
                }
                HirStmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    set_expr_parent(parent_expr, enclosing_expr, *condition);
                    visit_expr(body, *condition, parent_expr, visited_expr);
                    visit_stmt(
                        body,
                        *then_branch,
                        enclosing_expr,
                        parent_expr,
                        visited_expr,
                    );
                    if let Some(stmt) = else_branch {
                        visit_stmt(body, *stmt, enclosing_expr, parent_expr, visited_expr);
                    }
                }
                HirStmt::While {
                    condition, body: b, ..
                } => {
                    set_expr_parent(parent_expr, enclosing_expr, *condition);
                    visit_expr(body, *condition, parent_expr, visited_expr);
                    visit_stmt(body, *b, enclosing_expr, parent_expr, visited_expr);
                }
                HirStmt::For {
                    init,
                    condition,
                    update,
                    body: b,
                    ..
                } => {
                    for stmt in init {
                        visit_stmt(body, *stmt, enclosing_expr, parent_expr, visited_expr);
                    }
                    if let Some(expr) = condition {
                        set_expr_parent(parent_expr, enclosing_expr, *expr);
                        visit_expr(body, *expr, parent_expr, visited_expr);
                    }
                    for expr in update {
                        set_expr_parent(parent_expr, enclosing_expr, *expr);
                        visit_expr(body, *expr, parent_expr, visited_expr);
                    }
                    visit_stmt(body, *b, enclosing_expr, parent_expr, visited_expr);
                }
                HirStmt::ForEach {
                    iterable, body: b, ..
                } => {
                    set_expr_parent(parent_expr, enclosing_expr, *iterable);
                    visit_expr(body, *iterable, parent_expr, visited_expr);
                    visit_stmt(body, *b, enclosing_expr, parent_expr, visited_expr);
                }
                HirStmt::Synchronized { expr, body: b, .. } => {
                    set_expr_parent(parent_expr, enclosing_expr, *expr);
                    visit_expr(body, *expr, parent_expr, visited_expr);
                    visit_stmt(body, *b, enclosing_expr, parent_expr, visited_expr);
                }
                HirStmt::Switch {
                    selector, body: b, ..
                } => {
                    set_expr_parent(parent_expr, enclosing_expr, *selector);
                    visit_expr(body, *selector, parent_expr, visited_expr);
                    visit_stmt(body, *b, enclosing_expr, parent_expr, visited_expr);
                }
                HirStmt::Try {
                    body: b,
                    catches,
                    finally,
                    ..
                } => {
                    visit_stmt(body, *b, enclosing_expr, parent_expr, visited_expr);
                    for catch in catches {
                        visit_stmt(body, catch.body, enclosing_expr, parent_expr, visited_expr);
                    }
                    if let Some(stmt) = finally {
                        visit_stmt(body, *stmt, enclosing_expr, parent_expr, visited_expr);
                    }
                }
                HirStmt::Throw { expr, .. } => {
                    set_expr_parent(parent_expr, enclosing_expr, *expr);
                    visit_expr(body, *expr, parent_expr, visited_expr)
                }
                HirStmt::Break { .. } | HirStmt::Continue { .. } | HirStmt::Empty { .. } => {}
            }
        }

        visit_stmt(&body, body.root, None, &mut parent_expr, &mut visited_expr);

        // Map "expected types" for expression roots that are directly target-typed by a statement.
        let mut expected_roots: Vec<Option<Type>> = vec![None; body.exprs.len()];
        fn collect_expected_roots(
            body: &HirBody,
            stmt: nova_hir::hir::StmtId,
            expected_return: &Type,
            local_types: &[Type],
            expected_roots: &mut [Option<Type>],
        ) {
            match &body.stmts[stmt] {
                HirStmt::Return {
                    expr: Some(expr), ..
                } => {
                    if !expected_return.is_errorish() && expr.idx() < expected_roots.len() {
                        expected_roots[expr.idx()] = Some(expected_return.clone());
                    }
                }
                HirStmt::Block { statements, .. } => {
                    for stmt in statements {
                        collect_expected_roots(
                            body,
                            *stmt,
                            expected_return,
                            local_types,
                            expected_roots,
                        );
                    }
                }
                HirStmt::Let {
                    local,
                    initializer: Some(expr),
                    ..
                } => {
                    if expr.idx() < expected_roots.len() {
                        if let Some(ty) =
                            local_types.get(local.idx()).filter(|ty| !ty.is_errorish())
                        {
                            expected_roots[expr.idx()] = Some(ty.clone());
                        }
                    }
                }
                HirStmt::If {
                    then_branch,
                    else_branch,
                    ..
                } => {
                    collect_expected_roots(
                        body,
                        *then_branch,
                        expected_return,
                        local_types,
                        expected_roots,
                    );
                    if let Some(stmt) = else_branch {
                        collect_expected_roots(
                            body,
                            *stmt,
                            expected_return,
                            local_types,
                            expected_roots,
                        );
                    }
                }
                HirStmt::While { body: b, .. }
                | HirStmt::Switch { body: b, .. }
                | HirStmt::ForEach { body: b, .. }
                | HirStmt::Synchronized { body: b, .. } => {
                    collect_expected_roots(body, *b, expected_return, local_types, expected_roots);
                }
                HirStmt::For { init, body: b, .. } => {
                    for stmt in init {
                        collect_expected_roots(
                            body,
                            *stmt,
                            expected_return,
                            local_types,
                            expected_roots,
                        );
                    }
                    collect_expected_roots(body, *b, expected_return, local_types, expected_roots);
                }
                HirStmt::Try {
                    body: b,
                    catches,
                    finally,
                    ..
                } => {
                    collect_expected_roots(body, *b, expected_return, local_types, expected_roots);
                    for catch in catches {
                        collect_expected_roots(
                            body,
                            catch.body,
                            expected_return,
                            local_types,
                            expected_roots,
                        );
                    }
                    if let Some(stmt) = finally {
                        collect_expected_roots(
                            body,
                            *stmt,
                            expected_return,
                            local_types,
                            expected_roots,
                        );
                    }
                }
                HirStmt::Return { expr: None, .. }
                | HirStmt::Let { .. }
                | HirStmt::Expr { .. }
                | HirStmt::Yield { .. }
                | HirStmt::Assert { .. }
                | HirStmt::Throw { .. }
                | HirStmt::Break { .. }
                | HirStmt::Continue { .. }
                | HirStmt::Empty { .. } => {}
            }
        }

        let expected_return = checker.expected_return.clone();
        let local_types = &checker.local_types;
        collect_expected_roots(
            &body,
            body.root,
            &expected_return,
            local_types,
            &mut expected_roots,
        );

        fn find_enclosing_lambda(
            body: &HirBody,
            parent_expr: &[Option<HirExprId>],
            start: HirExprId,
        ) -> Option<HirExprId> {
            let mut current = start;
            while let Some(parent) = parent_expr.get(current.idx()).and_then(|p| *p) {
                if matches!(&body.exprs[parent], HirExpr::Lambda { .. }) {
                    return Some(parent);
                }
                current = parent;
            }
            None
        }

        fn is_descendant_of(
            mut expr: HirExprId,
            ancestor: HirExprId,
            parent_expr: &[Option<HirExprId>],
        ) -> bool {
            while let Some(parent) = parent_expr.get(expr.idx()).and_then(|p| *p) {
                if parent == ancestor {
                    return true;
                }
                expr = parent;
            }
            false
        }

        let enclosing_lambda = find_enclosing_lambda(&body, &parent_expr, call_site.expr);

        // Choose a root expression to infer:
        // - If this call is nested inside a return expr / typed initializer expr, infer that root
        //   with the appropriate expected type.
        // - Otherwise, if it's within the RHS of an assignment, infer the assignment expr so it can
        //   propagate the LHS type to the RHS.
        let mut root = call_site.expr;
        let mut expected: Option<Type> = None;
        let mut fallback_assignment: Option<HirExprId> = None;

        let mut current = call_site.expr;
        loop {
            if let Some(ty) = expected_roots.get(current.idx()).and_then(|t| t.clone()) {
                root = current;
                expected = Some(ty);
                break;
            }
            let Some(parent) = parent_expr.get(current.idx()).and_then(|p| *p) else {
                break;
            };

            if fallback_assignment.is_none() {
                if let HirExpr::Assign {
                    rhs,
                    op: AssignOp::Assign,
                    ..
                } = &body.exprs[parent]
                {
                    if *rhs == current {
                        fallback_assignment = Some(parent);
                    }
                }
            }

            current = parent;
        }

        if expected.is_none() {
            if let Some(assign) = fallback_assignment {
                root = assign;
            }
        }

        // If we're resolving a call inside a lambda body, prefer inferring an enclosing expression
        // that target-types the lambda so lambda parameter locals can be seeded from the SAM
        // signature.
        //
        // This matters when the call is inside a block-bodied lambda that is itself target-typed by
        // being passed as an argument (e.g. `use(s -> { return s.substring(1); })`) or when the
        // call is within a typed initializer inside the lambda block (`String t = s.substring(1);`)
        // and would otherwise become the chosen root before we reach the lambda.
        if let Some(lambda_expr) = enclosing_lambda {
            if is_descendant_of(root, lambda_expr, &parent_expr) {
                if let Some(ty) = expected_roots
                    .get(lambda_expr.idx())
                    .and_then(|t| t.clone())
                {
                    // The lambda itself is directly target-typed by a statement (typed initializer
                    // / return), so infer the lambda with that expected type.
                    root = lambda_expr;
                    expected = Some(ty);
                } else if let Some(parent) = parent_expr.get(lambda_expr.idx()).and_then(|p| *p) {
                    let mut candidate = None;
                    match &body.exprs[parent] {
                        HirExpr::Assign {
                            rhs,
                            op: AssignOp::Assign,
                            ..
                        } if *rhs == lambda_expr => {
                            // Assignment conversion will pass the LHS type as the expected type for
                            // the lambda RHS.
                            candidate = Some(parent);
                        }
                        HirExpr::Call { args, .. } if args.iter().any(|a| *a == lambda_expr) => {
                            // Overload resolution will pass the parameter type as the expected type
                            // for the lambda argument.
                            candidate = Some(parent);
                        }
                        _ => {}
                    }

                    if let Some(expr) = candidate {
                        root = expr;
                        expected = expected_roots.get(expr.idx()).and_then(|t| t.clone());
                    }
                }
            }
        }

        (root, expected)
    };

    let _ = checker.infer_expr_with_expected(&mut loader, root_expr, expected.as_ref());

    // `typeck_body` treats ambiguous calls as "best-effort" and still records the first candidate
    // for downstream type inference. For IDE features (signature help, hover, etc.) we instead
    // want to be resilient and avoid presenting an arbitrary choice, so ambiguous calls resolve to
    // `None`.
    let call_span = body.exprs[call_site.expr].range();
    if checker.diagnostics.iter().any(|d| {
        (d.code.as_ref() == "ambiguous-call" || d.code.as_ref() == "ambiguous-constructor")
            && d.span == Some(call_span)
    }) {
        db.record_query_stat("resolve_method_call_demand", start.elapsed());
        return None;
    }
    let resolved = checker
        .call_resolutions
        .get(call_site.expr.idx())
        .and_then(|m| m.clone());

    db.record_query_stat("resolve_method_call_demand", start.elapsed());
    resolved
}

fn type_diagnostics(db: &dyn NovaTypeck, file: FileId) -> Vec<Diagnostic> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "type_diagnostics", ?file).entered();

    cancel::check_cancelled(db);

    let file_text = db.file_content(file);
    let file_text = file_text.as_str();
    let file_tokens = lex(file_text);

    let tree = db.hir_item_tree(file);
    let mut diags = signature_type_diagnostics(db, file, &tree, &file_tokens, file_text);
    let owners = collect_body_owners(&tree);
    for (idx, owner) in owners.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 32);
        let body_diags: Vec<_> = db.typeck_body(*owner).diagnostics.iter().cloned().collect();
        extend_type_ref_diagnostics(&mut diags, &file_tokens, file_text, body_diags);
    }

    diags.sort_by_key(|d| {
        (
            d.span.map(|s| s.start).unwrap_or(usize::MAX),
            d.message.clone(),
        )
    });

    db.record_salsa_memo_bytes(
        file,
        TrackedSalsaMemo::TypeDiagnostics,
        super::estimated_diagnostics_bytes(&diags),
    );
    db.record_query_stat("type_diagnostics", start.elapsed());
    diags
}

fn signature_type_diagnostics(
    db: &dyn NovaTypeck,
    file: FileId,
    tree: &nova_hir::item_tree::ItemTree,
    file_tokens: &[Token],
    file_text: &str,
) -> Vec<Diagnostic> {
    cancel::check_cancelled(db);

    let project = db.file_project(file);
    let jdk = db.jdk_index(project);
    let classpath = db.classpath_index(project);
    let workspace = db.workspace_def_map(project);
    let jpms_env = db.jpms_compilation_env(project);

    let jpms_ctx = jpms_env.as_deref().map(|env| {
        let cfg = db.project_config(project);
        let file_rel = db.file_rel_path(file);
        let from = module_for_file(&cfg, file_rel.as_str());

        let index = JpmsProjectIndex {
            workspace: &workspace,
            graph: &env.env.graph,
            classpath: &env.classpath,
            jdk: &*jdk,
            from: from.clone(),
        };

        let provider = JpmsTypeProvider {
            graph: &env.env.graph,
            classpath: &env.classpath,
            jdk: &*jdk,
            from,
        };

        (index, provider)
    });

    let workspace_index = WorkspaceFirstIndex {
        workspace: &workspace,
        classpath: classpath.as_deref().map(|cp| cp as &dyn TypeIndex),
    };

    let resolver = if let Some((index, _)) = jpms_ctx.as_ref() {
        nova_resolve::Resolver::new(index)
    } else {
        nova_resolve::Resolver::new(&*jdk).with_classpath(&workspace_index)
    }
    .with_workspace(&workspace);

    let scopes = db.scope_graph(file);

    // Signature-only type resolution: build a minimal type environment and resolve type refs from
    // the `ItemTree` that are not checked as part of `typeck_body` (e.g. fields, abstract methods,
    // type declaration headers).
    let base_store = if jpms_ctx.is_some() {
        let cfg = db.project_config(project);
        let file_rel = db.file_rel_path(file);
        let from = module_for_file(&cfg, file_rel.as_str());
        db.project_base_type_store_for_module(project, from)
    } else {
        db.project_base_type_store(project)
    };
    let mut store = (&*base_store).clone();

    let chain_provider = match classpath.as_deref() {
        Some(cp) => nova_types::ChainTypeProvider::new(vec![
            cp as &dyn TypeProvider,
            &*jdk as &dyn TypeProvider,
        ]),
        None => nova_types::ChainTypeProvider::new(vec![&*jdk as &dyn TypeProvider]),
    };

    let provider: &dyn TypeProvider = if let Some((_, provider)) = jpms_ctx.as_ref() {
        provider
    } else {
        &chain_provider
    };
    let jdk_provider: &dyn TypeProvider = &*jdk;
    let java_only_provider = JavaOnlyJdkTypeProvider::new(provider, jdk_provider);
    let provider_for_loader: &dyn TypeProvider = if jpms_ctx.is_some() {
        provider
    } else {
        &java_only_provider
    };
    let shadowing_provider = WorkspaceShadowingTypeProvider::new(&workspace, provider_for_loader);
    let mut loader = ExternalTypeLoader::new(&mut store, &shadowing_provider);

    let object_ty = Type::class(loader.store.well_known().object, vec![]);

    let mut out = Vec::new();

    // JPMS module directives (`uses`/`provides`) can contain type names, but they are outside of any
    // class/method body so `typeck_body` will not visit them. Resolve them here so missing service
    // types still surface as anchored `unresolved-type` diagnostics.
    if let Some(module) = tree.module.as_ref() {
        let find_name_span =
            |outer: Span, needle: &str, start_at: usize| -> Option<(Span, usize)> {
                if needle.is_empty() {
                    return None;
                }
                let start = outer.start.min(file_text.len());
                let end = outer.end.min(file_text.len());
                if start >= end {
                    return None;
                }
                let slice = &file_text[start..end];
                if start_at >= slice.len() {
                    return None;
                }
                let idx = slice[start_at..].find(needle)?;
                let rel_start = start_at.saturating_add(idx);
                let abs_start = start.saturating_add(rel_start);
                let abs_end = abs_start.saturating_add(needle.len());
                if abs_end > file_text.len() {
                    return None;
                }
                Some((
                    Span::new(abs_start, abs_end),
                    rel_start.saturating_add(needle.len()),
                ))
            };

        let empty_vars: HashMap<String, TypeVarId> = HashMap::new();
        for directive in &module.directives {
            match directive {
                nova_hir::item_tree::ModuleDirective::Uses { service, range } => {
                    let base_span = find_name_span(*range, service, 0)
                        .map(|(span, _)| span)
                        .or(Some(*range));
                    let resolved = resolve_type_ref_text(
                        &resolver,
                        &scopes.scopes,
                        scopes.file_scope,
                        &mut loader,
                        &empty_vars,
                        service,
                        base_span,
                    );
                    out.extend(resolved.diagnostics);
                }
                nova_hir::item_tree::ModuleDirective::Provides {
                    service,
                    implementations,
                    range,
                } => {
                    let mut cursor = 0usize;

                    let service_span = find_name_span(*range, service, cursor)
                        .or_else(|| find_name_span(*range, service, 0))
                        .map(|(span, next)| {
                            cursor = next;
                            span
                        })
                        .or(Some(*range));
                    let resolved = resolve_type_ref_text(
                        &resolver,
                        &scopes.scopes,
                        scopes.file_scope,
                        &mut loader,
                        &empty_vars,
                        service,
                        service_span,
                    );
                    out.extend(resolved.diagnostics);

                    for impl_name in implementations {
                        let impl_span = find_name_span(*range, impl_name, cursor)
                            .or_else(|| find_name_span(*range, impl_name, 0))
                            .map(|(span, next)| {
                                cursor = next;
                                span
                            })
                            .or(Some(*range));
                        let resolved = resolve_type_ref_text(
                            &resolver,
                            &scopes.scopes,
                            scopes.file_scope,
                            &mut loader,
                            &empty_vars,
                            impl_name,
                            impl_span,
                        );
                        out.extend(resolved.diagnostics);
                    }
                }
                _ => {}
            }
        }
    }

    let mut steps: u32 = 0;
    for item in &tree.items {
        cancel::checkpoint_cancelled_every(db, steps, 32);
        steps = steps.wrapping_add(1);

        collect_signature_type_diagnostics_in_item(
            db,
            &resolver,
            &scopes,
            tree,
            *item,
            &mut loader,
            &object_ty,
            file_tokens,
            file_text,
            &mut out,
        );
    }

    out
}

fn collect_signature_type_diagnostics_in_item<'idx>(
    db: &dyn NovaTypeck,
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ItemTreeScopeBuildResult,
    tree: &nova_hir::item_tree::ItemTree,
    item: nova_hir::item_tree::Item,
    loader: &mut ExternalTypeLoader<'_>,
    default_bound: &Type,
    file_tokens: &[Token],
    file_text: &str,
    out: &mut Vec<Diagnostic>,
) {
    use nova_hir::ids::ItemId as HirItemId;
    use nova_hir::item_tree::AnnotationUse;
    use nova_hir::item_tree::Item as TreeItem;
    use nova_hir::item_tree::Member;

    cancel::check_cancelled(db);

    let item_id = match item {
        TreeItem::Class(id) => HirItemId::Class(id),
        TreeItem::Interface(id) => HirItemId::Interface(id),
        TreeItem::Enum(id) => HirItemId::Enum(id),
        TreeItem::Record(id) => HirItemId::Record(id),
        TreeItem::Annotation(id) => HirItemId::Annotation(id),
    };

    let class_scope = scopes
        .class_scopes
        .get(&item_id)
        .copied()
        .unwrap_or(scopes.file_scope);

    let type_params: &[nova_hir::item_tree::TypeParam] = match item_id {
        HirItemId::Class(id) => tree.class(id).type_params.as_slice(),
        HirItemId::Interface(id) => tree.interface(id).type_params.as_slice(),
        HirItemId::Record(id) => tree.record(id).type_params.as_slice(),
        HirItemId::Enum(_) | HirItemId::Annotation(_) => &[],
    };

    let mut class_vars = HashMap::new();
    alloc_type_param_ids(loader, default_bound, type_params, &mut class_vars);

    // Annotation uses on the type declaration.
    let item_annotations: &[AnnotationUse] = match item_id {
        HirItemId::Class(id) => tree.class(id).annotations.as_slice(),
        HirItemId::Interface(id) => tree.interface(id).annotations.as_slice(),
        HirItemId::Enum(id) => tree.enum_(id).annotations.as_slice(),
        HirItemId::Record(id) => tree.record(id).annotations.as_slice(),
        HirItemId::Annotation(id) => tree.annotation(id).annotations.as_slice(),
    };
    collect_annotation_use_diagnostics(
        resolver,
        &scopes.scopes,
        class_scope,
        loader,
        &class_vars,
        file_text,
        item_annotations,
        out,
    );

    // Type parameter bounds.
    for tp in type_params {
        for (idx, bound) in tp.bounds.iter().enumerate() {
            let base_span = tp.bounds_ranges.get(idx).copied();
            let resolved = resolve_type_ref_text(
                resolver,
                &scopes.scopes,
                class_scope,
                loader,
                &class_vars,
                bound,
                base_span,
            );
            extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
        }
    }

    // Type declaration header clauses (`extends`/`implements`/`permits`).
    match item_id {
        HirItemId::Class(id) => {
            let class = tree.class(id);
            for (idx, ext) in class.extends.iter().enumerate() {
                let base_span = class.extends_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    ext,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
            for (idx, imp) in class.implements.iter().enumerate() {
                let base_span = class.implements_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    imp,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
            for (idx, perm) in class.permits.iter().enumerate() {
                let base_span = class.permits_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    perm,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
        }
        HirItemId::Interface(id) => {
            let iface = tree.interface(id);
            for (idx, ext) in iface.extends.iter().enumerate() {
                let base_span = iface.extends_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    ext,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
            for (idx, perm) in iface.permits.iter().enumerate() {
                let base_span = iface.permits_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    perm,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
        }
        HirItemId::Enum(id) => {
            let enm = tree.enum_(id);
            for (idx, imp) in enm.implements.iter().enumerate() {
                let base_span = enm.implements_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    imp,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
            for (idx, perm) in enm.permits.iter().enumerate() {
                let base_span = enm.permits_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    perm,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
        }
        HirItemId::Record(id) => {
            let record = tree.record(id);
            for (idx, imp) in record.implements.iter().enumerate() {
                let base_span = record.implements_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    imp,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
            for (idx, perm) in record.permits.iter().enumerate() {
                let base_span = record.permits_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    perm,
                    base_span,
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
            }
        }
        HirItemId::Annotation(_) => {}
    }

    // Member types (fields + abstract method signatures).
    let mut steps: u32 = 0;
    for member in item_members(tree, item_id) {
        cancel::checkpoint_cancelled_every(db, steps, 32);
        steps = steps.wrapping_add(1);
        match *member {
            Member::Field(fid) => {
                let field = tree.field(fid);
                collect_annotation_use_diagnostics(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    file_text,
                    &field.annotations,
                    out,
                );
                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    &field.ty,
                    Some(field.ty_range),
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
                if resolved.ty == Type::Void {
                    let diag_span = if field.ty_range.is_empty() {
                        field.range
                    } else {
                        field.ty_range
                    };
                    out.push(Diagnostic::error(
                        "void-variable-type",
                        "`void` is not a valid type for variables",
                        Some(diag_span),
                    ));
                    out.push(Diagnostic::error(
                        "invalid-void-type",
                        "`void` is not a valid type for variables",
                        Some(diag_span),
                    ));
                }
            }
            Member::Method(mid) => {
                let method = tree.method(mid);
                collect_annotation_use_diagnostics(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    file_text,
                    &method.annotations,
                    out,
                );
                for param in &method.params {
                    collect_annotation_use_diagnostics(
                        resolver,
                        &scopes.scopes,
                        class_scope,
                        loader,
                        &class_vars,
                        file_text,
                        &param.annotations,
                        out,
                    );
                }
                if method.body.is_some() {
                    continue;
                }

                let scope = scopes
                    .method_scopes
                    .get(&mid)
                    .copied()
                    .unwrap_or(class_scope);

                let mut vars = class_vars.clone();
                alloc_type_param_ids(loader, default_bound, &method.type_params, &mut vars);

                for tp in &method.type_params {
                    for (idx, bound) in tp.bounds.iter().enumerate() {
                        let base_span = tp.bounds_ranges.get(idx).copied();
                        let resolved = resolve_type_ref_text(
                            resolver,
                            &scopes.scopes,
                            scope,
                            loader,
                            &vars,
                            bound,
                            base_span,
                        );
                        extend_type_ref_diagnostics(
                            out,
                            file_tokens,
                            file_text,
                            resolved.diagnostics,
                        );
                    }
                }

                let resolved = resolve_type_ref_text(
                    resolver,
                    &scopes.scopes,
                    scope,
                    loader,
                    &vars,
                    &method.return_ty,
                    Some(method.return_ty_range),
                );
                extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);

                for param in &method.params {
                    let resolved = resolve_type_ref_text(
                        resolver,
                        &scopes.scopes,
                        scope,
                        loader,
                        &vars,
                        &param.ty,
                        Some(param.ty_range),
                    );
                    extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
                    if resolved.ty == Type::Void {
                        out.push(Diagnostic::error(
                            "void-parameter-type",
                            "`void` is not a valid parameter type",
                            Some(param.ty_range),
                        ));
                        out.push(Diagnostic::error(
                            "invalid-void-type",
                            "`void` is not a valid parameter type",
                            Some(param.ty_range),
                        ));
                    }
                }

                for (idx, thrown) in method.throws.iter().enumerate() {
                    let base_span = method.throws_ranges.get(idx).copied();
                    let resolved = resolve_type_ref_text(
                        resolver,
                        &scopes.scopes,
                        scope,
                        loader,
                        &vars,
                        thrown,
                        base_span,
                    );
                    extend_type_ref_diagnostics(out, file_tokens, file_text, resolved.diagnostics);
                }
            }
            Member::Constructor(cid) => {
                let ctor = tree.constructor(cid);
                collect_annotation_use_diagnostics(
                    resolver,
                    &scopes.scopes,
                    class_scope,
                    loader,
                    &class_vars,
                    file_text,
                    &ctor.annotations,
                    out,
                );
                for param in &ctor.params {
                    collect_annotation_use_diagnostics(
                        resolver,
                        &scopes.scopes,
                        class_scope,
                        loader,
                        &class_vars,
                        file_text,
                        &param.annotations,
                        out,
                    );
                }
            }
            Member::Initializer(_) => {}
            Member::Type(child) => collect_signature_type_diagnostics_in_item(
                db,
                resolver,
                scopes,
                tree,
                child,
                loader,
                default_bound,
                file_tokens,
                file_text,
                out,
            ),
        }
    }
}

fn extend_type_ref_diagnostics(
    out: &mut Vec<Diagnostic>,
    file_tokens: &[Token],
    file_text: &str,
    diags: Vec<Diagnostic>,
) {
    // NOTE: Type-use annotations are currently ignored by Nova's type checker. The type-ref parser
    // is resilient to annotations (and can optionally diagnose them when anchored), but we
    // intentionally suppress diagnostics for annotation *type names* in type-use positions when
    // reporting type-check diagnostics.
    //
    // Example: `List<@Missing String>` should not surface an `unresolved-type` diagnostic for the
    // annotation name `Missing` in `db.type_diagnostics`.
    if diags.is_empty() {
        return;
    }

    out.extend(diags.into_iter().filter(|d| {
        let Some(span) = d.span else {
            return true;
        };
        if span.start == 0
            || span.start > file_text.len()
            || !file_text.is_char_boundary(span.start)
        {
            return true;
        }

        let start: u32 = match span.start.try_into() {
            Ok(v) => v,
            Err(_) => return true,
        };

        // Find the token that contains the diagnostic span start.
        let mut idx = file_tokens.partition_point(|tok| tok.range.end <= start);
        if idx >= file_tokens.len() {
            return true;
        }
        if file_tokens[idx].range.start > start && idx > 0 {
            idx -= 1;
        }

        // Walk backwards over trivia to find the previous token and confirm it is `@`.
        while idx > 0 {
            idx -= 1;
            let prev = &file_tokens[idx];
            if prev.kind.is_trivia() {
                continue;
            }
            return prev.kind != SyntaxKind::At;
        }

        true
    }));
}

fn collect_annotation_use_diagnostics<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    loader: &mut ExternalTypeLoader<'_>,
    type_vars: &HashMap<String, TypeVarId>,
    file_text: &str,
    annotations: &[nova_hir::item_tree::AnnotationUse],
    out: &mut Vec<Diagnostic>,
) {
    for ann in annotations {
        if ann.name.trim().is_empty() {
            continue;
        }
        let base_span = annotation_name_span(file_text, ann).or(Some(ann.range));
        let resolved = resolve_type_ref_text(
            resolver, scopes, scope_id, loader, type_vars, &ann.name, base_span,
        );
        out.extend(resolved.diagnostics);
    }
}

fn annotation_name_span(file_text: &str, ann: &nova_hir::item_tree::AnnotationUse) -> Option<Span> {
    if ann.name.is_empty() {
        return Some(ann.range);
    }
    let start = ann.range.start.min(file_text.len());
    let end = ann.range.end.min(file_text.len());
    if start >= end {
        return Some(ann.range);
    }
    let slice = &file_text[start..end];
    let idx = slice.find(&ann.name)?;
    let name_start = start.saturating_add(idx);
    let name_end = name_start.saturating_add(ann.name.len());
    if name_end > file_text.len() {
        return Some(ann.range);
    }
    Some(Span::new(name_start, name_end))
}

fn alloc_type_param_ids(
    loader: &mut ExternalTypeLoader<'_>,
    default_bound: &Type,
    type_params: &[nova_hir::item_tree::TypeParam],
    vars: &mut HashMap<String, TypeVarId>,
) {
    for tp in type_params {
        let id = loader
            .store
            .add_type_param(tp.name.clone(), vec![default_bound.clone()]);
        vars.insert(tp.name.clone(), id);
    }
}

fn type_at_offset_display(db: &dyn NovaTypeck, file: FileId, offset: u32) -> Option<String> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "type_at_offset_display", ?file, offset).entered();

    cancel::check_cancelled(db);

    let tree = db.hir_item_tree(file);
    let owners = collect_body_owners(&tree);

    let mut best: Option<(DefWithBodyId, HirExprId, usize)> = None;
    for (idx, owner) in owners.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 32);

        // Fast path: avoid loading HIR bodies for owners that cannot possibly contain the offset.
        //
        // The owner ranges come from the item tree and are cheap to access compared to lowering
        // bodies. Keep this best-effort by only pruning when spans are non-empty (parse recovery can
        // yield degenerate ranges).
        let owner_range = match *owner {
            DefWithBodyId::Method(m) => tree.method(m).range,
            DefWithBodyId::Constructor(c) => tree.constructor(c).range,
            DefWithBodyId::Initializer(i) => tree.initializer(i).range,
        };
        let offset_usize = offset as usize;
        if !owner_range.is_empty()
            && !(owner_range.start <= offset_usize && offset_usize < owner_range.end)
        {
            continue;
        }

        let body = match *owner {
            DefWithBodyId::Method(m) => db.hir_body(m),
            DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
            DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
        };

        find_best_expr_in_stmt(&body, body.root, offset_usize, *owner, &mut best);
    }

    let (owner, expr, _) = best?;
    let expr_res = db.type_of_expr_demand_result(file, FileExprId { owner, expr });
    let rendered = format_type(&*expr_res.env, &expr_res.ty);

    db.record_query_stat("type_at_offset_display", start.elapsed());
    Some(rendered)
}

fn expr_scopes(db: &dyn NovaTypeck, owner: DefWithBodyId) -> ArcEq<ExprScopes> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "expr_scopes", ?owner).entered();

    cancel::check_cancelled(db);

    let file = def_file(owner);
    let tree = db.hir_item_tree(file);
    let body = match owner {
        DefWithBodyId::Method(m) => db.hir_body(m),
        DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
        DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
    };

    let param_ids = params_for_owner(&tree, owner);
    let scopes = ExprScopes::new(&body, &param_ids, |id| param_name_lookup(&tree, id));

    let approx_bytes = scopes.estimated_bytes();
    let result = ArcEq::new(Arc::new(scopes));
    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::ExprScopes, approx_bytes);
    db.record_query_stat("expr_scopes", start.elapsed());
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectBaseTypeStoreData {
    store: ArcEq<TypeStore>,
    source_types: ArcEq<SourceTypes>,
}

impl ProjectBaseTypeStoreData {
    #[must_use]
    pub fn store(&self) -> ArcEq<TypeStore> {
        self.store.clone()
    }

    #[must_use]
    pub fn source_types(&self) -> ArcEq<SourceTypes> {
        self.source_types.clone()
    }
}

enum SourceTypesHandle {
    Owned(SourceTypes),
    Cached(ArcEq<SourceTypes>),
}

impl SourceTypesHandle {
    fn as_ref(&self) -> &SourceTypes {
        match self {
            SourceTypesHandle::Owned(types) => types,
            SourceTypesHandle::Cached(types) => types,
        }
    }
}

fn project_source_types(db: &dyn NovaTypeck, project: ProjectId) -> ArcEq<SourceTypes> {
    db.project_base_type_store_data(project)
        .source_types
        .clone()
}

fn project_base_type_store_data(
    db: &dyn NovaTypeck,
    project: ProjectId,
) -> ProjectBaseTypeStoreData {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "project_base_type_store", ?project).entered();

    cancel::check_cancelled(db);

    let cfg = db.project_config(project);
    let jdk = db.jdk_index(project);
    let classpath = db.classpath_index(project);
    let workspace = db.workspace_def_map(project);
    let jpms_env = db.jpms_compilation_env(project);

    // Stable, project-relative file order.
    let mut files: Vec<(Arc<String>, FileId)> = db
        .project_files(project)
        .iter()
        .map(|&file| (db.file_rel_path(file), file))
        .collect();
    files.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    let files: Vec<FileId> = files.into_iter().map(|(_, file)| file).collect();

    // Start with the built-in minimal JDK so type-system algorithms have a stable core (`Object`,
    // `String`, boxing types, etc).
    let mut store = TypeStore::with_minimal_jdk();
    // 1) Pre-intern all workspace source types in a stable order so their `ClassId`s do not depend
    // on which body/file happens to be typechecked first.
    for (idx, name) in workspace.iter_type_names().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 4096);
        store.intern_class_id(name.as_str());
    }

    // 2) Pre-intern referenced types (from signatures and bodies) so that subsequent per-body
    // loading does not allocate `ClassId`s in a body-dependent order.
    let mut referenced_type_names: Vec<String> = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 16);

        let scopes = db.scope_graph(*file);
        let tree = db.hir_item_tree(*file);

        // Build a resolver for this file; JPMS projects require per-file module context.
        let jpms_index = jpms_env.as_deref().map(|env| {
            let file_rel = db.file_rel_path(*file);
            let from = module_for_file(&cfg, file_rel.as_str());
            JpmsProjectIndex {
                workspace: &workspace,
                graph: &env.env.graph,
                classpath: &env.classpath,
                jdk: &*jdk,
                from,
            }
        });
        let workspace_index = WorkspaceFirstIndex {
            workspace: &workspace,
            classpath: classpath.as_deref().map(|cp| cp as &dyn TypeIndex),
        };

        let resolver = if let Some(index) = jpms_index.as_ref() {
            nova_resolve::Resolver::new(index).with_workspace(&workspace)
        } else {
            nova_resolve::Resolver::new(&*jdk)
                .with_classpath(&workspace_index)
                .with_workspace(&workspace)
        };

        let mut item_ids = Vec::new();
        for item in &tree.items {
            collect_item_ids(&tree, *item, &mut item_ids);
        }

        // Import statements can introduce types (including static-import owner types) that are only
        // referenced from expression position (e.g. `Map.of(...)`, `Entry.comparingByKey()`,
        // `max(1, 2)` after `import static java.lang.Math.max;`). Pre-intern those types so their
        // `ClassId`s remain stable across per-body `TypeStore` clones.
        for import in &tree.imports {
            let raw = import.path.trim();
            if raw.is_empty() {
                continue;
            }

            // For `import static X.Y;` treat `X` as the type owner and ignore the member segment.
            // For star imports, the stored `path` already excludes `.*`.
            let candidate = if import.is_static && !import.is_star {
                raw.rsplit_once('.').map(|(ty, _)| ty).unwrap_or(raw)
            } else {
                raw
            };

            collect_resolved_type_names(
                &resolver,
                &scopes.scopes,
                scopes.file_scope,
                candidate,
                &mut referenced_type_names,
            );
        }

        for item in item_ids.iter().copied() {
            let class_scope = scopes
                .class_scopes
                .get(&item)
                .copied()
                .unwrap_or(scopes.file_scope);

            for member in item_members(&tree, item) {
                match member {
                    nova_hir::item_tree::Member::Field(fid) => {
                        let field = tree.field(*fid);
                        collect_resolved_type_names(
                            &resolver,
                            &scopes.scopes,
                            class_scope,
                            &field.ty,
                            &mut referenced_type_names,
                        );
                    }
                    nova_hir::item_tree::Member::Method(mid) => {
                        let method = tree.method(*mid);
                        let scope = scopes
                            .method_scopes
                            .get(mid)
                            .copied()
                            .unwrap_or(class_scope);
                        collect_resolved_type_names(
                            &resolver,
                            &scopes.scopes,
                            scope,
                            &method.return_ty,
                            &mut referenced_type_names,
                        );
                        for p in &method.params {
                            collect_resolved_type_names(
                                &resolver,
                                &scopes.scopes,
                                scope,
                                &p.ty,
                                &mut referenced_type_names,
                            );
                        }
                    }
                    nova_hir::item_tree::Member::Constructor(cid) => {
                        let ctor = tree.constructor(*cid);
                        let scope = scopes
                            .constructor_scopes
                            .get(cid)
                            .copied()
                            .unwrap_or(class_scope);
                        for p in &ctor.params {
                            collect_resolved_type_names(
                                &resolver,
                                &scopes.scopes,
                                scope,
                                &p.ty,
                                &mut referenced_type_names,
                            );
                        }
                    }
                    nova_hir::item_tree::Member::Initializer(_)
                    | nova_hir::item_tree::Member::Type(_) => {}
                }
            }
        }

        // Best-effort: scan body locals and `new` expressions for type names so external types used
        // only in method bodies still receive stable `ClassId`s.
        let owners = collect_body_owners(&tree);
        for (owner_idx, owner) in owners.iter().enumerate() {
            cancel::checkpoint_cancelled_every(db, owner_idx as u32, 32);

            let body_scope = match *owner {
                DefWithBodyId::Method(m) => scopes.method_scopes.get(&m).copied(),
                DefWithBodyId::Constructor(c) => scopes.constructor_scopes.get(&c).copied(),
                DefWithBodyId::Initializer(i) => scopes.initializer_scopes.get(&i).copied(),
            }
            .unwrap_or(scopes.file_scope);

            let body = match *owner {
                DefWithBodyId::Method(m) => db.hir_body(m),
                DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
                DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
            };

            for (_, local) in body.locals.iter() {
                collect_resolved_type_names(
                    &resolver,
                    &scopes.scopes,
                    body_scope,
                    &local.ty_text,
                    &mut referenced_type_names,
                );
            }

            for (_, expr) in body.exprs.iter() {
                if let HirExpr::New { class, .. } = expr {
                    collect_resolved_type_names(
                        &resolver,
                        &scopes.scopes,
                        body_scope,
                        class,
                        &mut referenced_type_names,
                    );
                }
            }

            // Best-effort: scan expressions for qualified type names used in expression position
            // (primarily static member receivers and nested types, e.g. `Map.Entry`).
            //
            // Use a conservative heuristic (first segment starts with an ASCII uppercase letter) to
            // avoid doing resolver work for the vast majority of value expressions.
            for (_, expr) in body.exprs.iter() {
                let candidate = match expr {
                    HirExpr::Name { name, .. } => Some(name.as_str().to_string()),
                    HirExpr::FieldAccess { receiver, name, .. } => {
                        expr_qualified_name_from_field_access(&body, *receiver, name)
                    }
                    _ => None,
                };

                let Some(candidate) = candidate else {
                    continue;
                };
                if !candidate
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_uppercase())
                {
                    continue;
                }

                collect_resolved_type_names(
                    &resolver,
                    &scopes.scopes,
                    body_scope,
                    &candidate,
                    &mut referenced_type_names,
                );
            }
        }
    }

    referenced_type_names.sort();
    referenced_type_names.dedup();
    for (idx, name) in referenced_type_names.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 128);
        store.intern_class_id(name);
    }

    // 3) Pre-intern external types in deterministic order so `ClassId`s are stable across
    // body-local clones even when external types are loaded in different orders.
    //
    // In JPMS mode, do **not** use the legacy `classpath_index` input (workspace loading
    // historically merged module-path jars into it); instead, use the JPMS compilation
    // environment's module-aware classpath index.
    //
    // Also mirror the resolver's `java.*` handling: application class loaders cannot define
    // `java.*` packages, so classpath/module-path stubs should not be able to "rescue" unresolved
    // `java.*` references.
    if let Some(env) = jpms_env.as_deref() {
        for (idx, name) in env.classpath.types.iter_binary_names().enumerate() {
            cancel::checkpoint_cancelled_every(db, idx as u32, 4096);
            if name.starts_with("java.") {
                continue;
            }
            store.intern_class_id(name);
        }
    } else if let Some(cp) = classpath.as_deref() {
        for (idx, name) in cp.iter_binary_names().enumerate() {
            cancel::checkpoint_cancelled_every(db, idx as u32, 4096);
            if name.starts_with("java.") {
                continue;
            }
            store.intern_class_id(name);
        }
    }

    // NOTE: We currently do **not** pre-intern all JDK binary names here.
    //
    // While it would provide fully stable `ClassId` allocation for arbitrary
    // standard-library types, a real JDK contains tens of thousands of classes.
    // Pre-interning all of them would:
    // - increase the cost of building this base store, and
    // - (more importantly) bloat every cloned body-local `TypeStore`, since
    //   `typeck_body` needs a mutable `TypeStore` for on-demand loading.
    //
    // The current approach relies on `TypeStore::with_minimal_jdk()` for a small
    // but semantically useful set of core types, and loads other JDK types on
    // demand.

    // 4) Define all source types in the store so cross-file references can observe them via
    // `Type::Class` and member resolution can consult their (best-effort) members.
    // Mirror `typeck_body`'s external stub loading policies:
    // - never load `java.*` from the classpath (JDK wins)
    // - enforce JPMS readability + exports in JPMS mode
    // - prevent external stubs from shadowing workspace source types (even via recursive loads)
    let chain_provider = match classpath.as_deref() {
        Some(cp) => nova_types::ChainTypeProvider::new(vec![
            cp as &dyn TypeProvider,
            &*jdk as &dyn TypeProvider,
        ]),
        None => nova_types::ChainTypeProvider::new(vec![&*jdk as &dyn TypeProvider]),
    };

    let jdk_provider: &dyn TypeProvider = &*jdk;
    let java_only_provider = JavaOnlyJdkTypeProvider::new(&chain_provider, jdk_provider);

    let mut source_types = SourceTypes::default();

    if let Some(env) = jpms_env.as_deref() {
        for (idx, file) in files.iter().enumerate() {
            cancel::checkpoint_cancelled_every(db, idx as u32, 16);

            let scopes = db.scope_graph(*file);
            let tree = db.hir_item_tree(*file);

            let file_rel = db.file_rel_path(*file);
            let from = module_for_file(&cfg, file_rel.as_str());

            let jpms_index = JpmsProjectIndex {
                workspace: &workspace,
                graph: &env.env.graph,
                classpath: &env.classpath,
                jdk: &*jdk,
                from: from.clone(),
            };
            let resolver = nova_resolve::Resolver::new(&jpms_index).with_workspace(&workspace);

            let jpms_provider = JpmsTypeProvider {
                graph: &env.env.graph,
                classpath: &env.classpath,
                jdk: &*jdk,
                from,
            };
            let shadowing_provider =
                WorkspaceShadowingTypeProvider::new(&workspace, &jpms_provider);
            let mut loader = ExternalTypeLoader::new(&mut store, &shadowing_provider);

            // In JPMS mode, source type accessibility depends on the "from" module, so per-body
            // queries still build workspace member/type-param maps on demand.
            let _ = define_source_types(&resolver, &scopes, &tree, &mut loader);
        }
    } else {
        let shadowing_provider =
            WorkspaceShadowingTypeProvider::new(&workspace, &java_only_provider);
        let mut loader = ExternalTypeLoader::new(&mut store, &shadowing_provider);

        for (idx, file) in files.iter().enumerate() {
            cancel::checkpoint_cancelled_every(db, idx as u32, 16);

            let scopes = db.scope_graph(*file);
            let tree = db.hir_item_tree(*file);

            let workspace_index = WorkspaceFirstIndex {
                workspace: &workspace,
                classpath: classpath.as_deref().map(|cp| cp as &dyn TypeIndex),
            };

            let resolver = nova_resolve::Resolver::new(&*jdk)
                .with_classpath(&workspace_index)
                .with_workspace(&workspace);

            source_types.extend(define_source_types(&resolver, &scopes, &tree, &mut loader));
        }

        drop(loader);
    }

    let store_bytes = store.estimated_bytes();
    let source_types_bytes = source_types.estimated_bytes();
    db.record_salsa_project_memo_bytes(
        project,
        TrackedSalsaProjectMemo::ProjectBaseTypeStore,
        store_bytes.saturating_add(source_types_bytes),
    );
    db.record_query_stat("project_base_type_store", start.elapsed());

    ProjectBaseTypeStoreData {
        store: ArcEq::new(Arc::new(store)),
        source_types: ArcEq::new(Arc::new(source_types)),
    }
}

fn project_base_type_store(db: &dyn NovaTypeck, project: ProjectId) -> ArcEq<TypeStore> {
    db.project_base_type_store_data(project).store.clone()
}

fn project_base_type_store_for_module(
    db: &dyn NovaTypeck,
    project: ProjectId,
    from: ModuleName,
) -> ArcEq<TypeStore> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!(
        "query",
        name = "project_base_type_store_for_module",
        ?project,
        from = %from
    )
    .entered();

    cancel::check_cancelled(db);

    // Start with the built-in minimal JDK so type-system algorithms have
    // a stable core (`Object`, `String`, boxing types, etc).
    let mut store = TypeStore::with_minimal_jdk();

    let jpms_env = db.jpms_compilation_env(project);
    let Some(env) = jpms_env.as_deref() else {
        // If JPMS env construction fails, fall back to legacy behavior.
        if let Some(cp) = db.classpath_index(project).as_deref() {
            for (idx, name) in cp.binary_names_sorted().iter().enumerate() {
                cancel::checkpoint_cancelled_every(db, idx as u32, 4096);
                if name.starts_with("java.") {
                    continue;
                }
                store.intern_class_id(name);
            }
        }

        db.record_salsa_project_module_memo_bytes(
            project,
            from,
            TrackedSalsaProjectModuleMemo::ProjectBaseTypeStoreForModule,
            store.estimated_bytes(),
        );
        db.record_query_stat("project_base_type_store_for_module", start.elapsed());
        return ArcEq::new(Arc::new(store));
    };

    // In JPMS mode, pre-intern *only* types that are actually accessible from `from`.
    //
    // This avoids `nova_resolve::type_ref`'s `TypeEnv::lookup_class` fallback resolving a type that
    // JPMS would otherwise forbid (which would skip `unresolved-type` diagnostics).
    let unnamed = ModuleName::unnamed();
    let graph = &env.env.graph;
    for (idx, name) in env.classpath.types.binary_names_sorted().iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 4096);
        if name.starts_with("java.") {
            continue;
        }

        let to = env.classpath.module_of(name).unwrap_or(&unnamed);
        if !graph.can_read(&from, to) {
            continue;
        }

        let package = name.rsplit_once('.').map(|(pkg, _)| pkg).unwrap_or("");
        if let Some(info) = graph.get(to) {
            if !info.exports_package_to(package, &from) {
                continue;
            }
        }

        store.intern_class_id(name);
    }

    db.record_salsa_project_module_memo_bytes(
        project,
        from,
        TrackedSalsaProjectModuleMemo::ProjectBaseTypeStoreForModule,
        store.estimated_bytes(),
    );
    db.record_query_stat("project_base_type_store_for_module", start.elapsed());
    ArcEq::new(Arc::new(store))
}

fn typeck_body(db: &dyn NovaTypeck, owner: DefWithBodyId) -> Arc<BodyTypeckResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "typeck_body", ?owner).entered();

    cancel::check_cancelled(db);

    let file = def_file(owner);
    let java_level = db.java_language_level(file);
    let file_text_arc = db.file_content(file);
    let file_text = file_text_arc.as_str();
    let file_tokens = lex(file_text);
    let project = db.file_project(file);
    let jdk = db.jdk_index(project);
    let classpath = db.classpath_index(project);
    let workspace = db.workspace_def_map(project);
    let jpms_env = db.jpms_compilation_env(project);

    // JPMS-aware resolver + provider (when available).
    //
    // We keep the backing values (`jpms_ctx`, `workspace_index`, `chain_provider`) alive in this
    // scope so we can hand out references (`&dyn TypeIndex` / `&dyn TypeProvider`) that stay valid
    // for the rest of `typeck_body`.
    let jpms_ctx = jpms_env.as_deref().map(|env| {
        let cfg = db.project_config(project);
        let file_rel = db.file_rel_path(file);
        let from = module_for_file(&cfg, file_rel.as_str());

        let index = JpmsProjectIndex {
            workspace: &workspace,
            graph: &env.env.graph,
            classpath: &env.classpath,
            jdk: &*jdk,
            from: from.clone(),
        };

        let provider = JpmsTypeProvider {
            graph: &env.env.graph,
            classpath: &env.classpath,
            jdk: &*jdk,
            from,
        };

        (index, provider)
    });

    let workspace_index = WorkspaceFirstIndex {
        workspace: &workspace,
        classpath: classpath.as_deref().map(|cp| cp as &dyn TypeIndex),
    };

    let resolver = if let Some((index, _)) = jpms_ctx.as_ref() {
        nova_resolve::Resolver::new(index)
    } else {
        nova_resolve::Resolver::new(&*jdk).with_classpath(&workspace_index)
    }
    .with_workspace(&workspace);

    let chain_provider = match classpath.as_deref() {
        Some(cp) => nova_types::ChainTypeProvider::new(vec![
            cp as &dyn TypeProvider,
            &*jdk as &dyn TypeProvider,
        ]),
        None => nova_types::ChainTypeProvider::new(vec![&*jdk as &dyn TypeProvider]),
    };

    let provider: &dyn TypeProvider = if let Some((_, provider)) = jpms_ctx.as_ref() {
        provider
    } else {
        &chain_provider
    };

    let scopes = db.scope_graph(file);
    let body_scope = match owner {
        DefWithBodyId::Method(m) => scopes.method_scopes.get(&m).copied(),
        DefWithBodyId::Constructor(c) => scopes.constructor_scopes.get(&c).copied(),
        DefWithBodyId::Initializer(i) => scopes.initializer_scopes.get(&i).copied(),
    }
    .unwrap_or(scopes.file_scope);

    let tree = db.hir_item_tree(file);
    let body = match owner {
        DefWithBodyId::Method(m) => db.hir_body(m),
        DefWithBodyId::Constructor(c) => db.hir_constructor_body(c),
        DefWithBodyId::Initializer(i) => db.hir_initializer_body(i),
    };

    let expr_scopes = db.expr_scopes(owner);

    // Build an env for this body.
    let base_store = if jpms_ctx.is_some() {
        let cfg = db.project_config(project);
        let file_rel = db.file_rel_path(file);
        let from = module_for_file(&cfg, file_rel.as_str());
        db.project_base_type_store_for_module(project, from)
    } else {
        db.project_base_type_store(project)
    };
    let mut store = (&*base_store).clone();

    // Prevent classpath/module-path stubs from shadowing `java.*`.
    //
    // In JPMS mode we keep using the JPMS-aware provider (so `java.sql.*`/etc is still subject to
    // readability + exports checks), but for non-JPMS we wrap the legacy provider chain so `java.*`
    // loads exclusively from the JDK.
    let jdk_provider: &dyn TypeProvider = &*jdk;
    let java_only_provider = JavaOnlyJdkTypeProvider::new(provider, jdk_provider);
    let provider_for_loader: &dyn TypeProvider = if jpms_ctx.is_some() {
        provider
    } else {
        &java_only_provider
    };
    let shadowing_provider = WorkspaceShadowingTypeProvider::new(&workspace, provider_for_loader);
    let mut loader = ExternalTypeLoader::new(&mut store, &shadowing_provider);

    // Source typing metadata:
    // - In JPMS mode, type accessibility depends on the "from" module, so we must build the
    //   workspace view per body.
    // - In non-JPMS mode, reuse the project-level cached `SourceTypes` computed alongside
    //   `project_base_type_store`.
    let source_types_handle = if jpms_ctx.is_some() {
        SourceTypesHandle::Owned(define_workspace_source_types(
            db,
            project,
            file,
            &resolver,
            &mut loader,
        ))
    } else {
        SourceTypesHandle::Cached(db.project_source_types(project))
    };
    let source_types = source_types_handle.as_ref();
    let type_vars = type_vars_for_owner(
        &resolver,
        owner,
        body_scope,
        &scopes.scopes,
        &tree,
        &mut loader,
        &source_types.source_type_vars,
    );
    let field_types = &source_types.field_types;
    let field_owners = &source_types.field_owners;
    let method_owners = &source_types.method_owners;

    let (expected_return, signature_diags) = resolve_expected_return_type(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &type_vars,
        &mut loader,
    );
    let (param_types, param_diags) = resolve_param_types(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &type_vars,
        &mut loader,
    );
    let type_param_bound_diags = resolve_owner_type_param_bounds(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &type_vars,
        &mut loader,
    );
    let throws_diags = resolve_owner_throws_clause_types(
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        owner,
        &type_vars,
        &mut loader,
    );

    let mut checker = BodyChecker::new(
        db,
        file_text,
        &file_tokens,
        owner,
        &resolver,
        &scopes.scopes,
        body_scope,
        &tree,
        &body,
        expr_scopes,
        type_vars,
        expected_return.clone(),
        param_types,
        field_types,
        field_owners,
        method_owners,
        java_level,
        false,
    );
    extend_type_ref_diagnostics(
        &mut checker.diagnostics,
        &file_tokens,
        file_text,
        signature_diags,
    );
    extend_type_ref_diagnostics(
        &mut checker.diagnostics,
        &file_tokens,
        file_text,
        param_diags,
    );
    extend_type_ref_diagnostics(
        &mut checker.diagnostics,
        &file_tokens,
        file_text,
        type_param_bound_diags,
    );
    extend_type_ref_diagnostics(
        &mut checker.diagnostics,
        &file_tokens,
        file_text,
        throws_diags,
    );

    checker.check_body(&mut loader);

    // Finalize expression type table.
    let mut expr_types = Vec::with_capacity(body.exprs.len());
    for idx in 0..body.exprs.len() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 128);
        if let Some(info) = checker.expr_info.get(idx).and_then(|i| i.clone()) {
            expr_types.push(info.ty);
        } else {
            // If an expr was unreachable due to parse recovery, still provide a stable entry.
            expr_types.push(Type::Unknown);
        }
    }

    let call_resolutions = checker.call_resolutions;
    let diagnostics = checker.diagnostics;

    drop(loader);
    let store_bytes = store.estimated_bytes();
    let expr_types_bytes = (expr_types.capacity() * std::mem::size_of::<Type>()) as u64;
    let call_resolutions_bytes =
        (call_resolutions.capacity() * std::mem::size_of::<Option<ResolvedMethod>>()) as u64;
    let diagnostics_bytes = (diagnostics.capacity() * std::mem::size_of::<Diagnostic>()) as u64
        + diagnostics
            .iter()
            .map(|diag| diag.message.len() as u64)
            .sum::<u64>();
    let approx_bytes = store_bytes
        .saturating_add(expr_types_bytes)
        .saturating_add(call_resolutions_bytes)
        .saturating_add(diagnostics_bytes);
    let env = ArcEq::new(Arc::new(store));

    let result = Arc::new(BodyTypeckResult {
        env,
        expr_types,
        call_resolutions,
        diagnostics,
        expected_return,
    });

    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::TypeckBody, approx_bytes);
    db.record_query_stat("typeck_body", start.elapsed());
    result
}

#[derive(Debug, Clone)]
struct ExprInfo {
    ty: Type,
    is_type_ref: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalTyState {
    Uncomputed,
    Computing,
    Computed,
}

fn is_placeholder_class_def(def: &ClassDef) -> bool {
    def.kind == ClassKind::Class
        && def.name != "java.lang.Object"
        && def.super_class.is_none()
        && def.type_params.is_empty()
        && def.interfaces.is_empty()
        && def.fields.is_empty()
        && def.constructors.is_empty()
        && def.methods.is_empty()
}

struct BodyChecker<'a, 'idx> {
    db: &'a dyn NovaTypeck,
    file_text: &'a str,
    file_tokens: &'a [Token],
    owner: DefWithBodyId,
    resolver: &'a nova_resolve::Resolver<'idx>,
    scopes: &'a nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    tree: &'a nova_hir::item_tree::ItemTree,
    body: &'a HirBody,
    expr_scopes: ArcEq<ExprScopes>,
    type_vars: HashMap<String, TypeVarId>,
    expected_return: Type,
    current_expected_return: Type,
    local_types: Vec<Type>,
    local_ty_states: Vec<LocalTyState>,
    local_is_catch_param: Vec<bool>,
    local_initializers: Vec<Option<HirExprId>>,
    local_is_let_decl: Vec<bool>,
    local_foreach_iterables: Vec<Option<HirExprId>>,
    lazy_locals: bool,
    param_types: Vec<Type>,
    field_types: &'a HashMap<FieldId, Type>,
    field_owners: &'a HashMap<FieldId, String>,
    method_owners: &'a HashMap<MethodId, String>,
    expr_info: Vec<Option<ExprInfo>>,
    call_resolutions: Vec<Option<ResolvedMethod>>,
    diagnostics: Vec<Diagnostic>,
    switch_yield_stack: Vec<Vec<Type>>,
    workspace_in_progress: HashSet<String>,
    workspace_loaded: HashSet<String>,
    java_level: JavaLanguageLevel,
    steps: u32,
}

struct RestoreTypeOnDrop {
    slot: *mut Type,
    prev: Option<Type>,
}

impl Drop for RestoreTypeOnDrop {
    fn drop(&mut self) {
        let Some(prev) = self.prev.take() else {
            return;
        };
        // SAFETY: `slot` is created from a mutable reference to a `BodyChecker` field and the
        // guard does not outlive that borrow. We only write back the previous value on drop.
        unsafe {
            *self.slot = prev;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExplicitConstructorInvocationKind {
    This,
    Super,
}

impl ExplicitConstructorInvocationKind {
    fn as_str(self) -> &'static str {
        match self {
            ExplicitConstructorInvocationKind::This => "this",
            ExplicitConstructorInvocationKind::Super => "super",
        }
    }
}

impl<'a, 'idx> BodyChecker<'a, 'idx> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        db: &'a dyn NovaTypeck,
        file_text: &'a str,
        file_tokens: &'a [Token],
        owner: DefWithBodyId,
        resolver: &'a nova_resolve::Resolver<'idx>,
        scopes: &'a nova_resolve::ScopeGraph,
        scope_id: nova_resolve::ScopeId,
        tree: &'a nova_hir::item_tree::ItemTree,
        body: &'a HirBody,
        expr_scopes: ArcEq<ExprScopes>,
        type_vars: HashMap<String, TypeVarId>,
        expected_return: Type,
        param_types: Vec<Type>,
        field_types: &'a HashMap<FieldId, Type>,
        field_owners: &'a HashMap<FieldId, String>,
        method_owners: &'a HashMap<MethodId, String>,
        java_level: JavaLanguageLevel,
        lazy_locals: bool,
    ) -> Self {
        let local_types = vec![Type::Unknown; body.locals.len()];
        let local_ty_states = vec![LocalTyState::Uncomputed; body.locals.len()];
        let mut local_is_catch_param = vec![false; body.locals.len()];
        let mut initializers = vec![None; body.locals.len()];
        let mut is_let_decl = vec![false; body.locals.len()];
        let mut foreach_iterables = vec![None; body.locals.len()];
        for (_, stmt) in body.stmts.iter() {
            match stmt {
                HirStmt::Let {
                    local, initializer, ..
                } => {
                    if lazy_locals {
                        is_let_decl[local.idx()] = true;
                        initializers[local.idx()] = *initializer;
                    }
                }
                HirStmt::ForEach {
                    local, iterable, ..
                } => {
                    if lazy_locals {
                        foreach_iterables[local.idx()] = Some(*iterable);
                    }
                }
                HirStmt::Try { catches, .. } => {
                    for catch in catches {
                        let idx = catch.param.idx();
                        if idx < local_is_catch_param.len() {
                            local_is_catch_param[idx] = true;
                        }
                    }
                }
                _ => {}
            }
        }

        let (local_initializers, local_is_let_decl, local_foreach_iterables) = if lazy_locals {
            (initializers, is_let_decl, foreach_iterables)
        } else {
            (
                vec![None; body.locals.len()],
                vec![false; body.locals.len()],
                vec![None; body.locals.len()],
            )
        };
        let expr_info = vec![None; body.exprs.len()];
        let call_resolutions = vec![None; body.exprs.len()];

        Self {
            db,
            file_text,
            file_tokens,
            owner,
            resolver,
            scopes,
            scope_id,
            tree,
            body,
            expr_scopes,
            type_vars,
            current_expected_return: expected_return.clone(),
            expected_return,
            local_types,
            local_ty_states,
            local_is_catch_param,
            local_initializers,
            local_is_let_decl,
            local_foreach_iterables,
            lazy_locals,
            param_types,
            field_types,
            field_owners,
            method_owners,
            expr_info,
            call_resolutions,
            diagnostics: Vec::new(),
            switch_yield_stack: Vec::new(),
            java_level,
            workspace_in_progress: HashSet::new(),
            workspace_loaded: HashSet::new(),
            steps: 0,
        }
    }

    fn check_body(&mut self, loader: &mut ExternalTypeLoader<'_>) {
        if matches!(self.owner, DefWithBodyId::Constructor(_)) {
            self.check_explicit_constructor_invocation_placement();
        }

        let expected_return = self.expected_return.clone();
        self.check_stmt(loader, self.body.root, &expected_return);
    }

    /// Best-effort enforcement that `this(...);` / `super(...);` is the first statement in a
    /// constructor body.
    fn check_explicit_constructor_invocation_placement(&mut self) {
        let DefWithBodyId::Constructor(_) = self.owner else {
            return;
        };

        let allowed = if self.is_explicit_constructor_invocation_stmt(self.body.root) {
            Some(self.body.root)
        } else if let HirStmt::Block { statements, .. } = &self.body.stmts[self.body.root] {
            statements
                .first()
                .copied()
                .filter(|stmt| self.is_explicit_constructor_invocation_stmt(*stmt))
        } else {
            None
        };

        let mut stack = vec![self.body.root];
        // Lambda bodies are represented as statement blocks stored in `body.stmts` but are not
        // reachable from the root statement tree. Explicit ctor invocations inside lambda bodies
        // are still invalid, so include them in the traversal.
        for (_, expr) in self.body.exprs.iter() {
            match expr {
                HirExpr::Lambda {
                    body: LambdaBody::Block(block),
                    ..
                } => {
                    stack.push(*block);
                }
                HirExpr::Switch { arms, .. } => {
                    // Switch expressions can contain arm bodies lowered as statements/blocks stored
                    // in `body.stmts`, but those statement IDs are only referenced from the
                    // expression and are not reachable from the root statement tree.
                    //
                    // Explicit ctor invocations nested inside them are still invalid, so include
                    // them in the traversal.
                    for arm in arms {
                        match &arm.body {
                            SwitchArmBody::Block(stmt) | SwitchArmBody::Stmt(stmt) => {
                                stack.push(*stmt);
                            }
                            SwitchArmBody::Expr(_) => {}
                        }
                    }
                }
                _ => {}
            }
        }
        let mut seen = HashSet::new();
        let mut steps = 0u32;
        while let Some(stmt) = stack.pop() {
            // Avoid panics and infinite loops if the HIR is malformed.
            steps = steps.wrapping_add(1);
            if steps > 8192 {
                break;
            }
            if !seen.insert(stmt) {
                continue;
            }

            if self.is_explicit_constructor_invocation_stmt(stmt) && allowed != Some(stmt) {
                self.diagnostics.push(Diagnostic::error(
                    "constructor-invocation-not-first",
                    "constructor invocation must be the first statement in a constructor",
                    Some(self.stmt_range(stmt)),
                ));
            }

            match &self.body.stmts[stmt] {
                HirStmt::Block { statements, .. } => {
                    stack.extend(statements.iter().copied());
                }
                HirStmt::If {
                    then_branch,
                    else_branch,
                    ..
                } => {
                    stack.push(*then_branch);
                    if let Some(else_branch) = else_branch {
                        stack.push(*else_branch);
                    }
                }
                HirStmt::While { body, .. } => {
                    stack.push(*body);
                }
                HirStmt::For { init, body, .. } => {
                    stack.extend(init.iter().copied());
                    stack.push(*body);
                }
                HirStmt::ForEach { body, .. } => {
                    stack.push(*body);
                }
                HirStmt::Synchronized { body, .. } => {
                    stack.push(*body);
                }
                HirStmt::Switch { body, .. } => {
                    stack.push(*body);
                }
                HirStmt::Try {
                    body,
                    catches,
                    finally,
                    ..
                } => {
                    stack.push(*body);
                    for catch in catches {
                        stack.push(catch.body);
                    }
                    if let Some(finally) = finally {
                        stack.push(*finally);
                    }
                }
                HirStmt::Let { .. }
                | HirStmt::Expr { .. }
                | HirStmt::Yield { .. }
                | HirStmt::Return { .. }
                | HirStmt::Assert { .. }
                | HirStmt::Throw { .. }
                | HirStmt::Break { .. }
                | HirStmt::Continue { .. }
                | HirStmt::Empty { .. } => {}
            }
        }
    }

    fn is_explicit_constructor_invocation_stmt(&self, stmt: nova_hir::hir::StmtId) -> bool {
        let HirStmt::Expr { expr, .. } = &self.body.stmts[stmt] else {
            return false;
        };

        let HirExpr::Call { callee, .. } = &self.body.exprs[*expr] else {
            return false;
        };

        matches!(
            &self.body.exprs[*callee],
            HirExpr::This { .. } | HirExpr::Super { .. }
        )
    }

    fn stmt_range(&self, stmt: nova_hir::hir::StmtId) -> Span {
        match &self.body.stmts[stmt] {
            HirStmt::Block { range, .. }
            | HirStmt::Let { range, .. }
            | HirStmt::Expr { range, .. }
            | HirStmt::Yield { range, .. }
            | HirStmt::Assert { range, .. }
            | HirStmt::Return { range, .. }
            | HirStmt::If { range, .. }
            | HirStmt::While { range, .. }
            | HirStmt::For { range, .. }
            | HirStmt::ForEach { range, .. }
            | HirStmt::Synchronized { range, .. }
            | HirStmt::Switch { range, .. }
            | HirStmt::Try { range, .. }
            | HirStmt::Throw { range, .. }
            | HirStmt::Break { range }
            | HirStmt::Continue { range }
            | HirStmt::Empty { range } => *range,
        }
    }

    fn ensure_workspace_class(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        binary_name: &str,
    ) -> Option<ClassId> {
        // Mirror resolver behavior: application class loaders cannot define classes in `java.*`,
        // so even if the workspace contains a `java.*` definition we should not load it for
        // downstream type checking (it would otherwise "rescue" unresolved `java.*` references).
        if binary_name.starts_with("java.") {
            return None;
        }

        let file = def_file(self.owner);
        let project = self.db.file_project(file);
        let workspace = self.db.workspace_def_map(project);
        let item = workspace.item_by_type_name_str(binary_name)?;

        // In JPMS mode, prevent unreadable/unexported workspace (source) types from being "rescued"
        // during type checking. Name resolution already enforces JPMS access rules, so if a type
        // reference produced `Type::Named` due to JPMS restrictions we must not define/load the
        // workspace `ClassDef` here (otherwise member/method resolution can succeed and diagnostics
        // become inconsistent).
        if let Some(env) = self.db.jpms_compilation_env(project).as_deref() {
            let cfg = self.db.project_config(project);
            let file_rel = self.db.file_rel_path(file);
            let from = module_for_file(&cfg, file_rel.as_str());
            let to = workspace
                .module_for_item(item)
                .cloned()
                .unwrap_or_else(ModuleName::unnamed);

            if !env.env.graph.can_read(&from, &to) {
                // The workspace store may already contain a defined `ClassDef` for this type
                // (e.g. from project-level preloading). Remove it so member/method resolution can't
                // observe it through `TypeEnv::lookup_class`.
                let _ = loader.store.remove_class(binary_name);
                return None;
            }

            // Same-module access is always allowed; otherwise require the package to be exported.
            if from != to {
                let package = binary_name
                    .rsplit_once('.')
                    .map(|(pkg, _)| pkg)
                    .unwrap_or("");
                if let Some(info) = env.env.graph.get(&to) {
                    if !info.exports_package_to(package, &from) {
                        let _ = loader.store.remove_class(binary_name);
                        return None;
                    }
                }
            }
        }

        if self.workspace_loaded.contains(binary_name) {
            return Some(loader.store.intern_class_id(binary_name));
        }
        if self.workspace_in_progress.contains(binary_name) {
            return Some(loader.store.intern_class_id(binary_name));
        }

        let kind = match item {
            nova_hir::ids::ItemId::Interface(_) | nova_hir::ids::ItemId::Annotation(_) => {
                ClassKind::Interface
            }
            _ => ClassKind::Class,
        };

        // Reserve the id early so self-referential members (e.g. `A next;`) can resolve to a stable
        // `Type::Class` instead of forcing `Type::Named`.
        let class_id = loader.store.intern_class_id(binary_name);

        // If the class is already defined (i.e. not the minimal placeholder inserted by
        // `TypeStore::intern_class_id`), avoid re-loading it. Re-defining would allocate duplicate
        // type params and overwrite constructor metadata (e.g. varargs tagging).
        let already_defined = loader
            .store
            .class(class_id)
            .is_some_and(|def| !is_placeholder_class_def(def) && def.kind == kind);
        if already_defined {
            self.workspace_loaded.insert(binary_name.to_string());
            return Some(class_id);
        }

        self.workspace_in_progress.insert(binary_name.to_string());

        let item_file = item.file();
        let tree = self.db.hir_item_tree(item_file);
        let scopes = self.db.scope_graph(item_file);
        let class_scope = scopes
            .class_scopes
            .get(&item)
            .copied()
            .unwrap_or(scopes.file_scope);

        let object_ty = Type::class(loader.store.well_known().object, vec![]);
        let mut class_vars = HashMap::new();
        let class_type_params = allocate_type_params(
            self.resolver,
            &scopes.scopes,
            class_scope,
            loader,
            &object_ty,
            item_type_params(&tree, item),
            &mut class_vars,
        );
        let class_type_param_ids: Vec<TypeVarId> =
            class_type_params.iter().map(|(_, id)| *id).collect();
        let (kind, super_class, interfaces) = source_item_supertypes(
            self.resolver,
            &scopes.scopes,
            class_scope,
            loader,
            &class_vars,
            &tree,
            item,
            binary_name,
            class_id,
        );

        // Ensure any referenced supertypes are loaded so inherited members can be discovered even
        // when this helper is used in demand-driven queries (which only define types in the
        // current file).
        if let Some(sc) = &super_class {
            self.ensure_type_loaded(loader, sc);
        }
        for iface in &interfaces {
            self.ensure_type_loaded(loader, iface);
        }

        let members = match item {
            nova_hir::ids::ItemId::Class(id) => tree
                .classes
                .get(&id.ast_id)
                .map(|data| data.members.as_slice()),
            nova_hir::ids::ItemId::Interface(id) => tree
                .interfaces
                .get(&id.ast_id)
                .map(|data| data.members.as_slice()),
            nova_hir::ids::ItemId::Enum(id) => tree
                .enums
                .get(&id.ast_id)
                .map(|data| data.members.as_slice()),
            nova_hir::ids::ItemId::Record(id) => tree
                .records
                .get(&id.ast_id)
                .map(|data| data.members.as_slice()),
            nova_hir::ids::ItemId::Annotation(id) => tree
                .annotations
                .get(&id.ast_id)
                .map(|data| data.members.as_slice()),
        };

        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut constructors = Vec::new();

        if let Some(members) = members {
            for member in members {
                match *member {
                    nova_hir::item_tree::Member::Field(fid) => {
                        let Some(field) = tree.fields.get(&fid.ast_id) else {
                            continue;
                        };

                        preload_type_names(
                            self.resolver,
                            &scopes.scopes,
                            class_scope,
                            loader,
                            &field.ty,
                        );
                        let ty = nova_resolve::type_ref::resolve_type_ref_text(
                            self.resolver,
                            &scopes.scopes,
                            class_scope,
                            &*loader.store,
                            &class_vars,
                            &field.ty,
                            None,
                        )
                        .ty;

                        let is_implicitly_static =
                            field.kind == FieldKind::EnumConstant || kind == ClassKind::Interface;
                        let is_static =
                            is_implicitly_static || field.modifiers.raw & Modifiers::STATIC != 0;
                        let is_final =
                            is_implicitly_static || field.modifiers.raw & Modifiers::FINAL != 0;
                        fields.push(FieldDef {
                            name: field.name.clone(),
                            ty,
                            is_static,
                            is_final,
                        });
                    }
                    nova_hir::item_tree::Member::Method(mid) => {
                        let Some(method) = tree.methods.get(&mid.ast_id) else {
                            continue;
                        };

                        let scope = scopes
                            .method_scopes
                            .get(&mid)
                            .copied()
                            .unwrap_or(class_scope);
                        let mut vars = class_vars.clone();
                        let type_params = allocate_type_params(
                            self.resolver,
                            &scopes.scopes,
                            scope,
                            loader,
                            &object_ty,
                            &method.type_params,
                            &mut vars,
                        );
                        let method_type_param_ids: Vec<TypeVarId> =
                            type_params.iter().map(|(_, id)| *id).collect();

                        let is_varargs = method.params.last().is_some_and(|param| {
                            param.is_varargs || param.ty.trim().contains("...")
                        });

                        let params = method
                            .params
                            .iter()
                            .enumerate()
                            .map(|(idx, p)| {
                                let is_varargs_param = is_varargs && idx + 1 == method.params.len();
                                preload_type_names(
                                    self.resolver,
                                    &scopes.scopes,
                                    scope,
                                    loader,
                                    &p.ty,
                                );
                                let ty = nova_resolve::type_ref::resolve_type_ref_text(
                                    self.resolver,
                                    &scopes.scopes,
                                    scope,
                                    &*loader.store,
                                    &vars,
                                    &p.ty,
                                    None,
                                )
                                .ty;

                                // If the ellipsis was not included in the captured type text (e.g.
                                // due to parse recovery or future lowering changes), still model
                                // varargs as an array type.
                                if is_varargs_param && !p.ty.trim().contains("...") {
                                    Type::Array(Box::new(ty))
                                } else {
                                    ty
                                }
                            })
                            .collect::<Vec<_>>();

                        preload_type_names(
                            self.resolver,
                            &scopes.scopes,
                            scope,
                            loader,
                            &method.return_ty,
                        );
                        let return_type = nova_resolve::type_ref::resolve_type_ref_text(
                            self.resolver,
                            &scopes.scopes,
                            scope,
                            &*loader.store,
                            &vars,
                            &method.return_ty,
                            None,
                        )
                        .ty;

                        let is_static = method.modifiers.raw & Modifiers::STATIC != 0;
                        methods.push(MethodDef {
                            name: method.name.clone(),
                            type_params: method_type_param_ids,
                            params,
                            return_type,
                            is_static,
                            is_varargs,
                            is_abstract: method.body.is_none(),
                        });
                    }
                    nova_hir::item_tree::Member::Constructor(cid) => {
                        let Some(ctor) = tree.constructors.get(&cid.ast_id) else {
                            continue;
                        };

                        let scope = scopes
                            .constructor_scopes
                            .get(&cid)
                            .copied()
                            .unwrap_or(class_scope);
                        // Best-effort: treat class type params as in-scope for constructor
                        // signatures.
                        let vars = class_vars.clone();
                        let is_varargs = ctor
                            .params
                            .last()
                            .is_some_and(|p| p.is_varargs || p.ty.trim().contains("..."));
                        let params = ctor
                            .params
                            .iter()
                            .enumerate()
                            .map(|(idx, p)| {
                                let is_varargs_param = is_varargs && idx + 1 == ctor.params.len();
                                preload_type_names(
                                    self.resolver,
                                    &scopes.scopes,
                                    scope,
                                    loader,
                                    &p.ty,
                                );
                                let ty = nova_resolve::type_ref::resolve_type_ref_text(
                                    self.resolver,
                                    &scopes.scopes,
                                    scope,
                                    &*loader.store,
                                    &vars,
                                    &p.ty,
                                    None,
                                )
                                .ty;

                                if is_varargs_param && !p.ty.trim().contains("...") {
                                    Type::Array(Box::new(ty))
                                } else {
                                    ty
                                }
                            })
                            .collect::<Vec<_>>();

                        let is_accessible = ctor.modifiers.raw & Modifiers::PRIVATE == 0;
                        constructors.push(ConstructorDef {
                            params,
                            is_varargs,
                            is_accessible,
                        });
                    }
                    _ => {}
                }
            }
        }

        // Best-effort: Java implicit constructors.
        //
        // - Classes with no declared constructors get an implicit no-arg constructor.
        // - Records always have a canonical constructor matching their components; if none was
        //   declared (or if only non-canonical ctors were declared), add it.
        match item {
            nova_hir::ids::ItemId::Class(_) if constructors.is_empty() => {
                constructors.push(ConstructorDef {
                    params: Vec::new(),
                    is_varargs: false,
                    is_accessible: true,
                });
            }
            nova_hir::ids::ItemId::Record(id) => {
                let record = tree.record(id);
                let canonical_params = record
                    .components
                    .iter()
                    .map(|component| {
                        preload_type_names(
                            self.resolver,
                            &scopes.scopes,
                            class_scope,
                            loader,
                            &component.ty,
                        );
                        nova_resolve::type_ref::resolve_type_ref_text(
                            self.resolver,
                            &scopes.scopes,
                            class_scope,
                            &*loader.store,
                            &class_vars,
                            &component.ty,
                            None,
                        )
                        .ty
                    })
                    .collect::<Vec<_>>();

                let used_ellipsis = record
                    .components
                    .last()
                    .is_some_and(|component| component.ty.trim().contains("..."));
                let last_is_array = canonical_params
                    .last()
                    .is_some_and(|t| matches!(t, Type::Array(_)));
                let canonical_is_varargs = used_ellipsis && last_is_array;

                let canonical_exists = constructors.iter().any(|ctor| {
                    ctor.params == canonical_params && ctor.is_varargs == canonical_is_varargs
                });
                if !canonical_exists {
                    let is_accessible = record.modifiers.raw & Modifiers::PRIVATE == 0;
                    constructors.push(ConstructorDef {
                        params: canonical_params,
                        is_varargs: canonical_is_varargs,
                        is_accessible,
                    });
                }
            }
            _ => {}
        }

        loader.store.define_class(
            class_id,
            ClassDef {
                name: binary_name.to_string(),
                kind,
                type_params: class_type_param_ids,
                super_class,
                interfaces,
                fields,
                constructors,
                methods,
            },
        );

        self.workspace_in_progress.remove(binary_name);
        self.workspace_loaded.insert(binary_name.to_string());

        Some(class_id)
    }

    fn ensure_type_loaded(&mut self, loader: &mut ExternalTypeLoader<'_>, ty: &Type) {
        fn ensure_inner<'a, 'idx>(
            checker: &mut BodyChecker<'a, 'idx>,
            loader: &mut ExternalTypeLoader<'_>,
            ty: &Type,
            seen_classes: &mut HashSet<ClassId>,
            seen_type_vars: &mut HashSet<TypeVarId>,
        ) {
            match ty {
                Type::Class(nova_types::ClassType { def, args }) => {
                    if !seen_classes.insert(*def) {
                        return;
                    }

                    // Ensure the class body is available (either workspace or external).
                    let Some(name) = loader.store.class(*def).map(|def| def.name.clone()) else {
                        return;
                    };
                    if checker.ensure_workspace_class(loader, &name).is_none() {
                        let _ = loader.ensure_class(&name);
                    }

                    // Also ensure type arguments are loaded (best-effort), since wildcards/type vars
                    // can refer to external bounds that member lookup may need to normalize.
                    for arg in args {
                        ensure_inner(checker, loader, arg, seen_classes, seen_type_vars);
                    }

                    // Ensure direct supertypes/interfaces are loaded so member resolution can
                    // traverse them (including when the supertypes are external and only stubbed).
                    let (super_class, interfaces) = match loader.store.class(*def) {
                        Some(def) => (def.super_class.clone(), def.interfaces.clone()),
                        None => return,
                    };

                    if let Some(sc) = super_class {
                        ensure_inner(checker, loader, &sc, seen_classes, seen_type_vars);
                    }
                    for iface in interfaces {
                        ensure_inner(checker, loader, &iface, seen_classes, seen_type_vars);
                    }
                }
                Type::Named(name) => {
                    let id = loader.store.intern_class_id(name);
                    if !seen_classes.insert(id) {
                        return;
                    }
                    if checker.ensure_workspace_class(loader, name).is_none() {
                        let _ = loader.ensure_class(name);
                    }

                    let (super_class, interfaces) = match loader.store.class(id) {
                        Some(def) => (def.super_class.clone(), def.interfaces.clone()),
                        None => return,
                    };

                    if let Some(sc) = super_class {
                        ensure_inner(checker, loader, &sc, seen_classes, seen_type_vars);
                    }
                    for iface in interfaces {
                        ensure_inner(checker, loader, &iface, seen_classes, seen_type_vars);
                    }
                }
                Type::Array(elem) => {
                    ensure_inner(checker, loader, elem, seen_classes, seen_type_vars);
                }
                Type::Intersection(types) => {
                    for t in types {
                        ensure_inner(checker, loader, t, seen_classes, seen_type_vars);
                    }
                }
                Type::TypeVar(id) => {
                    if !seen_type_vars.insert(*id) {
                        return;
                    }
                    let Some(tp) = loader.store.type_param(*id).cloned() else {
                        return;
                    };
                    for bound in tp.upper_bounds {
                        ensure_inner(checker, loader, &bound, seen_classes, seen_type_vars);
                    }
                    if let Some(lower) = tp.lower_bound {
                        ensure_inner(checker, loader, &lower, seen_classes, seen_type_vars);
                    }
                }
                Type::Wildcard(bound) => match bound {
                    WildcardBound::Unbounded => {}
                    WildcardBound::Extends(upper) | WildcardBound::Super(upper) => {
                        ensure_inner(checker, loader, upper, seen_classes, seen_type_vars);
                    }
                },
                Type::VirtualInner { owner, .. } => {
                    let owner_ty = Type::class(*owner, vec![]);
                    ensure_inner(checker, loader, &owner_ty, seen_classes, seen_type_vars);
                }
                Type::Void | Type::Primitive(_) | Type::Null | Type::Unknown | Type::Error => {}
            }
        }

        let mut seen_classes = HashSet::new();
        let mut seen_type_vars = HashSet::new();
        ensure_inner(self, loader, ty, &mut seen_classes, &mut seen_type_vars);
    }

    fn is_statement_expression(&self, expr: HirExprId) -> bool {
        let expr_data = &self.body.exprs[expr];
        let expr_range = expr_data.range();

        match expr_data {
            HirExpr::Missing { .. } => true,
            _ if self.range_is_wrapped_in_parens(expr_range) => false,
            HirExpr::Assign { .. }
            | HirExpr::Call { .. }
            | HirExpr::Unary {
                op: UnaryOp::PreInc | UnaryOp::PreDec | UnaryOp::PostInc | UnaryOp::PostDec,
                ..
            } => true,
            HirExpr::New {
                class,
                class_range,
                range,
                ..
            } => {
                // Match javac/JLS 14.8: only *class instance* creation expressions are allowed as
                // expression statements (`new C()`), not array creation expressions (`new int[0]`).
                //
                // Array creation is lowered as `HirExpr::New` too, so detect it via the inferred
                // type (preferred) and fall back to textual heuristics.
                let inferred_is_array = self
                    .expr_info
                    .get(expr.idx())
                    .and_then(|info| info.as_ref())
                    .is_some_and(|info| matches!(info.ty, Type::Array(_)));

                let syntactic_is_array =
                    class.contains('[') || self.new_expr_array_dims(*class_range, *range).is_some();

                !(inferred_is_array || syntactic_is_array)
            }
            _ => false,
        }
    }

    fn validate_statement_expression(&mut self, expr: HirExprId) {
        if matches!(&self.body.exprs[expr], HirExpr::Missing { .. }) {
            return;
        }

        if !self.is_statement_expression(expr) {
            self.diagnostics.push(Diagnostic::error(
                "invalid-statement-expression",
                "invalid expression statement",
                Some(self.body.exprs[expr].range()),
            ));
            // Backwards-compatible alias used by some clients/tests.
            self.diagnostics.push(Diagnostic::error(
                "invalid-expr-stmt",
                "invalid expression statement",
                Some(self.body.exprs[expr].range()),
            ));
        }
    }

    fn validate_for_update_expression(&mut self, expr: HirExprId) {
        if matches!(&self.body.exprs[expr], HirExpr::Missing { .. }) {
            return;
        }

        if !self.is_statement_expression(expr) {
            self.diagnostics.push(Diagnostic::error(
                "invalid-for-update-expression",
                "invalid expression in for-loop update",
                Some(self.body.exprs[expr].range()),
            ));
            // Backwards-compatible alias used by some clients/tests.
            self.diagnostics.push(Diagnostic::error(
                "invalid-for-update-expr",
                "invalid expression in for-loop update",
                Some(self.body.exprs[expr].range()),
            ));
        }
    }

    fn is_lvalue_expr(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        expr: HirExprId,
        is_type_ref: bool,
    ) -> bool {
        if is_type_ref {
            return false;
        }

        match &self.body.exprs[expr] {
            HirExpr::Name { name, .. } => {
                let scope = self
                    .expr_scopes
                    .scope_for_expr(expr)
                    .unwrap_or_else(|| self.expr_scopes.root_scope());
                if self
                    .expr_scopes
                    .resolve_name(scope, &Name::from(name.as_str()))
                    .is_some()
                {
                    true
                } else {
                    match self.resolver.resolve_name_detailed(
                        self.scopes,
                        self.scope_id,
                        &Name::from(name.as_str()),
                    ) {
                        NameResolution::Resolved(res) => match res {
                            Resolution::Local(_)
                            | Resolution::Parameter(_)
                            | Resolution::Field(_) => true,
                            Resolution::StaticMember(member) => match member {
                                StaticMemberResolution::SourceField(_) => true,
                                StaticMemberResolution::SourceMethod(_) => false,
                                StaticMemberResolution::External(id) => {
                                    match id.as_str().split_once("::") {
                                        Some((owner, member)) => {
                                            let receiver = self
                                                .ensure_workspace_class(loader, owner)
                                                .or_else(|| loader.ensure_class(owner))
                                                .map(|id| Type::class(id, vec![]))
                                                .unwrap_or_else(|| Type::Named(owner.to_string()));
                                            self.ensure_type_loaded(loader, &receiver);
                                            let env_ro: &dyn TypeEnv = &*loader.store;
                                            let mut ctx = TyContext::new(env_ro);
                                            ctx.resolve_field(&receiver, member, CallKind::Static)
                                                .is_some()
                                        }
                                        None => false,
                                    }
                                }
                            },
                            Resolution::Methods(_)
                            | Resolution::Constructors(_)
                            | Resolution::Type(_)
                            | Resolution::Package(_) => false,
                        },
                        // Prefer the name-resolution diagnostics produced while inferring the name
                        // itself.
                        NameResolution::Ambiguous(_) | NameResolution::Unresolved => true,
                    }
                }
            }
            HirExpr::FieldAccess { .. } | HirExpr::ArrayAccess { .. } => true,
            _ => false,
        }
    }

    fn range_is_wrapped_in_parens(&self, range: Span) -> bool {
        let file = def_file(self.owner);
        let Ok(start) = u32::try_from(range.start) else {
            return false;
        };
        let Ok(end) = u32::try_from(range.end) else {
            return false;
        };

        let parse = self.db.parse_java(file);

        let Some(start_token) = parse
            .token_at_offset(start)
            .right_biased()
            .or_else(|| parse.token_at_offset(start).left_biased())
        else {
            return false;
        };

        let mut prev = start_token.prev_token();
        while prev.as_ref().is_some_and(|tok| tok.kind().is_trivia()) {
            prev = prev.and_then(|tok| tok.prev_token());
        }
        let Some(prev) = prev else {
            return false;
        };
        if prev.kind() != SyntaxKind::LParen {
            return false;
        }

        let Some(end_token) = parse
            .token_at_offset(end)
            .right_biased()
            .or_else(|| parse.token_at_offset(end).left_biased())
        else {
            return false;
        };
        let mut next = Some(end_token);
        while next.as_ref().is_some_and(|tok| tok.kind().is_trivia()) {
            next = next.and_then(|tok| tok.next_token());
        }
        let Some(next) = next else {
            return false;
        };
        next.kind() == SyntaxKind::RParen
    }

    fn check_stmt(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        stmt: nova_hir::hir::StmtId,
        expected_return: &Type,
    ) {
        self.tick();
        let slot: *mut Type = &mut self.current_expected_return;
        let prev = std::mem::replace(&mut self.current_expected_return, expected_return.clone());
        let _restore = RestoreTypeOnDrop {
            slot,
            prev: Some(prev),
        };
        match &self.body.stmts[stmt] {
            HirStmt::Block { statements, .. } => {
                for &stmt in statements {
                    self.check_stmt(loader, stmt, expected_return);
                }
            }
            HirStmt::Let {
                local, initializer, ..
            } => {
                let data = &self.body.locals[*local];

                // Handle `var` inference (Java 10+).
                if data.ty_text.trim() == "var" && self.java_level.supports_var_local_inference() {
                    let diag_span = if data.ty_range.is_empty() {
                        data.range
                    } else {
                        data.ty_range
                    };

                    let Some(init) = initializer else {
                        // Align with javac: `var` requires an initializer (Java 10+).
                        self.diagnostics.push(Diagnostic::error(
                            "var-requires-initializer",
                            "`var` declarations must have an initializer",
                            Some(diag_span),
                        ));
                        self.diagnostics.push(Diagnostic::error(
                            "invalid-var",
                            "`var` local variables require an initializer",
                            Some(diag_span),
                        ));
                        self.local_types[local.idx()] = Type::Error;
                        self.local_ty_states[local.idx()] = LocalTyState::Computed;
                        return;
                    };

                    let init_range = self.body.exprs[*init].range();
                    let init_is_poly = self.is_poly_expression(*init);

                    // `var` cannot be inferred from target-typed ("poly") expressions without an
                    // explicit target type.
                    if init_is_poly {
                        // Still walk the expression for internal errors/best-effort IDE info.
                        let _ = self.infer_expr(loader, *init);
                        self.diagnostics.push(Diagnostic::error(
                            "var-functional-initializer",
                            "`var` cannot be initialized with a lambda, method reference, or constructor reference",
                            Some(init_range),
                        ));
                        self.diagnostics.push(Diagnostic::error(
                            "var-poly-expression",
                            "cannot infer `var` from a poly expression without a target type",
                            Some(init_range),
                        ));
                        self.local_types[local.idx()] = Type::Error;
                        self.local_ty_states[local.idx()] = LocalTyState::Computed;
                        return;
                    }
                    let init_ty = self.infer_expr(loader, *init).ty;

                    let init_is_null = matches!(&self.body.exprs[*init], HirExpr::Null { .. })
                        || init_ty == Type::Null;
                    if init_is_null {
                        self.diagnostics.push(Diagnostic::error(
                            "var-null-initializer",
                            "`var` cannot be initialized with `null`",
                            Some(init_range),
                        ));
                        self.diagnostics.push(Diagnostic::error(
                            "invalid-var",
                            "cannot infer a `var` local variable type from `null`",
                            Some(init_range),
                        ));
                        self.local_types[local.idx()] = Type::Error;
                        self.local_ty_states[local.idx()] = LocalTyState::Computed;
                        return;
                    }

                    if init_ty == Type::Void {
                        self.diagnostics.push(Diagnostic::error(
                            "var-void-initializer",
                            "cannot infer `var` from `void` initializer",
                            Some(init_range),
                        ));
                        self.local_types[local.idx()] = Type::Error;
                        self.local_ty_states[local.idx()] = LocalTyState::Computed;
                        return;
                    }

                    if init_ty.is_errorish() {
                        self.diagnostics.push(Diagnostic::error(
                            "var-cannot-infer",
                            "cannot infer `var` type from initializer",
                            Some(diag_span),
                        ));
                        self.local_types[local.idx()] = Type::Error;
                        self.local_ty_states[local.idx()] = LocalTyState::Computed;
                        return;
                    }

                    self.local_types[local.idx()] = init_ty;
                    self.local_ty_states[local.idx()] = LocalTyState::Computed;
                    return;
                }

                let mut decl_ty =
                    self.resolve_source_type(loader, data.ty_text.as_str(), Some(data.ty_range));
                if decl_ty == Type::Void {
                    self.diagnostics.push(Diagnostic::error(
                        "void-variable-type",
                        "`void` is not a valid type for variables",
                        Some(data.ty_range),
                    ));
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-void-type",
                        "`void` is not a valid type for variables",
                        Some(data.ty_range),
                    ));
                    decl_ty = Type::Error;
                }
                self.local_types[local.idx()] = decl_ty.clone();
                self.local_ty_states[local.idx()] = LocalTyState::Computed;

                let Some(init) = initializer else {
                    return;
                };

                // Array initializers (`{...}`) are only typeable when we have an expected array type
                // from the declaration itself (e.g. `int[] a = {1,2};`).
                if matches!(self.body.exprs[*init], HirExpr::ArrayInitializer { .. }) {
                    if matches!(&decl_ty, Type::Array(_)) && !decl_ty.is_errorish() {
                        let _ = self.infer_array_initializer_with_expected(loader, *init, &decl_ty);
                    } else {
                        let _ = self.infer_expr(loader, *init);
                    }
                    return;
                }

                let init_ty = self
                    .infer_expr_with_expected(
                        loader,
                        *init,
                        (!decl_ty.is_errorish()).then_some(&decl_ty),
                    )
                    .ty;

                if decl_ty.is_errorish() || init_ty.is_errorish() {
                    return;
                }

                let env_ro: &dyn TypeEnv = &*loader.store;
                match assignment_conversion_with_const(
                    env_ro,
                    &init_ty,
                    &decl_ty,
                    const_value_for_expr(self.body, *init),
                ) {
                    None => {
                        let expected = format_type(env_ro, &decl_ty);
                        let found = format_type(env_ro, &init_ty);
                        self.diagnostics.push(Diagnostic::error(
                            "type-mismatch",
                            format!("type mismatch: expected {expected}, found {found}"),
                            Some(self.body.exprs[*init].range()),
                        ));
                    }
                    Some(conv) => {
                        for warning in conv.warnings {
                            if let TypeWarning::Unchecked(reason) = warning {
                                self.emit_unchecked_warning(reason, self.body.exprs[*init].range());
                            }
                        }
                    }
                }
            }
            HirStmt::Expr { expr, .. } => {
                // Heuristic: `yield` is a restricted identifier in Java, but `yield <expr>;` is
                // only valid as a statement inside a switch *expression*. Outside of that
                // context, the parser may recover by producing a bare `yield` name expression
                // statement (e.g. `yield 1;` becomes `yield; 1;` with a missing semicolon).
                //
                // Report a dedicated diagnostic so IDE users get a clearer error than the
                // generic `unresolved-name`.
                if matches!(&self.body.exprs[*expr], HirExpr::Name { name, .. } if name == "yield")
                {
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-yield",
                        "`yield` is only valid inside a switch expression",
                        Some(self.body.exprs[*expr].range()),
                    ));
                }
                let _ = self.infer_expr(loader, *expr);
                self.validate_statement_expression(*expr);
            }
            HirStmt::Assert {
                condition, message, ..
            } => {
                let condition_ty = self.infer_expr(loader, *condition).ty;
                if !condition_ty.is_errorish() {
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    if assignment_conversion(env_ro, &condition_ty, &Type::boolean()).is_none() {
                        self.diagnostics.push(Diagnostic::error(
                            "assert-condition-not-boolean",
                            "assert condition must be boolean",
                            Some(self.body.exprs[*condition].range()),
                        ));
                    }
                }

                if let Some(expr) = message {
                    let _ = self.infer_expr(loader, *expr);
                }
            }
            HirStmt::Yield { expr, range } => {
                let ty = expr
                    .as_ref()
                    .map(|expr| self.infer_expr(loader, *expr).ty)
                    .unwrap_or(Type::Unknown);

                let Some(yields) = self.switch_yield_stack.last_mut() else {
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-yield",
                        "`yield` is only valid inside a switch expression",
                        Some(*range),
                    ));
                    return;
                };

                yields.push(ty);
            }
            HirStmt::Return { expr, range } => {
                if matches!(self.owner, DefWithBodyId::Initializer(_)) {
                    self.diagnostics.push(Diagnostic::error(
                        "return-in-initializer",
                        "`return` is not allowed in initializer blocks",
                        Some(*range),
                    ));
                    if let Some(expr) = expr {
                        let _ = self.infer_expr(loader, *expr);
                    }
                    return;
                }
                let Some(expr) = expr else {
                    if *expected_return == Type::Void || expected_return.is_errorish() {
                        return;
                    }
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    let expected = format_type(env_ro, expected_return);
                    self.diagnostics.push(Diagnostic::error(
                        "return-mismatch",
                        format!("return type mismatch: expected {expected}, found void"),
                        Some(*range),
                    ));
                    return;
                };
                // Returning a value from a `void` method is always an error, but we still type the
                // expression for IDE features. Don't propagate `void` as an "expected type" into
                // call/generic inference: Java doesn't allow `void` as a type argument and using it
                // as a target type can lead to nonsensical inferred return types (e.g. `T = void`).
                let expected = (!expected_return.is_errorish() && expected_return != &Type::Void)
                    .then_some(expected_return);
                let expr_ty = self.infer_expr_with_expected(loader, *expr, expected).ty;
                if expected_return == &Type::Void {
                    self.diagnostics.push(Diagnostic::error(
                        "return-mismatch",
                        "cannot return a value from a `void` method",
                        Some(self.body.exprs[*expr].range()),
                    ));
                    return;
                }

                if expr_ty.is_errorish() || expected_return.is_errorish() {
                    return;
                }

                let env_ro: &dyn TypeEnv = &*loader.store;
                match assignment_conversion_with_const(
                    env_ro,
                    &expr_ty,
                    expected_return,
                    const_value_for_expr(self.body, *expr),
                ) {
                    None => {
                        let expected = format_type(env_ro, expected_return);
                        let found = format_type(env_ro, &expr_ty);
                        self.diagnostics.push(Diagnostic::error(
                            "return-mismatch",
                            format!("return type mismatch: expected {expected}, found {found}"),
                            Some(self.body.exprs[*expr].range()),
                        ));
                    }
                    Some(conv) => {
                        for warning in conv.warnings {
                            if let TypeWarning::Unchecked(reason) = warning {
                                self.emit_unchecked_warning(reason, self.body.exprs[*expr].range());
                            }
                        }
                    }
                }
            }
            HirStmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let condition_ty = self.infer_expr(loader, *condition).ty;
                if !condition_ty.is_errorish() {
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    if assignment_conversion(env_ro, &condition_ty, &Type::boolean()).is_none() {
                        self.diagnostics.push(Diagnostic::error(
                            "condition-not-boolean",
                            "condition must be boolean",
                            Some(self.body.exprs[*condition].range()),
                        ));
                    }
                }
                self.check_stmt(loader, *then_branch, expected_return);
                if let Some(else_branch) = else_branch {
                    self.check_stmt(loader, *else_branch, expected_return);
                }
            }
            HirStmt::While {
                condition, body, ..
            } => {
                let condition_ty = self.infer_expr(loader, *condition).ty;
                if !condition_ty.is_errorish() {
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    if assignment_conversion(env_ro, &condition_ty, &Type::boolean()).is_none() {
                        self.diagnostics.push(Diagnostic::error(
                            "condition-not-boolean",
                            "condition must be boolean",
                            Some(self.body.exprs[*condition].range()),
                        ));
                    }
                }
                self.check_stmt(loader, *body, expected_return);
            }
            HirStmt::For {
                init,
                condition,
                update,
                body,
                ..
            } => {
                for stmt in init {
                    self.check_stmt(loader, *stmt, expected_return);
                }
                if let Some(condition) = condition {
                    let condition_ty = self.infer_expr(loader, *condition).ty;
                    if !condition_ty.is_errorish() {
                        let env_ro: &dyn TypeEnv = &*loader.store;
                        if assignment_conversion(env_ro, &condition_ty, &Type::boolean()).is_none()
                        {
                            self.diagnostics.push(Diagnostic::error(
                                "condition-not-boolean",
                                "condition must be boolean",
                                Some(self.body.exprs[*condition].range()),
                            ));
                        }
                    }
                }
                for expr in update {
                    let _ = self.infer_expr(loader, *expr);
                    self.validate_for_update_expression(*expr);
                }
                self.check_stmt(loader, *body, expected_return);
            }
            HirStmt::ForEach {
                local,
                iterable,
                body,
                ..
            } => {
                let data = &self.body.locals[*local];
                let iterable_ty = self.infer_expr(loader, *iterable).ty;
                let element_ty = self.infer_foreach_element_type(loader, &iterable_ty);
                let is_iterable =
                    matches!(iterable_ty, Type::Array(_)) || !element_ty.is_errorish();

                // Emit an error if the iterable expression is not something we can iterate.
                if !iterable_ty.is_errorish() && !is_iterable {
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    let found = format_type(env_ro, &iterable_ty);
                    self.diagnostics.push(Diagnostic::error(
                        "foreach-non-iterable",
                        "expression in enhanced for-loop is not iterable",
                        Some(self.body.exprs[*iterable].range()),
                    ));
                    self.diagnostics.push(Diagnostic::error(
                        "foreach-not-iterable",
                        format!("foreach expression is not iterable: found {found}"),
                        Some(self.body.exprs[*iterable].range()),
                    ));
                }

                // If `var` inference is not enabled at this language level, treat `var` as an
                // explicit type name for compatibility with older Java versions.
                if data.ty_text.trim() != "var" || !self.var_inference_enabled() {
                    let mut decl_ty = self.resolve_source_type(
                        loader,
                        data.ty_text.as_str(),
                        Some(data.ty_range),
                    );
                    if decl_ty == Type::Void {
                        self.diagnostics.push(Diagnostic::error(
                            "void-variable-type",
                            "`void` is not a valid type for variables",
                            Some(data.ty_range),
                        ));
                        self.diagnostics.push(Diagnostic::error(
                            "invalid-void-type",
                            "`void` is not a valid type for variables",
                            Some(data.ty_range),
                        ));
                        decl_ty = Type::Error;
                    }
                    self.local_types[local.idx()] = decl_ty.clone();
                    self.local_ty_states[local.idx()] = LocalTyState::Computed;

                    if decl_ty.is_errorish() || element_ty.is_errorish() {
                        self.check_stmt(loader, *body, expected_return);
                        return;
                    }

                    let env_ro: &dyn TypeEnv = &*loader.store;
                    match assignment_conversion_with_const(env_ro, &element_ty, &decl_ty, None) {
                        None => {
                            let expected = format_type(env_ro, &decl_ty);
                            let found = format_type(env_ro, &element_ty);
                            self.diagnostics.push(Diagnostic::error(
                                "type-mismatch",
                                format!("type mismatch: expected {expected}, found {found}"),
                                Some(data.ty_range),
                            ));
                        }
                        Some(conv) => {
                            for warning in conv.warnings {
                                if let TypeWarning::Unchecked(reason) = warning {
                                    self.emit_unchecked_warning(reason, data.ty_range);
                                }
                            }
                        }
                    }
                } else {
                    // `var` local variable type inference was added in Java 10, including support
                    // in enhanced-for loops.
                    if element_ty.is_errorish() {
                        self.diagnostics.push(Diagnostic::error(
                            "cannot-infer-foreach-var",
                            "cannot infer foreach loop variable type from iterable expression",
                            Some(data.ty_range),
                        ));
                    } else {
                        self.local_types[local.idx()] = element_ty;
                    }
                    self.local_ty_states[local.idx()] = LocalTyState::Computed;
                }
                self.check_stmt(loader, *body, expected_return);
            }
            HirStmt::Synchronized { expr, body, .. } => {
                let lock_ty = self.infer_expr(loader, *expr).ty;
                if !lock_ty.is_errorish() && !lock_ty.is_reference() {
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-synchronized-expression",
                        "synchronized expression must be a reference type",
                        Some(self.body.exprs[*expr].range()),
                    ));
                }

                self.check_stmt(loader, *body, expected_return);
            }
            HirStmt::Switch { selector, body, .. } => {
                let _ = self.infer_expr(loader, *selector);
                self.check_stmt(loader, *body, expected_return);
            }
            HirStmt::Try {
                body,
                catches,
                finally,
                ..
            } => {
                self.check_stmt(loader, *body, expected_return);
                for catch in catches {
                    let data = &self.body.locals[catch.param];
                    let mut catch_ty = self.resolve_source_type(
                        loader,
                        data.ty_text.as_str(),
                        Some(data.ty_range),
                    );
                    if catch_ty == Type::Void {
                        self.diagnostics.push(Diagnostic::error(
                            "void-catch-parameter-type",
                            "`void` is not a valid catch parameter type",
                            Some(data.ty_range),
                        ));
                        self.diagnostics.push(Diagnostic::error(
                            "invalid-void-type",
                            "`void` is not a valid catch parameter type",
                            Some(data.ty_range),
                        ));
                        catch_ty = Type::Error;
                    }
                    self.local_types[catch.param.idx()] = catch_ty.clone();
                    self.local_ty_states[catch.param.idx()] = LocalTyState::Computed;

                    self.ensure_type_loaded(loader, &catch_ty);
                    if !catch_ty.is_errorish() {
                        // Best-effort: some minimal environments may not define Throwable.
                        if let Some(throwable_id) = loader.store.lookup_class("java.lang.Throwable")
                        {
                            let throwable_ty = Type::class(throwable_id, vec![]);

                            let env_ro: &dyn TypeEnv = &*loader.store;
                            if !is_subtype(env_ro, &catch_ty, &throwable_ty) {
                                let found = format_type(env_ro, &catch_ty);
                                self.diagnostics.push(Diagnostic::error(
                                    "invalid-catch-type",
                                    format!(
                                        "catch parameter type must be a subtype of Throwable; found {found}"
                                    ),
                                    Some(data.ty_range),
                                ));
                            }
                        }
                    }
                    self.check_stmt(loader, catch.body, expected_return);
                }
                if let Some(finally) = finally {
                    self.check_stmt(loader, *finally, expected_return);
                }
            }
            HirStmt::Throw { expr, .. } => {
                let expr_ty = self.infer_expr(loader, *expr).ty;

                self.ensure_type_loaded(loader, &expr_ty);
                if expr_ty.is_errorish() {
                    return;
                }

                let Some(throwable_id) = loader.store.lookup_class("java.lang.Throwable") else {
                    // Best-effort recovery: if the minimal environment does not define Throwable,
                    // we can't validate the throw statement.
                    return;
                };
                let throwable_ty = Type::class(throwable_id, vec![]);

                let env_ro: &dyn TypeEnv = &*loader.store;
                if assignment_conversion(env_ro, &expr_ty, &throwable_ty).is_none() {
                    let found = format_type(env_ro, &expr_ty);
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-throw",
                        format!("cannot throw expression of type {found}"),
                        Some(self.body.exprs[*expr].range()),
                    ));
                }
            }
            HirStmt::Break { .. } | HirStmt::Continue { .. } => {}
            HirStmt::Empty { .. } => {}
        }
    }

    fn infer_local_type(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        local: nova_hir::hir::LocalId,
    ) -> Type {
        if !self.lazy_locals {
            return self.local_types[local.idx()].clone();
        }

        match self.local_ty_states[local.idx()] {
            LocalTyState::Computed => {
                return self.local_types[local.idx()].clone();
            }
            LocalTyState::Computing => {
                // Prevent cycles like `var x = x;` or mutual recursion between `var` locals.
                let data = &self.body.locals[local];
                self.diagnostics.push(Diagnostic::error(
                    "cyclic-var",
                    format!("cyclic `var` initializer for `{}`", data.name),
                    Some(data.range),
                ));
                return Type::Unknown;
            }
            LocalTyState::Uncomputed => {}
        }

        self.local_ty_states[local.idx()] = LocalTyState::Computing;

        let data = &self.body.locals[local];
        // Lambda parameters are lowered as locals with an empty `ty_text` (their types are inferred
        // from the target functional interface). In demand-driven mode we infer locals lazily on
        // first use; avoid emitting `invalid-type-ref` noise by treating an empty type as unknown.
        //
        // This also helps with parse recovery where the type text might be missing.
        if data.ty_text.trim().is_empty() {
            self.local_types[local.idx()] = Type::Unknown;
            self.local_ty_states[local.idx()] = LocalTyState::Computed;
            return Type::Unknown;
        }
        let is_catch_param = self
            .local_is_catch_param
            .get(local.idx())
            .copied()
            .unwrap_or(false);
        let ty = if data.ty_text.trim() == "var" && self.var_inference_enabled() && !is_catch_param
        {
            if self.local_is_let_decl[local.idx()] {
                let diag_span = if data.ty_range.is_empty() {
                    data.range
                } else {
                    data.ty_range
                };

                if let Some(init) = self.local_initializers[local.idx()] {
                    let init_range = self.body.exprs[init].range();
                    let init_is_poly = self.is_poly_expression(init);
                    if init_is_poly {
                        // Still walk the expression for internal errors/best-effort IDE info.
                        let _ = self.infer_expr(loader, init);
                        self.diagnostics.push(Diagnostic::error(
                            "var-functional-initializer",
                            "`var` cannot be initialized with a lambda, method reference, or constructor reference",
                            Some(init_range),
                        ));
                        self.diagnostics.push(Diagnostic::error(
                            "var-poly-expression",
                            "cannot infer `var` from a poly expression without a target type",
                            Some(init_range),
                        ));
                        Type::Error
                    } else {
                        let init_ty = self.infer_expr(loader, init).ty;
                        let init_is_null = matches!(&self.body.exprs[init], HirExpr::Null { .. })
                            || init_ty == Type::Null;
                        if init_is_null {
                            self.diagnostics.push(Diagnostic::error(
                                "var-null-initializer",
                                "`var` cannot be initialized with `null`",
                                Some(init_range),
                            ));
                            self.diagnostics.push(Diagnostic::error(
                                "invalid-var",
                                "cannot infer a `var` local variable type from `null`",
                                Some(init_range),
                            ));
                            Type::Error
                        } else if init_ty == Type::Void {
                            self.diagnostics.push(Diagnostic::error(
                                "var-void-initializer",
                                "cannot infer `var` from `void` initializer",
                                Some(init_range),
                            ));
                            Type::Error
                        } else if init_ty.is_errorish() {
                            self.diagnostics.push(Diagnostic::error(
                                "var-cannot-infer",
                                "cannot infer `var` type from initializer",
                                Some(diag_span),
                            ));
                            init_ty
                        } else {
                            init_ty
                        }
                    }
                } else {
                    self.diagnostics.push(Diagnostic::error(
                        "var-requires-initializer",
                        "`var` declarations must have an initializer",
                        Some(diag_span),
                    ));
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-var",
                        "`var` local variables require an initializer",
                        Some(diag_span),
                    ));
                    Type::Error
                }
            } else if let Some(iterable) = self.local_foreach_iterables[local.idx()] {
                let iterable_ty = self.infer_expr(loader, iterable).ty;
                self.infer_foreach_element_type(loader, &iterable_ty)
            } else {
                // `var` is only a reserved type name in certain contexts; treat any other use
                // (e.g. catch parameters) as a normal type name.
                self.resolve_source_type(loader, data.ty_text.as_str(), Some(data.ty_range))
            }
        } else {
            self.resolve_source_type(loader, data.ty_text.as_str(), Some(data.ty_range))
        };

        self.local_types[local.idx()] = ty.clone();
        self.local_ty_states[local.idx()] = LocalTyState::Computed;
        ty
    }

    fn infer_expr(&mut self, loader: &mut ExternalTypeLoader<'_>, expr: HirExprId) -> ExprInfo {
        self.infer_expr_with_expected(loader, expr, None)
    }

    fn infer_expr_with_expected(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        expr: HirExprId,
        expected: Option<&Type>,
    ) -> ExprInfo {
        // Some expressions are target-typed: their type can depend on an expected type from the
        // surrounding context. If we previously inferred one of these expressions without an
        // expected target type, allow re-inference once a target type becomes known.
        if let Some(info) = self.expr_info[expr.idx()].clone() {
            let is_target_typed = matches!(
                self.body.exprs[expr],
                HirExpr::Lambda { .. }
                    | HirExpr::MethodReference { .. }
                    | HirExpr::ConstructorReference { .. }
                    | HirExpr::ArrayInitializer { .. }
            );
            let can_refine = expected.is_some() && is_target_typed && info.ty == Type::Unknown;
            if !can_refine {
                return info;
            }
        }
        self.tick();

        let info = match &self.body.exprs[expr] {
            HirExpr::Missing { .. } => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            HirExpr::Invalid { children, .. } => {
                for child in children {
                    let _ = self.infer_expr(loader, *child);
                }
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
            HirExpr::Literal { kind, value, range } => match kind {
                LiteralKind::Int => {
                    if let Err(err) = nova_syntax::parse_int_literal(value) {
                        let span = Span::new(
                            range.start.saturating_add(err.span.start),
                            range.start.saturating_add(err.span.end),
                        );
                        self.diagnostics.push(Diagnostic::error(
                            "invalid-literal",
                            err.message,
                            Some(span),
                        ));
                        ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        }
                    } else {
                        ExprInfo {
                            ty: Type::Primitive(PrimitiveType::Int),
                            is_type_ref: false,
                        }
                    }
                }
                LiteralKind::Long => {
                    if let Err(err) = nova_syntax::parse_long_literal(value) {
                        let span = Span::new(
                            range.start.saturating_add(err.span.start),
                            range.start.saturating_add(err.span.end),
                        );
                        self.diagnostics.push(Diagnostic::error(
                            "invalid-literal",
                            err.message,
                            Some(span),
                        ));
                        ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        }
                    } else {
                        ExprInfo {
                            ty: Type::Primitive(PrimitiveType::Long),
                            is_type_ref: false,
                        }
                    }
                }
                LiteralKind::Float => {
                    if let Err(err) = nova_syntax::parse_float_literal(value) {
                        let span = Span::new(
                            range.start.saturating_add(err.span.start),
                            range.start.saturating_add(err.span.end),
                        );
                        self.diagnostics.push(Diagnostic::error(
                            "invalid-literal",
                            err.message,
                            Some(span),
                        ));
                        ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        }
                    } else {
                        ExprInfo {
                            ty: Type::Primitive(PrimitiveType::Float),
                            is_type_ref: false,
                        }
                    }
                }
                LiteralKind::Double => {
                    if let Err(err) = nova_syntax::parse_double_literal(value) {
                        let span = Span::new(
                            range.start.saturating_add(err.span.start),
                            range.start.saturating_add(err.span.end),
                        );
                        self.diagnostics.push(Diagnostic::error(
                            "invalid-literal",
                            err.message,
                            Some(span),
                        ));
                        ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        }
                    } else {
                        ExprInfo {
                            ty: Type::Primitive(PrimitiveType::Double),
                            is_type_ref: false,
                        }
                    }
                }
                LiteralKind::Char => ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Char),
                    is_type_ref: false,
                },
                LiteralKind::String => ExprInfo {
                    ty: Type::class(loader.store.well_known().string, vec![]),
                    is_type_ref: false,
                },
                LiteralKind::TextBlock => ExprInfo {
                    ty: Type::class(loader.store.well_known().string, vec![]),
                    is_type_ref: false,
                },
                LiteralKind::Bool => ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Boolean),
                    is_type_ref: false,
                },
            },
            HirExpr::Null { .. } => ExprInfo {
                ty: Type::Null,
                is_type_ref: false,
            },
            HirExpr::This { range } => {
                if self.is_static_context() {
                    self.diagnostics.push(Diagnostic::error(
                        "this-in-static-context",
                        "cannot use `this` in a static context",
                        Some(*range),
                    ));
                    ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    }
                } else {
                    // Best-effort support for qualified `this` expressions (e.g. `Outer.this`).
                    //
                    // The HIR currently stores qualified `this` as a normal `This` expression with
                    // an extended span, so we recover the qualifier text from the source range.
                    ExprInfo {
                        ty: self
                            .resolve_qualified_this_super_qualifier_type(loader, *range, "this")
                            .or_else(|| self.enclosing_class_type(loader))
                            .unwrap_or(Type::Unknown),
                        is_type_ref: false,
                    }
                }
            }
            HirExpr::Super { range } => {
                if self.is_static_context() {
                    self.diagnostics.push(Diagnostic::error(
                        "super-in-static-context",
                        "cannot use `super` in a static context",
                        Some(*range),
                    ));
                    ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    }
                } else {
                    let object_ty = loader
                        .store
                        .lookup_class("java.lang.Object")
                        .map(|id| Type::class(id, vec![]))
                        .unwrap_or(Type::Unknown);
                    // Best-effort support for qualified `super` expressions (e.g. `Outer.super`).
                    // See note in the `this` arm above.
                    let base = self
                        .resolve_qualified_this_super_qualifier_type(loader, *range, "super")
                        .or_else(|| self.enclosing_class_type(loader))
                        .unwrap_or(Type::Unknown);
                    if base.is_errorish() {
                        return ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        };
                    }
                    self.ensure_type_loaded(loader, &base);
                    let ty = match base {
                        Type::Class(class_ty) => loader
                            .store
                            .class(class_ty.def)
                            .and_then(|def| def.super_class.clone())
                            .unwrap_or(object_ty),
                        _ => Type::Unknown,
                    };

                    ExprInfo {
                        ty,
                        is_type_ref: false,
                    }
                }
            }
            HirExpr::Name { name, range } => self.infer_name(loader, expr, name.as_str(), *range),
            HirExpr::FieldAccess {
                receiver,
                name,
                name_range,
                ..
            } => self.infer_field_access(loader, *receiver, name.as_str(), *name_range, expr),
            HirExpr::ArrayAccess {
                array,
                index,
                range,
            } => {
                let array_info = self.infer_expr(loader, *array);
                // Array type suffixes (e.g. `String[].class`) are lowered as `ArrayAccess` with a
                // missing index expression. If the base is a type reference, treat this as an
                // array *type* reference rather than an array access expression.
                if array_info.is_type_ref
                    && matches!(&self.body.exprs[*index], HirExpr::Missing { .. })
                {
                    ExprInfo {
                        ty: Type::Array(Box::new(array_info.ty)),
                        is_type_ref: true,
                    }
                } else {
                    let array_ty = array_info.ty;
                    let index_ty = self.infer_expr(loader, *index).ty;

                    match array_ty {
                        Type::Array(elem) => {
                            let index_prim = primitive_like(&*loader.store, &index_ty);
                            if !index_ty.is_errorish()
                                && !matches!(
                                    index_prim,
                                    Some(
                                        PrimitiveType::Byte
                                            | PrimitiveType::Short
                                            | PrimitiveType::Char
                                            | PrimitiveType::Int
                                    )
                                )
                            {
                                let found = format_type(&*loader.store, &index_ty);
                                self.diagnostics.push(Diagnostic::error(
                                    "invalid-array-index",
                                    format!(
                                        "array index must be an integral type (byte, short, char, or int), found {found}"
                                    ),
                                    Some(self.body.exprs[*index].range()),
                                ));
                            }

                            ExprInfo {
                                ty: elem.as_ref().clone(),
                                is_type_ref: false,
                            }
                        }
                        ty if ty.is_errorish() => ExprInfo {
                            ty,
                            is_type_ref: false,
                        },
                        ty => {
                            let found = format_type(&*loader.store, &ty);
                            self.diagnostics.push(Diagnostic::error(
                                "invalid-array-access",
                                format!("cannot index non-array type {found}"),
                                Some(*range),
                            ));
                            ExprInfo {
                                ty: Type::Error,
                                is_type_ref: false,
                            }
                        }
                    }
                }
            }
            HirExpr::MethodReference { receiver, name, .. } => {
                // Always infer the receiver so IDE hover works.
                let recv_info = self.infer_expr(loader, *receiver);

                let Some(expected) = expected else {
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                };

                self.ensure_type_loaded(loader, expected);
                let Some(sig) = ({
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    nova_types::infer_lambda_sam_signature(env_ro, expected)
                }) else {
                    self.diagnostics.push(Diagnostic::error(
                        "method-ref-without-target",
                        "cannot infer method reference type without a target functional interface",
                        Some(self.body.exprs[expr].range()),
                    ));
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                };

                // If the receiver is unknown/error, we can't validate the method reference. Fall
                // back to target typing without additional diagnostics.
                if recv_info.ty.is_errorish() {
                    return ExprInfo {
                        ty: expected.clone(),
                        is_type_ref: false,
                    };
                }

                self.ensure_type_loaded(loader, &recv_info.ty);
                for param in &sig.params {
                    self.ensure_type_loaded(loader, param);
                }
                self.ensure_type_loaded(loader, &sig.return_type);

                let env_ro: &dyn TypeEnv = &*loader.store;
                let matches_resolution = |resolution: &MethodResolution, require_instance: bool| {
                    let return_ok = |method: &ResolvedMethod| {
                        if require_instance && method.is_static {
                            return false;
                        }

                        if sig.return_type == Type::Void {
                            return true;
                        }
                        assignment_conversion(env_ro, &method.return_type, &sig.return_type)
                            .is_some()
                    };

                    match resolution {
                        MethodResolution::Found(method) => return_ok(method),
                        MethodResolution::Ambiguous(amb) => {
                            amb.candidates.iter().any(|cand| return_ok(cand))
                        }
                        MethodResolution::NotFound(_) => false,
                    }
                };

                let ok = if recv_info.is_type_ref {
                    // `TypeName::method` can refer to either a static method on `TypeName`, or an
                    // "unbound" instance method where the first SAM parameter becomes the receiver.
                    let static_call = MethodCall {
                        receiver: recv_info.ty.clone(),
                        call_kind: CallKind::Static,
                        name: name.as_str(),
                        args: sig.params.clone(),
                        expected_return: Some(sig.return_type.clone()),
                        explicit_type_args: vec![],
                    };
                    let mut ctx = TyContext::new(env_ro);
                    let static_res = nova_types::resolve_method_call(&mut ctx, &static_call);
                    let static_ok = matches_resolution(&static_res, false);

                    if static_ok {
                        true
                    } else if let Some((recv_param, rest)) = sig.params.split_first() {
                        if assignment_conversion(env_ro, recv_param, &recv_info.ty).is_none() {
                            false
                        } else {
                            let instance_call = MethodCall {
                                receiver: recv_info.ty.clone(),
                                call_kind: CallKind::Instance,
                                name: name.as_str(),
                                args: rest.to_vec(),
                                expected_return: Some(sig.return_type.clone()),
                                explicit_type_args: vec![],
                            };
                            let mut ctx = TyContext::new(env_ro);
                            let instance_res =
                                nova_types::resolve_method_call(&mut ctx, &instance_call);
                            matches_resolution(&instance_res, true)
                        }
                    } else {
                        false
                    }
                } else {
                    let call = MethodCall {
                        receiver: recv_info.ty.clone(),
                        call_kind: CallKind::Instance,
                        name: name.as_str(),
                        args: sig.params.clone(),
                        expected_return: Some(sig.return_type.clone()),
                        explicit_type_args: vec![],
                    };
                    let mut ctx = TyContext::new(env_ro);
                    let res = nova_types::resolve_method_call(&mut ctx, &call);
                    matches_resolution(&res, false)
                };

                if ok {
                    ExprInfo {
                        ty: expected.clone(),
                        is_type_ref: false,
                    }
                } else {
                    let expected_display = format_type(env_ro, expected);
                    self.diagnostics.push(Diagnostic::error(
                        "method-ref-mismatch",
                        format!(
                            "method reference is not compatible with target type {expected_display}"
                        ),
                        Some(self.body.exprs[expr].range()),
                    ));
                    ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    }
                }
            }
            HirExpr::ConstructorReference { receiver, .. } => {
                // Always infer the receiver so IDE hover works.
                let recv_info = self.infer_expr(loader, *receiver);

                let Some(expected) = expected else {
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                };

                self.ensure_type_loaded(loader, expected);
                let Some(sig) = ({
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    nova_types::infer_lambda_sam_signature(env_ro, expected)
                }) else {
                    self.diagnostics.push(Diagnostic::error(
                        "method-ref-without-target",
                        "cannot infer constructor reference type without a target functional interface",
                        Some(self.body.exprs[expr].range()),
                    ));
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                };

                // If the receiver is unknown/error, we can't validate the constructor reference.
                // Fall back to target typing without additional diagnostics.
                if recv_info.ty.is_errorish() {
                    return ExprInfo {
                        ty: expected.clone(),
                        is_type_ref: false,
                    };
                }

                self.ensure_type_loaded(loader, &recv_info.ty);
                for param in &sig.params {
                    self.ensure_type_loaded(loader, param);
                }
                self.ensure_type_loaded(loader, &sig.return_type);

                // Constructor references are only valid for type receivers. If we don't have a type
                // receiver (e.g. due to broken lowering), bail out without extra diagnostics.
                if !recv_info.is_type_ref {
                    return ExprInfo {
                        ty: expected.clone(),
                        is_type_ref: false,
                    };
                }

                let class_id = match &recv_info.ty {
                    Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
                    Type::Named(name) => self
                        .ensure_workspace_class(loader, name)
                        .or_else(|| loader.ensure_class(name)),
                    _ => None,
                };
                let Some(class_id) = class_id else {
                    return ExprInfo {
                        ty: expected.clone(),
                        is_type_ref: false,
                    };
                };

                let env_ro: &dyn TypeEnv = &*loader.store;
                let res = nova_types::resolve_constructor_call(
                    env_ro,
                    class_id,
                    &sig.params,
                    Some(&recv_info.ty),
                );

                let return_ok = |method: &ResolvedMethod| {
                    if sig.return_type == Type::Void {
                        return true;
                    }
                    assignment_conversion(env_ro, &method.return_type, &sig.return_type).is_some()
                };

                let ok = match &res {
                    MethodResolution::Found(method) => return_ok(method),
                    MethodResolution::Ambiguous(amb) => amb.candidates.iter().any(|m| return_ok(m)),
                    MethodResolution::NotFound(_) => false,
                };

                if ok {
                    ExprInfo {
                        ty: expected.clone(),
                        is_type_ref: false,
                    }
                } else {
                    let expected_display = format_type(env_ro, expected);
                    self.diagnostics.push(Diagnostic::error(
                        "method-ref-mismatch",
                        format!(
                            "constructor reference is not compatible with target type {expected_display}"
                        ),
                        Some(self.body.exprs[expr].range()),
                    ));
                    ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    }
                }
            }
            HirExpr::ClassLiteral { ty, range } => {
                let inner = self.infer_expr(loader, *ty);
                if !inner.is_type_ref {
                    ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    }
                } else {
                    // Best-effort: recover array suffixes from the source range.
                    //
                    // The parser preserves `[]` tokens inside the `NameExpression` used as the
                    // receiver for class literals (e.g. `String[].class`, `int[][].class`), but the
                    // lightweight AST/HIR currently drops those suffixes. Instead of returning
                    // `Class<String>` for `String[].class`, we count trailing `[]` pairs and apply
                    // them to the inferred base type.
                    let mut referenced = inner.ty.clone();
                    if !referenced.is_errorish() {
                        let dims_in_source = self.class_literal_array_dims(*range);
                        let (_, existing_dims) = peel_array_dims(&referenced);
                        let extra_dims = dims_in_source.saturating_sub(existing_dims);
                        for _ in 0..extra_dims {
                            referenced = Type::Array(Box::new(referenced));
                        }
                    }

                    // Java class literals have type `Class<T>` but `T` must be a *reference type*.
                    // Use an unbounded wildcard for primitives / `void` (`int.class` -> `Class<?>`).
                    let mut arg = referenced.clone();
                    if !arg.is_reference() {
                        arg = Type::Wildcard(WildcardBound::Unbounded);
                    }

                    match loader
                        .store
                        .lookup_class("java.lang.Class")
                        .or_else(|| loader.ensure_class("java.lang.Class"))
                    {
                        Some(class_id) => ExprInfo {
                            ty: Type::class(class_id, vec![arg]),
                            is_type_ref: false,
                        },
                        None => ExprInfo {
                            ty: Type::Unknown,
                            is_type_ref: false,
                        },
                    }
                }
            }
            HirExpr::Cast {
                ty_text,
                ty_range,
                expr: inner,
                range,
            } => {
                let to = self.resolve_source_type(loader, ty_text.as_str(), Some(*ty_range));
                let from = if to.is_errorish() {
                    self.infer_expr(loader, *inner).ty
                } else {
                    // A cast provides a target type for target-typed expressions like lambdas and
                    // method references. Use it as the expected type so we can infer lambda
                    // parameter types (and other target-typed behavior) correctly.
                    self.infer_expr_with_expected(loader, *inner, Some(&to)).ty
                };

                if from.is_errorish() || to.is_errorish() {
                    ExprInfo {
                        ty: to,
                        is_type_ref: false,
                    }
                } else {
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    match cast_conversion(env_ro, &from, &to) {
                        None => {
                            let from = format_type(env_ro, &from);
                            let to_display = format_type(env_ro, &to);
                            self.diagnostics.push(Diagnostic::error(
                                "invalid-cast",
                                format!("cannot cast from {from} to {to_display}"),
                                Some(*range),
                            ));
                            ExprInfo {
                                ty: Type::Error,
                                is_type_ref: false,
                            }
                        }
                        Some(conv) => {
                            // Surface unchecked/raw-cast warnings even though the cast typechecks.
                            for warning in conv.warnings {
                                if let TypeWarning::Unchecked(reason) = warning {
                                    self.emit_unchecked_warning(reason, *range);
                                }
                            }

                            ExprInfo {
                                ty: to,
                                is_type_ref: false,
                            }
                        }
                    }
                }
            }
            HirExpr::Call {
                callee,
                args,
                explicit_type_args,
                ..
            } => self.infer_call(loader, *callee, args, explicit_type_args, expr, expected),
            HirExpr::New {
                class,
                class_range,
                args,
                range,
            } => self.infer_new_expr(
                loader,
                expr,
                class.as_str(),
                *class_range,
                *range,
                args,
                expected,
            ),
            HirExpr::ArrayCreation {
                elem_ty_text,
                elem_ty_range,
                dim_exprs,
                extra_dims,
                initializer,
                ..
            } => {
                let elem_ty =
                    self.resolve_source_type(loader, elem_ty_text.as_str(), Some(*elem_ty_range));
                let total_dims = dim_exprs.len().saturating_add(*extra_dims);

                for dim_expr in dim_exprs {
                    let dim_ty = self.infer_expr(loader, *dim_expr).ty;
                    if dim_ty.is_errorish() {
                        continue;
                    }

                    let dim_prim = primitive_like(&*loader.store, &dim_ty);
                    let is_integral = matches!(
                        dim_prim,
                        Some(
                            PrimitiveType::Byte
                                | PrimitiveType::Short
                                | PrimitiveType::Char
                                | PrimitiveType::Int
                        )
                    );
                    if !is_integral {
                        let found = format_type(&*loader.store, &dim_ty);
                        self.diagnostics.push(Diagnostic::error(
                            "array-dimension-type",
                            format!(
                                "array dimension must be an integral type (byte, short, char, or int), found {found}"
                            ),
                            Some(self.body.exprs[*dim_expr].range()),
                        ));
                    }
                }

                if total_dims == 0 {
                    ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    }
                } else {
                    let mut ty = elem_ty;
                    for _ in 0..total_dims {
                        ty = Type::Array(Box::new(ty));
                    }

                    if let Some(init) = initializer {
                        match &self.body.exprs[*init] {
                            HirExpr::ArrayInitializer { .. } => {
                                let _ =
                                    self.infer_array_initializer_with_expected(loader, *init, &ty);
                            }
                            _ => {
                                let _ = self.infer_expr(loader, *init);
                            }
                        }
                    }

                    ExprInfo {
                        ty,
                        is_type_ref: false,
                    }
                }
            }
            HirExpr::ArrayInitializer { items, .. } => {
                // Array initializer braces (`{...}`) are only typeable when we have an expected
                // array type from context (e.g. `int[] a = {1,2};` or `new int[] {1,2}`).
                if let Some(expected) =
                    expected.filter(|ty| matches!(ty, Type::Array(_)) && !ty.is_errorish())
                {
                    self.infer_array_initializer_with_expected(loader, expr, expected)
                } else {
                    for item in items {
                        let _ = self.infer_expr(loader, *item);
                    }

                    self.report_invalid_array_initializer(expr);
                    ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    }
                }
            }
            HirExpr::Unary {
                op, expr: operand, ..
            } => {
                let diag_start = self.diagnostics.len();
                let operand_info = self.infer_expr(loader, *operand);
                let mut inner = operand_info.ty.clone();
                let span = self.body.exprs[expr].range();
                let operand_range = self.body.exprs[*operand].range();
                let operand_is_lvalue =
                    matches!(
                        op,
                        UnaryOp::PreInc | UnaryOp::PreDec | UnaryOp::PostInc | UnaryOp::PostDec
                    ) && self.is_lvalue_expr(loader, *operand, operand_info.is_type_ref);
                let env_ro: &dyn TypeEnv = &*loader.store;

                if matches!(op, UnaryOp::Minus) && inner.is_errorish() {
                    fn parse_decimal_u64(text: &str) -> Option<u64> {
                        let text: String = text.chars().filter(|c| *c != '_').collect();
                        if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
                            return None;
                        }
                        text.parse().ok()
                    }

                    let replacement = match &self.body.exprs[*operand] {
                        HirExpr::Literal {
                            kind: LiteralKind::Int,
                            value,
                            ..
                        } if parse_decimal_u64(value) == Some((i32::MAX as u64) + 1) => {
                            Some(Type::Primitive(PrimitiveType::Int))
                        }
                        HirExpr::Literal {
                            kind: LiteralKind::Long,
                            value,
                            ..
                        } => {
                            let digits = value
                                .strip_suffix('l')
                                .or_else(|| value.strip_suffix('L'))
                                .unwrap_or(value);
                            (parse_decimal_u64(digits) == Some((i64::MAX as u64) + 1))
                                .then_some(Type::Primitive(PrimitiveType::Long))
                        }
                        _ => None,
                    };

                    if let Some(ty) = replacement {
                        // Suppress literal out-of-range diagnostics for the special JLS case:
                        // `-2147483648` / `-9223372036854775808L`.
                        self.diagnostics.truncate(diag_start);
                        inner = ty.clone();
                        self.expr_info[operand.idx()] = Some(ExprInfo {
                            ty,
                            is_type_ref: false,
                        });
                    }
                }

                let inner_prim = primitive_like(env_ro, &inner);
                let ty = match op {
                    UnaryOp::Not => {
                        if !inner.is_errorish()
                            && assignment_conversion(env_ro, &inner, &Type::boolean()).is_none()
                        {
                            self.diagnostics.push(Diagnostic::error(
                                "invalid-unary-op",
                                "operator ! requires boolean operand",
                                Some(span),
                            ));
                        }
                        Type::boolean()
                    }
                    UnaryOp::Plus | UnaryOp::Minus => {
                        if inner.is_errorish() {
                            inner
                        } else if let Some(primitive) = inner_prim {
                            if primitive.is_numeric() {
                                Type::Primitive(unary_numeric_promotion(primitive))
                            } else {
                                self.diagnostics.push(Diagnostic::error(
                                    "invalid-unary-op",
                                    "operator +/- requires numeric operand",
                                    Some(span),
                                ));
                                Type::Error
                            }
                        } else {
                            self.diagnostics.push(Diagnostic::error(
                                "invalid-unary-op",
                                "operator +/- requires numeric operand",
                                Some(span),
                            ));
                            Type::Error
                        }
                    }
                    UnaryOp::BitNot => {
                        if inner.is_errorish() {
                            inner
                        } else if let Some(primitive) = inner_prim {
                            if is_integral_primitive(primitive) {
                                Type::Primitive(unary_numeric_promotion(primitive))
                            } else {
                                self.diagnostics.push(Diagnostic::error(
                                    "invalid-unary-op",
                                    "operator ~ requires integral operand",
                                    Some(span),
                                ));
                                Type::Error
                            }
                        } else {
                            self.diagnostics.push(Diagnostic::error(
                                "invalid-unary-op",
                                "operator ~ requires integral operand",
                                Some(span),
                            ));
                            Type::Error
                        }
                    }
                    UnaryOp::PreInc | UnaryOp::PreDec | UnaryOp::PostInc | UnaryOp::PostDec => {
                        if !operand_is_lvalue {
                            self.diagnostics.push(Diagnostic::error(
                                "invalid-lvalue",
                                "expression is not assignable",
                                Some(operand_range),
                            ));
                            Type::Error
                        } else if inner.is_errorish() {
                            inner
                        } else if let Some(primitive) = inner_prim {
                            if primitive.is_numeric() {
                                // JLS: the ++/-- expression has the type of its operand variable.
                                //
                                // The increment/decrement operation itself performs numeric
                                // promotion internally, but the expression result type does *not*
                                // undergo unary numeric promotion (`byte b; byte c = b++;` is
                                // valid Java).
                                inner
                            } else {
                                self.diagnostics.push(Diagnostic::error(
                                    "invalid-inc-dec",
                                    "increment/decrement requires a numeric operand",
                                    Some(operand_range),
                                ));
                                Type::Error
                            }
                        } else {
                            self.diagnostics.push(Diagnostic::error(
                                "invalid-inc-dec",
                                "increment/decrement requires a numeric operand",
                                Some(operand_range),
                            ));
                            Type::Error
                        }
                    }
                };
                ExprInfo {
                    ty,
                    is_type_ref: false,
                }
            }
            HirExpr::Binary { op, lhs, rhs, .. } => {
                self.infer_binary(loader, expr, *op, *lhs, *rhs)
            }
            HirExpr::Instanceof {
                expr: lhs_expr,
                ty_text,
                ty_range,
                range,
            } => {
                let lhs = self.infer_expr(loader, *lhs_expr).ty;
                let rhs = if ty_text.trim().is_empty() {
                    Type::Unknown
                } else {
                    self.resolve_source_type(loader, ty_text.as_str(), Some(*ty_range))
                };

                if rhs == Type::Void {
                    self.diagnostics.push(Diagnostic::error(
                        "instanceof-void",
                        "`void` is not a valid `instanceof` target",
                        Some(*ty_range),
                    ));
                } else if matches!(rhs, Type::Primitive(_)) {
                    self.diagnostics.push(Diagnostic::error(
                        "instanceof-invalid-type",
                        "`instanceof` requires a reference type",
                        Some(*ty_range),
                    ));
                }

                if !lhs.is_errorish() && matches!(lhs, Type::Primitive(_)) {
                    self.diagnostics.push(Diagnostic::error(
                        "instanceof-primitive",
                        "`instanceof` cannot be applied to a primitive expression",
                        Some(self.body.exprs[*lhs_expr].range()),
                    ));
                }

                let env_ro: &dyn TypeEnv = &*loader.store;
                if lhs.is_reference()
                    && rhs.is_reference()
                    && !lhs.is_errorish()
                    && !rhs.is_errorish()
                    && cast_conversion(env_ro, &lhs, &rhs).is_none()
                    && cast_conversion(env_ro, &rhs, &lhs).is_none()
                {
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-instanceof",
                        "invalid `instanceof` check between unrelated types",
                        Some(*range),
                    ));
                }
                ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Boolean),
                    is_type_ref: false,
                }
            }
            HirExpr::Assign { lhs, rhs, op, .. } => {
                let lhs_info = self.infer_expr(loader, *lhs);
                let lhs_range = self.body.exprs[*lhs].range();
                let is_lvalue = self.is_lvalue_expr(loader, *lhs, lhs_info.is_type_ref);

                let rhs_expected = if is_lvalue {
                    match op {
                        AssignOp::Assign if !lhs_info.ty.is_errorish() => Some(&lhs_info.ty),
                        _ => None,
                    }
                } else {
                    None
                };
                let rhs_info = self.infer_expr_with_expected(loader, *rhs, rhs_expected);

                if lhs_info.is_type_ref {
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-assignment-target",
                        "invalid assignment target: cannot assign to a type",
                        Some(lhs_range),
                    ));
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-lvalue",
                        "expression is not assignable",
                        Some(lhs_range),
                    ));
                    ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    }
                } else if !is_lvalue {
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-assignment-target",
                        "invalid assignment target",
                        Some(lhs_range),
                    ));
                    self.diagnostics.push(Diagnostic::error(
                        "invalid-lvalue",
                        "expression is not assignable",
                        Some(lhs_range),
                    ));
                    ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    }
                } else {
                    let lhs_ty = lhs_info.ty.clone();
                    let rhs_ty = rhs_info.ty.clone();

                    match *op {
                        AssignOp::Assign => {
                            if !lhs_ty.is_errorish() && !rhs_ty.is_errorish() {
                                let env_ro: &dyn TypeEnv = &*loader.store;
                                let const_value = const_value_for_expr(self.body, *rhs);
                                match assignment_conversion_with_const(
                                    env_ro,
                                    &rhs_ty,
                                    &lhs_ty,
                                    const_value,
                                ) {
                                    None => {
                                        let expected = format_type(env_ro, &lhs_ty);
                                        let found = format_type(env_ro, &rhs_ty);
                                        self.diagnostics.push(Diagnostic::error(
                                            "type-mismatch",
                                            format!(
                                                "type mismatch: expected {expected}, found {found}"
                                            ),
                                            Some(self.body.exprs[*rhs].range()),
                                        ));
                                    }
                                    Some(conv) => {
                                        for warning in conv.warnings {
                                            if let TypeWarning::Unchecked(reason) = warning {
                                                self.emit_unchecked_warning(
                                                    reason,
                                                    self.body.exprs[*rhs].range(),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            // Compound assignments: infer the operator result type, then validate the
                            // implicit cast back to the LHS type (JLS 15.26.2/15.26.3).
                            if !lhs_ty.is_errorish() && !rhs_ty.is_errorish() {
                                let env_ro: &dyn TypeEnv = &*loader.store;
                                let string_ty =
                                    Type::class(loader.store.well_known().string, vec![]);
                                let lhs_prim = primitive_like(env_ro, &lhs_ty);
                                let rhs_prim = primitive_like(env_ro, &rhs_ty);
                                let result_ty = match op {
                                    AssignOp::AddAssign => {
                                        // String concatenation is only defined for non-void
                                        // expressions.
                                        if rhs_ty == Type::Void || lhs_ty == Type::Void {
                                            None
                                        } else if is_java_lang_string(loader.store, &lhs_ty)
                                            || is_java_lang_string(loader.store, &rhs_ty)
                                        {
                                            Some(string_ty)
                                        } else if let (Some(a), Some(b)) = (lhs_prim, rhs_prim) {
                                            if a.is_numeric() && b.is_numeric() {
                                                binary_numeric_promotion(a, b).map(Type::Primitive)
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    }
                                    AssignOp::SubAssign
                                    | AssignOp::MulAssign
                                    | AssignOp::DivAssign
                                    | AssignOp::RemAssign => match (lhs_prim, rhs_prim) {
                                        (Some(a), Some(b)) if a.is_numeric() && b.is_numeric() => {
                                            binary_numeric_promotion(a, b).map(Type::Primitive)
                                        }
                                        _ => None,
                                    },
                                    AssignOp::AndAssign
                                    | AssignOp::OrAssign
                                    | AssignOp::XorAssign => match (lhs_prim, rhs_prim) {
                                        (
                                            Some(PrimitiveType::Boolean),
                                            Some(PrimitiveType::Boolean),
                                        ) => Some(Type::Primitive(PrimitiveType::Boolean)),
                                        (Some(a), Some(b))
                                            if is_integral_primitive(a)
                                                && is_integral_primitive(b) =>
                                        {
                                            binary_numeric_promotion(a, b).map(Type::Primitive)
                                        }
                                        _ => None,
                                    },
                                    AssignOp::ShlAssign
                                    | AssignOp::ShrAssign
                                    | AssignOp::UShrAssign => match (lhs_prim, rhs_prim) {
                                        (Some(lhs_p), Some(rhs_p))
                                            if is_integral_primitive(lhs_p)
                                                && is_integral_primitive(rhs_p) =>
                                        {
                                            Some(Type::Primitive(unary_numeric_promotion(lhs_p)))
                                        }
                                        _ => None,
                                    },
                                    AssignOp::Assign => None,
                                };

                                match result_ty {
                                    Some(result_ty) => {
                                        if cast_conversion(env_ro, &result_ty, &lhs_ty).is_none() {
                                            let expected = format_type(env_ro, &lhs_ty);
                                            let found = format_type(env_ro, &result_ty);
                                            self.diagnostics.push(Diagnostic::error(
                                                "type-mismatch",
                                                format!(
                                                    "type mismatch in compound assignment: expected {expected}, found {found}"
                                                ),
                                                Some(self.body.exprs[expr].range()),
                                            ));
                                        }
                                    }
                                    None => {
                                        let lhs_rendered = format_type(env_ro, &lhs_ty);
                                        let rhs_rendered = format_type(env_ro, &rhs_ty);
                                        self.diagnostics.push(Diagnostic::error(
                                            "type-mismatch",
                                            format!(
                                                "invalid operands for compound assignment: {lhs_rendered} and {rhs_rendered}"
                                            ),
                                            Some(self.body.exprs[expr].range()),
                                        ));
                                    }
                                }
                            }
                        }
                    }

                    // Java assignment expressions have the type of the LHS.
                    ExprInfo {
                        ty: lhs_ty,
                        is_type_ref: false,
                    }
                }
            }
            HirExpr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                let condition_ty = self.infer_expr(loader, *condition).ty;
                if !condition_ty.is_errorish() {
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    if assignment_conversion(env_ro, &condition_ty, &Type::boolean()).is_none() {
                        self.diagnostics.push(Diagnostic::error(
                            "condition-not-boolean",
                            "condition must be boolean",
                            Some(self.body.exprs[*condition].range()),
                        ));
                    }
                }
                let then_ty = self
                    .infer_expr_with_expected(loader, *then_expr, expected)
                    .ty;
                let else_ty = self
                    .infer_expr_with_expected(loader, *else_expr, expected)
                    .ty;
                let ty = if then_ty == else_ty {
                    then_ty
                } else if then_ty.is_errorish() {
                    // Preserve existing "errorish short-circuit": prefer the non-errorish branch
                    // when one side is unknown/error.
                    else_ty
                } else if else_ty.is_errorish() {
                    then_ty
                } else if matches!(then_ty, Type::Null) && else_ty.is_reference() {
                    // `cond ? ref : null` => ref
                    else_ty
                } else if matches!(else_ty, Type::Null) && then_ty.is_reference() {
                    // `cond ? null : ref` => ref
                    then_ty
                } else if matches!(then_ty, Type::Null) {
                    // `cond ? primitive : null` => boxed primitive (JLS 15.25).
                    //
                    // NOTE: This is intentionally only for the literal null type. If the other
                    // branch is a boxed primitive expression that happens to evaluate to null
                    // (e.g. `(Integer) null`), Java picks the unboxed primitive result and will
                    // throw NPE at runtime if that branch is taken.
                    let prim = {
                        let env_ro: &dyn TypeEnv = &*loader.store;
                        primitive_like(env_ro, &else_ty)
                    };
                    prim.map(|p| boxed_primitive_type(loader, p))
                        .unwrap_or(Type::Unknown)
                } else if matches!(else_ty, Type::Null) {
                    // `cond ? null : primitive` => boxed primitive (JLS 15.25).
                    let prim = {
                        let env_ro: &dyn TypeEnv = &*loader.store;
                        primitive_like(env_ro, &then_ty)
                    };
                    prim.map(|p| boxed_primitive_type(loader, p))
                        .unwrap_or(Type::Unknown)
                } else if let (Some(a), Some(b)) = {
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    (
                        primitive_like(env_ro, &then_ty),
                        primitive_like(env_ro, &else_ty),
                    )
                } {
                    // Best-effort: conditional expressions participate in unboxing + numeric
                    // promotion (JLS 15.25). We approximate this by treating boxed primitives as
                    // "primitive-like" and applying binary numeric promotion.
                    if a.is_numeric() && b.is_numeric() {
                        binary_numeric_promotion(a, b)
                            .map(Type::Primitive)
                            .unwrap_or(Type::Unknown)
                    } else if a == PrimitiveType::Boolean && b == PrimitiveType::Boolean {
                        Type::boolean()
                    } else {
                        Type::Unknown
                    }
                } else if then_ty.is_reference() && else_ty.is_reference() {
                    // Reference conditional result uses least-upper-bound of the two branches.
                    self.ensure_type_loaded(loader, &then_ty);
                    self.ensure_type_loaded(loader, &else_ty);
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    nova_types::lub(env_ro, &then_ty, &else_ty)
                } else {
                    Type::Unknown
                };

                ExprInfo {
                    ty,
                    is_type_ref: false,
                }
            }
            HirExpr::Switch { selector, arms, .. } => {
                let _ = self.infer_expr(loader, *selector);

                let mut yield_types = Vec::new();
                for arm in arms {
                    // Best-effort: type case label expressions too so IDE hover works.
                    for label in &arm.labels {
                        match label {
                            SwitchLabel::Case { values, .. } => {
                                for value in values {
                                    let _ = self.infer_expr(loader, *value);
                                }
                            }
                            SwitchLabel::Default { .. } => {}
                        }
                    }

                    match &arm.body {
                        SwitchArmBody::Expr(expr) => {
                            yield_types.push(self.infer_expr(loader, *expr).ty);
                        }
                        SwitchArmBody::Block(stmt) | SwitchArmBody::Stmt(stmt) => {
                            self.switch_yield_stack.push(Vec::new());
                            let expected_return = self.current_expected_return.clone();
                            self.check_stmt(loader, *stmt, &expected_return);
                            let yields = self.switch_yield_stack.pop().unwrap_or_default();
                            yield_types.extend(yields);
                        }
                    }
                }

                let env_ro: &dyn TypeEnv = &*loader.store;
                let ty = self.infer_switch_expr_type(
                    env_ro,
                    &yield_types,
                    self.body.exprs[expr].range(),
                );

                ExprInfo {
                    ty,
                    is_type_ref: false,
                }
            }
            HirExpr::Lambda { params, body, .. } => {
                let mut sam_return = Type::Unknown;
                let ty = if let Some(target) = expected {
                    // Best-effort: the lambda expression itself is typed as its target functional
                    // interface.
                    self.ensure_type_loaded(loader, target);
                    let env_ro: &dyn TypeEnv = &*loader.store;
                    if let Some(sig) = nova_types::infer_lambda_sam_signature(env_ro, target) {
                        sam_return = sig.return_type;
                        if sig.params.len() != params.len() {
                            self.diagnostics.push(Diagnostic::error(
                                "lambda-arity-mismatch",
                                format!(
                                    "lambda arity mismatch: expected {}, found {}",
                                    sig.params.len(),
                                    params.len()
                                ),
                                Some(self.body.exprs[expr].range()),
                            ));
                        }
                        for (param, ty) in params.iter().zip(sig.params.into_iter()) {
                            self.local_types[param.local.idx()] = ty;
                            // In demand-driven mode, locals are lazily inferred via
                            // `infer_local_type` unless marked computed.
                            self.local_ty_states[param.local.idx()] = LocalTyState::Computed;
                        }
                    }
                    target.clone()
                } else {
                    Type::Unknown
                };

                let prev_expected_return = self.current_expected_return.clone();
                self.current_expected_return = sam_return.clone();
                match body {
                    LambdaBody::Expr(expr_id) => {
                        // Expression-bodied lambdas:
                        // - For non-void SAM return types, the expression is a return value.
                        // - For void SAM return types, the expression must be "void-compatible"
                        //   (JLS 15.27.2): a statement expression whose value (if any) is ignored.
                        //
                        // Avoid propagating `void` as an expected type into call/generic inference:
                        // `void` is not a valid Java type argument and can lead to nonsensical
                        // inferences like `T = void` in best-effort mode.
                        let expected_body = (!sam_return.is_errorish() && sam_return != Type::Void)
                            .then_some(&sam_return);
                        let body_info = match expected_body {
                            Some(expected) => {
                                self.infer_expr_with_expected(loader, *expr_id, Some(expected))
                            }
                            None => self.infer_expr(loader, *expr_id),
                        };

                        if sam_return == Type::Void && !sam_return.is_errorish() {
                            // `Runnable r = () -> foo();` is OK (statement expression),
                            // but `Runnable r = () -> 1;` is not.
                            if !self.is_statement_expression(*expr_id)
                                && !body_info.ty.is_errorish()
                            {
                                let env_ro: &dyn TypeEnv = &*loader.store;
                                let found = format_type(env_ro, &body_info.ty);
                                self.diagnostics.push(Diagnostic::error(
                                    "return-mismatch",
                                    format!("return type mismatch: expected void, found {found}"),
                                    Some(self.body.exprs[*expr_id].range()),
                                ));
                            }
                        } else if sam_return != Type::Void
                            && !sam_return.is_errorish()
                            && !body_info.ty.is_errorish()
                        {
                            let env_ro: &dyn TypeEnv = &*loader.store;
                            match assignment_conversion_with_const(
                                env_ro,
                                &body_info.ty,
                                &sam_return,
                                const_value_for_expr(self.body, *expr_id),
                            ) {
                                None => {
                                    let expected = format_type(env_ro, &sam_return);
                                    let found = format_type(env_ro, &body_info.ty);
                                    self.diagnostics.push(Diagnostic::error(
                                        "return-mismatch",
                                        format!(
                                            "return type mismatch: expected {expected}, found {found}"
                                        ),
                                        Some(self.body.exprs[*expr_id].range()),
                                    ));
                                }
                                Some(conv) => {
                                    for warning in conv.warnings {
                                        if let TypeWarning::Unchecked(reason) = warning {
                                            self.emit_unchecked_warning(
                                                reason,
                                                self.body.exprs[*expr_id].range(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    LambdaBody::Block(stmt) => {
                        let expected_return = sam_return.clone();
                        self.check_stmt(loader, *stmt, &expected_return);
                    }
                }
                self.current_expected_return = prev_expected_return;
                ExprInfo {
                    ty,
                    is_type_ref: false,
                }
            }
        };

        self.expr_info[expr.idx()] = Some(info.clone());
        info
    }

    fn infer_foreach_element_type(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        iterable_ty: &Type,
    ) -> Type {
        match iterable_ty {
            Type::Array(elem) => (**elem).clone(),
            other => {
                self.ensure_type_loaded(loader, other);

                let Some(iterable_def) = self
                    .ensure_workspace_class(loader, "java.lang.Iterable")
                    .or_else(|| loader.ensure_class("java.lang.Iterable"))
                else {
                    return Type::Unknown;
                };

                let env_ro: &dyn TypeEnv = &*loader.store;
                let Some(args) = nova_types::instantiate_supertype(env_ro, other, iterable_def)
                else {
                    return Type::Unknown;
                };

                if let Some(first) = args.first() {
                    first.clone()
                } else {
                    Type::class(env_ro.well_known().object, vec![])
                }
            }
        }
    }

    fn new_expr_array_dims(&self, class_range: Span, expr_range: Span) -> Option<usize> {
        if class_range.end > expr_range.end {
            return None;
        }

        let file = def_file(self.owner);
        let text = self.db.file_content(file);
        let bytes = text.as_bytes();
        if expr_range.end > bytes.len() {
            return None;
        }

        let mut i = class_range.end;
        while i < expr_range.end && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        // `new Foo()` => not an array creation expression.
        if i >= expr_range.end || bytes[i] != b'[' {
            return None;
        }

        // Count top-level bracket groups (`[<expr>]` / `[]`) after the type name, but stop once we
        // hit an array initializer (`{ ... }`) so we don't count `[` that appear inside initializer
        // expressions.
        let mut dims = 0usize;
        let mut nesting = 0usize;
        while i < expr_range.end {
            match bytes[i] {
                b'[' => {
                    if nesting == 0 {
                        dims += 1;
                    }
                    nesting += 1;
                }
                b']' => {
                    nesting = nesting.saturating_sub(1);
                }
                b'{' | b'(' => {
                    if nesting == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        (dims > 0).then_some(dims)
    }

    fn infer_new_expr(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        expr: HirExprId,
        class_text: &str,
        class_range: Span,
        expr_range: Span,
        args: &[HirExprId],
        expected: Option<&Type>,
    ) -> ExprInfo {
        let arg_types = |this: &mut Self, loader: &mut ExternalTypeLoader<'_>| -> Vec<Type> {
            args.iter()
                .map(|arg| match &this.body.exprs[*arg] {
                    // Target-typed expressions need the constructor parameter types to infer
                    // correctly; avoid inferring them eagerly to prevent spurious diagnostics (and
                    // allow diamond inference to use the parameter target type).
                    HirExpr::Lambda { .. } => Type::Unknown,
                    HirExpr::MethodReference { receiver, .. }
                    | HirExpr::ConstructorReference { receiver, .. } => {
                        let _ = this.infer_expr(loader, *receiver);
                        Type::Unknown
                    }
                    // Best-effort poly-expression support: certain expressions (notably generic
                    // invocations like `Collections.emptyList()` and diamond class instance
                    // creation like `new ArrayList<>()`) can depend on the *target type* for
                    // inference. When they appear as arguments, we don't yet know the parameter
                    // target type until overload resolution succeeds.
                    //
                    // To avoid caching a non-target-typed result (e.g. `List<Object>`), we:
                    // - treat diamond `new` as `<unknown>` up-front
                    // - treat *generic* method invocations with placeholder arguments (`null`,
                    //   unknown/error) as `<unknown>` so we can later re-infer them once we know the
                    //   selected parameter type.
                    HirExpr::Call {
                        callee,
                        args: inner_args,
                        explicit_type_args: inner_type_args,
                        ..
                    } if inner_type_args.is_empty() => {
                        // Fast path: only bother with the poly-expression heuristic when the call's
                        // arguments provide weak/no constraints (no args or placeholder-ish args).
                        //
                        // Otherwise, generic inference from the arguments alone is usually
                        // sufficient and we can infer the call normally to help disambiguate
                        // overloads (e.g. `take(singletonList(\"x\"))`).
                        let mut inner_arg_tys = Vec::with_capacity(inner_args.len());
                        let mut has_placeholder_arg = inner_args.is_empty();
                        for inner_arg in inner_args {
                            let ty = match &this.body.exprs[*inner_arg] {
                                HirExpr::Lambda { .. } => {
                                    has_placeholder_arg = true;
                                    Type::Unknown
                                }
                                HirExpr::MethodReference { receiver, .. }
                                | HirExpr::ConstructorReference { receiver, .. } => {
                                    has_placeholder_arg = true;
                                    let _ = this.infer_expr(loader, *receiver);
                                    Type::Unknown
                                }
                                HirExpr::Null { .. } => {
                                    has_placeholder_arg = true;
                                    Type::Null
                                }
                                HirExpr::Missing { .. } => {
                                    has_placeholder_arg = true;
                                    Type::Unknown
                                }
                                HirExpr::New { class, .. }
                                    if is_diamond_type_ref_text(class.as_str()) =>
                                {
                                    has_placeholder_arg = true;
                                    Type::Unknown
                                }
                                _ => {
                                    let ty = this.infer_expr(loader, *inner_arg).ty;
                                    if ty.is_errorish() || ty == Type::Null {
                                        has_placeholder_arg = true;
                                    }
                                    ty
                                }
                            };
                            inner_arg_tys.push(ty);
                        }

                        if !has_placeholder_arg {
                            return this.infer_expr(loader, *arg).ty;
                        }

                        // For now, restrict the generic/poly heuristic to qualified calls like
                        // `Collections.emptyList()` where we can cheaply recover the receiver type
                        // and check whether the invoked method is actually generic.
                        let HirExpr::FieldAccess { receiver, name, .. } = &this.body.exprs[*callee]
                        else {
                            return this.infer_expr(loader, *arg).ty;
                        };

                        let recv_info = this.infer_expr(loader, *receiver);
                        let recv_ty = recv_info.ty.clone();
                        if recv_ty.is_errorish() {
                            return Type::Unknown;
                        }
                        this.ensure_type_loaded(loader, &recv_ty);

                        let call_kind = if recv_info.is_type_ref {
                            CallKind::Static
                        } else {
                            CallKind::Instance
                        };

                        let call = MethodCall {
                            receiver: recv_ty,
                            call_kind,
                            name: name.as_str(),
                            args: inner_arg_tys,
                            expected_return: None,
                            explicit_type_args: Vec::new(),
                        };

                        let env_ro: &dyn TypeEnv = &*loader.store;
                        let mut ctx = TyContext::new(env_ro);
                        match nova_types::resolve_method_call(&mut ctx, &call) {
                            MethodResolution::Found(method)
                                if !method.inferred_type_args.is_empty() =>
                            {
                                Type::Unknown
                            }
                            _ => this.infer_expr(loader, *arg).ty,
                        }
                    }
                    HirExpr::New { class, .. } if is_diamond_type_ref_text(class.as_str()) => {
                        Type::Unknown
                    }
                    _ => this.infer_expr(loader, *arg).ty,
                })
                .collect()
        };
        let apply_arg_targets =
            |this: &mut Self, loader: &mut ExternalTypeLoader<'_>, method: &ResolvedMethod| {
                for (arg, param_ty) in args.iter().zip(method.params.iter()) {
                    // Target-typed expressions like lambdas and method references may need the full
                    // functional interface definition (SAM) available. Ensure the parameter type is
                    // loaded before attempting target typing.
                    this.ensure_type_loaded(loader, param_ty);
                    let _ = this.infer_expr_with_expected(loader, *arg, Some(param_ty));
                }
            };
        let arg_types = arg_types(self, loader);

        let raw_text = class_text.trim();
        let used_diamond = is_diamond_type_ref_text(raw_text);
        let resolved_text = if used_diamond {
            let lt = raw_text.rfind('<').unwrap_or(raw_text.len());
            raw_text[..lt].trim_end()
        } else {
            raw_text
        };

        // Resolve the class type. When diamond is used we strip the `<>` so the
        // type-ref parser doesn't emit `invalid-type-ref` (it expects at least one
        // type argument).
        let mut class_ty = self.resolve_source_type(loader, resolved_text, Some(class_range));

        // Array creation expressions use the same HIR node as class instantiation.
        // Best-effort: recover array-ness for `new T[0]` expressions when the lowered type
        // text only contains the base element type.
        if matches!(class_ty, Type::Array(_)) {
            return ExprInfo {
                ty: class_ty,
                is_type_ref: false,
            };
        }
        if let Some(dims) = self.new_expr_array_dims(class_range, expr_range) {
            for _ in 0..dims {
                class_ty = Type::Array(Box::new(class_ty));
            }
            return ExprInfo {
                ty: class_ty,
                is_type_ref: false,
            };
        }

        // Best-effort: ensure external classes are loaded so constructors are available.
        self.ensure_type_loaded(loader, &class_ty);

        let class_id = match &class_ty {
            Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
            Type::Named(name) => self
                .ensure_workspace_class(loader, name)
                .or_else(|| loader.ensure_class(name)),
            _ => None,
        };

        let expected_target = expected.filter(|ty| !ty.is_errorish());
        if let Some(expected_target) = expected_target {
            self.ensure_type_loaded(loader, expected_target);
        }

        // Compute the instantiated type for the `new` expression.
        let receiver_ty = match (class_id, &class_ty) {
            (Some(def), _) if used_diamond => {
                let env_ro: &dyn TypeEnv = &*loader.store;
                let inferred = infer_diamond_type_args(env_ro, def, expected_target);
                Type::class(def, inferred)
            }
            (Some(def), Type::Class(nova_types::ClassType { args, .. })) => {
                Type::class(def, args.clone())
            }
            (Some(def), _) => Type::class(def, vec![]),
            (None, _) => class_ty.clone(),
        };

        // Resolve the constructor call and emit diagnostics.
        if let Some(def) = class_id {
            let env_ro: &dyn TypeEnv = &*loader.store;
            let expected_for_call = Some(&receiver_ty);
            match nova_types::resolve_constructor_call(env_ro, def, &arg_types, expected_for_call) {
                MethodResolution::Found(method) => {
                    self.call_resolutions[expr.idx()] = Some(method.clone());
                    apply_arg_targets(self, loader, &method);
                }
                MethodResolution::Ambiguous(amb) => {
                    self.diagnostics.push(self.ambiguous_constructor_diag(
                        env_ro,
                        def,
                        &amb.candidates,
                        self.body.exprs[expr].range(),
                    ));
                    if let Some(first) = amb.candidates.first() {
                        self.call_resolutions[expr.idx()] = Some(first.clone());
                        apply_arg_targets(self, loader, first);
                    }
                }
                MethodResolution::NotFound(not_found) => {
                    self.diagnostics.push(self.unresolved_constructor_diag(
                        env_ro,
                        def,
                        &not_found,
                        self.body.exprs[expr].range(),
                    ));
                }
            }
        }

        ExprInfo {
            ty: receiver_ty,
            is_type_ref: false,
        }
    }

    fn report_invalid_array_initializer(&mut self, expr: HirExprId) {
        if self.expr_info[expr.idx()].is_some() {
            return;
        }

        self.diagnostics.push(Diagnostic::error(
            "invalid-array-initializer",
            "array initializer can only be used as part of an array variable declaration or array creation",
            Some(self.body.exprs[expr].range()),
        ));
        self.expr_info[expr.idx()] = Some(ExprInfo {
            ty: Type::Unknown,
            is_type_ref: false,
        });
    }

    fn is_poly_expression(&self, expr: HirExprId) -> bool {
        match &self.body.exprs[expr] {
            HirExpr::Lambda { .. }
            | HirExpr::MethodReference { .. }
            | HirExpr::ConstructorReference { .. } => true,
            HirExpr::Conditional {
                then_expr,
                else_expr,
                ..
            } => self.is_poly_expression(*then_expr) || self.is_poly_expression(*else_expr),
            _ => false,
        }
    }

    fn infer_array_initializer_with_expected(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        expr: HirExprId,
        expected: &Type,
    ) -> ExprInfo {
        let Type::Array(elem_ty) = expected else {
            // Walk the initializer as an invalid expression for best-effort diagnostics.
            let _ = self.infer_expr(loader, expr);
            return ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            };
        };

        // Cache the type early so recursive initializers can reference each other without
        // producing spurious `invalid-array-initializer` diagnostics.
        let info = ExprInfo {
            ty: expected.clone(),
            is_type_ref: false,
        };
        self.expr_info[expr.idx()] = Some(info.clone());

        let HirExpr::ArrayInitializer { items, .. } = &self.body.exprs[expr] else {
            return info;
        };

        for &item in items {
            let expected_elem = elem_ty.as_ref();

            // Nested initializer: only valid when the element type is itself an array type.
            if matches!(self.body.exprs[item], HirExpr::ArrayInitializer { .. }) {
                if matches!(expected_elem, Type::Array(_)) {
                    let _ = self.infer_array_initializer_with_expected(loader, item, expected_elem);
                    continue;
                }

                let env_ro: &dyn TypeEnv = &*loader.store;
                let expected = format_type(env_ro, expected_elem);
                self.diagnostics.push(Diagnostic::error(
                    "array-initializer-type-mismatch",
                    format!(
                        "array initializer element type mismatch: expected {expected}, found array initializer"
                    ),
                    Some(self.body.exprs[item].range()),
                ));
                // Avoid follow-up `invalid-array-initializer` for the nested braces.
                self.expr_info[item.idx()] = Some(ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                });
                continue;
            }

            let item_ty = if expected_elem.is_errorish() || expected_elem == &Type::Void {
                self.infer_expr(loader, item).ty
            } else {
                self.infer_expr_with_expected(loader, item, Some(expected_elem))
                    .ty
            };
            if item_ty.is_errorish() || expected_elem.is_errorish() {
                continue;
            }

            let env_ro: &dyn TypeEnv = &*loader.store;
            if assignment_conversion_with_const(
                env_ro,
                &item_ty,
                expected_elem,
                const_value_for_expr(self.body, item),
            )
            .is_none()
            {
                let expected = format_type(env_ro, expected_elem);
                let found = format_type(env_ro, &item_ty);
                self.diagnostics.push(Diagnostic::error(
                    "array-initializer-type-mismatch",
                    format!(
                        "array initializer element type mismatch: expected {expected}, found {found}"
                    ),
                    Some(self.body.exprs[item].range()),
                ));
            }
        }

        info
    }

    fn infer_switch_expr_type(&mut self, env: &dyn TypeEnv, yields: &[Type], range: Span) -> Type {
        let Some(first) = yields.first() else {
            self.diagnostics.push(Diagnostic::error(
                "switch-type",
                "cannot infer switch expression type (no yielded values found)",
                Some(range),
            ));
            return Type::Unknown;
        };

        if yields.iter().all(|t| t == first) {
            return first.clone();
        }

        if yields
            .iter()
            .all(|t| matches!(t, Type::Primitive(p) if p.is_numeric()))
        {
            let mut promoted = match first {
                Type::Primitive(p) => *p,
                _ => PrimitiveType::Int,
            };
            for ty in yields.iter().skip(1) {
                let Type::Primitive(p) = ty else { continue };
                promoted = binary_numeric_promotion(promoted, *p).unwrap_or(promoted);
            }
            return Type::Primitive(promoted);
        }

        if yields
            .iter()
            .all(|t| t.is_reference() || matches!(t, Type::Null))
        {
            let mut acc = first.clone();
            for ty in yields.iter().skip(1) {
                acc = lub(env, &acc, ty);
            }
            return acc;
        }

        self.diagnostics.push(Diagnostic::error(
            "switch-type",
            "cannot infer switch expression type",
            Some(range),
        ));
        Type::Unknown
    }

    fn infer_name(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        expr: HirExprId,
        name: &str,
        range: Span,
    ) -> ExprInfo {
        // Primitive types can appear in expression position in a few places (notably `int.class`).
        // Treat them as type references so downstream passes (e.g. class literal typing) can
        // behave sensibly.
        match name {
            "boolean" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Boolean),
                    is_type_ref: true,
                };
            }
            "byte" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Byte),
                    is_type_ref: true,
                };
            }
            "short" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Short),
                    is_type_ref: true,
                };
            }
            "char" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Char),
                    is_type_ref: true,
                };
            }
            "int" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Int),
                    is_type_ref: true,
                };
            }
            "long" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Long),
                    is_type_ref: true,
                };
            }
            "float" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Float),
                    is_type_ref: true,
                };
            }
            "double" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Double),
                    is_type_ref: true,
                };
            }
            "void" => {
                return ExprInfo {
                    ty: Type::Void,
                    is_type_ref: true,
                };
            }
            _ => {}
        }

        match name {
            "null" => {
                return ExprInfo {
                    ty: Type::Null,
                    is_type_ref: false,
                }
            }
            "true" | "false" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Boolean),
                    is_type_ref: false,
                }
            }
            // Primitive type keywords can appear in expression position only in a narrow set of
            // contexts (notably class literals / array constructor references, e.g. `int.class`,
            // `int[]::new`). The parser lowers these as `Name` expressions for resilience; treat
            // them as *type references* to avoid spurious `unresolved-name` diagnostics.
            "boolean" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Boolean),
                    is_type_ref: true,
                }
            }
            "byte" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Byte),
                    is_type_ref: true,
                }
            }
            "short" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Short),
                    is_type_ref: true,
                }
            }
            "char" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Char),
                    is_type_ref: true,
                }
            }
            "int" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Int),
                    is_type_ref: true,
                }
            }
            "long" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Long),
                    is_type_ref: true,
                }
            }
            "float" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Float),
                    is_type_ref: true,
                }
            }
            "double" => {
                return ExprInfo {
                    ty: Type::Primitive(PrimitiveType::Double),
                    is_type_ref: true,
                }
            }
            "void" => {
                return ExprInfo {
                    ty: Type::Void,
                    is_type_ref: true,
                }
            }
            _ => {}
        }

        let scope = self
            .expr_scopes
            .scope_for_expr(expr)
            .unwrap_or_else(|| self.expr_scopes.root_scope());
        let resolved = self.expr_scopes.resolve_name(scope, &Name::from(name));
        if let Some(resolved) = resolved {
            match resolved {
                ResolvedLocal::Local(local) => {
                    return ExprInfo {
                        ty: self.infer_local_type(loader, local),
                        is_type_ref: false,
                    };
                }
                ResolvedLocal::Param(param) => {
                    let idx = param.index as usize;
                    return ExprInfo {
                        ty: self.param_types.get(idx).cloned().unwrap_or(Type::Unknown),
                        is_type_ref: false,
                    };
                }
            }
        }

        match self
            .resolver
            .resolve_name_detailed(self.scopes, self.scope_id, &Name::from(name))
        {
            NameResolution::Resolved(res) => self.resolution_to_expr(loader, res, range),
            NameResolution::Ambiguous(_) => {
                self.diagnostics.push(Diagnostic::error(
                    "ambiguous-name",
                    format!("ambiguous reference `{name}`"),
                    Some(range),
                ));
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
            NameResolution::Unresolved => {
                self.diagnostics.push(Diagnostic::error(
                    "unresolved-name",
                    format!("unresolved reference `{name}`"),
                    Some(range),
                ));
                ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                }
            }
        }
    }

    fn resolution_to_expr(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        res: Resolution,
        range: Span,
    ) -> ExprInfo {
        match res {
            Resolution::Local(_) | Resolution::Parameter(_) => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            Resolution::Field(field) => {
                let tree = self.db.hir_item_tree(field.file);
                let Some(field_def) = tree.fields.get(&field.ast_id) else {
                    // Broken HIR / invalid `FieldId` shouldn't crash type checking.
                    return ExprInfo {
                        ty: self
                            .field_types
                            .get(&field)
                            .cloned()
                            .unwrap_or(Type::Unknown),
                        is_type_ref: false,
                    };
                };

                // Enum constants are implicitly `static final`, and interface fields are implicitly
                // `public static final`. Model both here so static-context diagnostics don't fire
                // for valid references like `A` inside an enum's static method.
                let mut is_static = field_def.kind == FieldKind::EnumConstant
                    || field_def.modifiers.raw & Modifiers::STATIC != 0;
                if !is_static {
                    if let Some(owner) = self.field_owners.get(&field).cloned() {
                        let id = loader
                            .store
                            .lookup_class(&owner)
                            .or_else(|| self.ensure_workspace_class(loader, &owner))
                            .or_else(|| loader.ensure_class(&owner));
                        if let Some(id) = id {
                            is_static = loader
                                .store
                                .class(id)
                                .is_some_and(|def| def.kind == ClassKind::Interface);
                        }
                    }
                }
                if !is_static {
                    // Unqualified field references implicitly use `this` (or an enclosing
                    // `Outer.this`). If there is no suitable implicit instance receiver, reject the
                    // reference with the standard static-context diagnostic.
                    let invalid = self.is_static_context()
                        || self
                            .field_owners
                            .get(&field)
                            .is_some_and(|owner| !self.has_enclosing_instance_of(owner));
                    if invalid {
                        self.diagnostics.push(Diagnostic::error(
                            "static-context",
                            format!(
                                "cannot reference instance field `{}` from a static context",
                                field_def.name
                            ),
                            Some(range),
                        ));
                        return ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        };
                    }
                }

                ExprInfo {
                    ty: self
                        .field_types
                        .get(&field)
                        .cloned()
                        .unwrap_or(Type::Unknown),
                    is_type_ref: false,
                }
            }
            Resolution::Methods(_) | Resolution::Constructors(_) => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            Resolution::Type(ty) => {
                let binary_name = match ty {
                    TypeResolution::External(name) => name.as_str().to_string(),
                    TypeResolution::Source(item) => {
                        let project = self.db.file_project(def_file(self.owner));
                        let workspace = self.db.workspace_def_map(project);
                        if let Some(name) = workspace.type_name(item) {
                            name.as_str().to_string()
                        } else if let Some(name) =
                            self.db.scope_graph(item.file()).scopes.type_name(item)
                        {
                            name.as_str().to_string()
                        } else {
                            "<unknown>".to_string()
                        }
                    }
                };

                let id = self
                    .ensure_workspace_class(loader, &binary_name)
                    .or_else(|| loader.ensure_class(&binary_name));
                if let Some(id) = id {
                    ExprInfo {
                        ty: Type::class(id, vec![]),
                        is_type_ref: true,
                    }
                } else {
                    ExprInfo {
                        ty: Type::Named(binary_name),
                        is_type_ref: true,
                    }
                }
            }
            Resolution::Package(_) => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
            Resolution::StaticMember(member) => self.static_member_to_expr(loader, member, range),
        }
    }

    fn static_member_to_expr(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        member: StaticMemberResolution,
        range: Span,
    ) -> ExprInfo {
        if let StaticMemberResolution::SourceField(field) = member {
            // Static field imports carry the stable `FieldId`; prefer the already-resolved declared
            // type over name-based lookup.
            let ty = self
                .field_types
                .get(&field)
                .cloned()
                .unwrap_or(Type::Unknown);
            return ExprInfo {
                ty,
                is_type_ref: false,
            };
        }

        let (owner, name) = match member {
            StaticMemberResolution::External(id) => match id.as_str().split_once("::") {
                Some((owner, name)) => (owner.to_string(), name.to_string()),
                None => {
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                }
            },
            StaticMemberResolution::SourceField(_) => {
                return ExprInfo {
                    ty: Type::Unknown,
                    is_type_ref: false,
                };
            }
            StaticMemberResolution::SourceMethod(method) => {
                let Some(owner) = self.method_owners.get(&method).cloned() else {
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                };
                let tree = self.db.hir_item_tree(method.file);
                let Some(method_def) = tree.methods.get(&method.ast_id) else {
                    return ExprInfo {
                        ty: Type::Unknown,
                        is_type_ref: false,
                    };
                };
                let name = method_def.name.clone();
                (owner, name)
            }
        };

        let receiver = self
            .ensure_workspace_class(loader, &owner)
            .or_else(|| loader.ensure_class(&owner))
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| Type::Named(owner.to_string()));
        self.ensure_type_loaded(loader, &receiver);

        {
            let env_ro: &dyn TypeEnv = &*loader.store;
            let mut ctx = TyContext::new(env_ro);
            if let Some(field) = ctx.resolve_field(&receiver, &name, CallKind::Static) {
                return ExprInfo {
                    ty: field.ty,
                    is_type_ref: false,
                };
            }
        }

        self.diagnostics.push(Diagnostic::error(
            "unresolved-static-member",
            format!("unresolved static member `{owner}::{name}`"),
            Some(range),
        ));
        ExprInfo {
            ty: Type::Unknown,
            is_type_ref: false,
        }
    }

    fn infer_field_access(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        receiver: HirExprId,
        name: &str,
        name_range: Span,
        expr: HirExprId,
    ) -> ExprInfo {
        // Fast path: interpret `a.b.c` style `FieldAccess` chains as fully qualified type names in
        // expression position (e.g. `java.lang.String.valueOf(1)`).
        //
        // Important: do this *before* inferring the receiver expression so intermediate package
        // segments don't emit bogus `unresolved-field` diagnostics.
        if let Some(mut segments) = self.collect_name_or_field_chain_segments(receiver) {
            segments.push(name.to_string());

            if let Some(first) = segments.first() {
                // Guard against misinterpreting value field accesses: if the leftmost segment is a
                // local/param/field, treat the chain as a normal value expression.
                let scope = self
                    .expr_scopes
                    .scope_for_expr(expr)
                    .unwrap_or_else(|| self.expr_scopes.root_scope());
                let first_name = Name::from(first.as_str());
                let first_is_local_or_param =
                    self.expr_scopes.resolve_name(scope, &first_name).is_some();
                let first_resolution =
                    self.resolver
                        .resolve_name_detailed(self.scopes, self.scope_id, &first_name);
                let first_is_field = matches!(
                    &first_resolution,
                    NameResolution::Resolved(Resolution::Field(_))
                );
                let first_is_type = matches!(
                    &first_resolution,
                    NameResolution::Resolved(Resolution::Type(_))
                );

                if !first_is_local_or_param && !first_is_field && !first_is_type {
                    let q = QualifiedName::from_dotted(&segments.join("."));
                    if let Some(resolved) = self.resolver.resolve_qualified_type_in_scope(
                        self.scopes,
                        self.scope_id,
                        &q,
                    ) {
                        let binary_name = resolved.as_str().to_string();
                        // Preserve workspace defs: a fully-qualified name expression can collide
                        // with a classpath type of the same binary name. If we call
                        // `ExternalTypeLoader::ensure_class` directly, it can overwrite an
                        // already-defined workspace `ClassDef` with the external stub.
                        if let Some(id) = self
                            .ensure_workspace_class(loader, &binary_name)
                            .or_else(|| loader.ensure_class(&binary_name))
                        {
                            return ExprInfo {
                                ty: Type::class(id, vec![]),
                                is_type_ref: true,
                            };
                        }

                        return ExprInfo {
                            ty: Type::Named(binary_name),
                            is_type_ref: true,
                        };
                    }
                }
            }
        }

        let recv_info = self.infer_expr(loader, receiver);
        let recv_ty = recv_info.ty.clone();

        if recv_ty == Type::Error {
            return ExprInfo {
                ty: Type::Error,
                is_type_ref: false,
            };
        }

        // Best-effort array `length` support.
        if !recv_info.is_type_ref && matches!(recv_ty, Type::Array(_)) && name == "length" {
            return ExprInfo {
                ty: Type::Primitive(PrimitiveType::Int),
                is_type_ref: false,
            };
        }

        self.ensure_type_loaded(loader, &recv_ty);

        if recv_info.is_type_ref {
            // Qualified `this` / `super` (e.g. `Outer.this`, `Outer.super`) are represented as field
            // accesses in the HIR. Treat them as value expressions rather than static member lookups.
            //
            // Note: `this`/`super` cannot appear as valid identifiers, so this is safe to
            // disambiguate from normal field access.
            match name {
                "this" => {
                    if self.is_static_context() {
                        let span = if !name_range.is_empty() {
                            Some(name_range)
                        } else {
                            Some(self.body.exprs[expr].range())
                        };
                        self.diagnostics.push(Diagnostic::error(
                            "this-in-static-context",
                            "cannot use `this` in a static context",
                            span,
                        ));
                        return ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        };
                    }

                    if let Some(owner) = type_binary_name(loader.store, &recv_ty) {
                        if !self.has_enclosing_instance_of(&owner) {
                            let span = if !name_range.is_empty() {
                                Some(name_range)
                            } else {
                                Some(self.body.exprs[expr].range())
                            };
                            let env_ro: &dyn TypeEnv = &*loader.store;
                            self.diagnostics.push(Diagnostic::error(
                                "static-context",
                                format!(
                                    "cannot use `{}.{}` from a static context",
                                    format_type(env_ro, &recv_ty),
                                    name
                                ),
                                span,
                            ));
                            return ExprInfo {
                                ty: Type::Error,
                                is_type_ref: false,
                            };
                        }
                    }

                    return ExprInfo {
                        ty: recv_ty,
                        is_type_ref: false,
                    };
                }
                "super" => {
                    if self.is_static_context() {
                        let span = if !name_range.is_empty() {
                            Some(name_range)
                        } else {
                            Some(self.body.exprs[expr].range())
                        };
                        self.diagnostics.push(Diagnostic::error(
                            "super-in-static-context",
                            "cannot use `super` in a static context",
                            span,
                        ));
                        return ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        };
                    }

                    if let Some(owner) = type_binary_name(loader.store, &recv_ty) {
                        if !self.has_enclosing_instance_of(&owner) {
                            let span = if !name_range.is_empty() {
                                Some(name_range)
                            } else {
                                Some(self.body.exprs[expr].range())
                            };
                            let env_ro: &dyn TypeEnv = &*loader.store;
                            self.diagnostics.push(Diagnostic::error(
                                "static-context",
                                format!(
                                    "cannot use `{}.{}` from a static context",
                                    format_type(env_ro, &recv_ty),
                                    name
                                ),
                                span,
                            ));
                            return ExprInfo {
                                ty: Type::Error,
                                is_type_ref: false,
                            };
                        }
                    }

                    let object_ty = loader
                        .store
                        .lookup_class("java.lang.Object")
                        .map(|id| Type::class(id, vec![]))
                        .unwrap_or(Type::Unknown);
                    let ty = match recv_ty {
                        Type::Class(class_ty) => loader
                            .store
                            .class(class_ty.def)
                            .and_then(|def| def.super_class.clone())
                            .unwrap_or(object_ty),
                        _ => Type::Unknown,
                    };
                    return ExprInfo {
                        ty,
                        is_type_ref: false,
                    };
                }
                _ => {}
            }

            // Static access: field or nested type.
            {
                let env_ro: &dyn TypeEnv = &*loader.store;
                let mut ctx = TyContext::new(env_ro);
                if let Some(field) = ctx.resolve_field(&recv_ty, name, CallKind::Static) {
                    return ExprInfo {
                        ty: field.ty,
                        is_type_ref: false,
                    };
                }
            }

            // Nested class (binary `$` form).
            if let Some(binary) = type_binary_name(loader.store, &recv_ty) {
                let nested = format!("{binary}${name}");
                let id = self
                    .ensure_workspace_class(loader, &nested)
                    .or_else(|| loader.ensure_class(&nested));
                if let Some(id) = id {
                    return ExprInfo {
                        ty: Type::class(id, vec![]),
                        is_type_ref: true,
                    };
                }
            }

            // Best-effort: if this field *would* resolve in an instance context, emit a more
            // precise diagnostic instead of `unresolved-field`.
            {
                let env_ro: &dyn TypeEnv = &*loader.store;
                let mut ctx = TyContext::new(env_ro);
                if ctx
                    .resolve_field(&recv_ty, name, CallKind::Instance)
                    .is_some()
                {
                    self.diagnostics.push(Diagnostic::error(
                        "static-context",
                        format!("cannot reference instance field `{name}` from a static context"),
                        Some(self.body.exprs[expr].range()),
                    ));
                    return ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    };
                }
            }
        } else {
            // Instance access.
            let env_ro: &dyn TypeEnv = &*loader.store;
            let mut ctx = TyContext::new(env_ro);
            if let Some(field) = ctx.resolve_field(&recv_ty, name, CallKind::Instance) {
                if field.is_static {
                    let span = if !name_range.is_empty() {
                        Some(name_range)
                    } else {
                        Some(self.body.exprs[expr].range())
                    };
                    self.diagnostics.push(Diagnostic::warning(
                        "static-access-via-instance",
                        format!("static field `{name}` accessed via an instance"),
                        span,
                    ));
                }
                return ExprInfo {
                    ty: field.ty,
                    is_type_ref: false,
                };
            }
        }

        self.diagnostics.push(Diagnostic::error(
            "unresolved-field",
            format!("unresolved field `{name}`"),
            Some(self.body.exprs[expr].range()),
        ));
        ExprInfo {
            ty: Type::Unknown,
            is_type_ref: false,
        }
    }

    /// If `expr` is a pure `Name` / `FieldAccess` chain, collect its segments.
    ///
    /// Examples:
    /// - `Name("java")` -> `["java"]`
    /// - `FieldAccess(receiver=<...>, name="lang")` -> append `"lang"`
    fn collect_name_or_field_chain_segments(&self, expr: HirExprId) -> Option<Vec<String>> {
        match &self.body.exprs[expr] {
            HirExpr::Name { name, .. } => Some(vec![name.clone()]),
            HirExpr::FieldAccess { receiver, name, .. } => {
                let mut segments = self.collect_name_or_field_chain_segments(*receiver)?;
                segments.push(name.clone());
                Some(segments)
            }
            _ => None,
        }
    }

    fn emit_unchecked_warning(&mut self, reason: UncheckedReason, span: Span) {
        let reason = match reason {
            UncheckedReason::RawConversion => "raw conversion",
            UncheckedReason::UncheckedCast => "cast",
            UncheckedReason::UncheckedVarargs => "varargs",
        };
        self.diagnostics.push(Diagnostic::warning(
            "unchecked",
            format!("unchecked {reason}"),
            Some(span),
        ));
    }

    fn emit_method_warnings(
        &mut self,
        method: &ResolvedMethod,
        call_span: Span,
        warn_static_access_via_instance: bool,
    ) {
        // `nova-types` aggregates some warnings in `ResolvedMethod.warnings`, but certain
        // call paths may also attach warnings to per-argument conversions. Surface both,
        // while ensuring we don't emit duplicates for the same call-site.
        let mut unique: Vec<TypeWarning> = Vec::new();
        for warning in &method.warnings {
            if !unique.contains(warning) {
                unique.push(warning.clone());
            }
        }
        for conv in &method.conversions {
            for warning in &conv.warnings {
                if !unique.contains(warning) {
                    unique.push(warning.clone());
                }
            }
        }

        for warning in unique {
            match warning {
                TypeWarning::StaticAccessViaInstance => {
                    if !warn_static_access_via_instance {
                        continue;
                    }
                    self.diagnostics.push(Diagnostic::warning(
                        "static-access-via-instance",
                        format!(
                            "static member `{}` accessed via an instance",
                            method.name.as_str()
                        ),
                        Some(call_span),
                    ));
                }
                TypeWarning::Unchecked(reason) => {
                    self.emit_unchecked_warning(reason, call_span);
                }
            }
        }
    }

    fn infer_call(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        callee: HirExprId,
        args: &[HirExprId],
        explicit_type_args: &[(String, Span)],
        expr: HirExprId,
        expected: Option<&Type>,
    ) -> ExprInfo {
        let arg_types = |this: &mut Self, loader: &mut ExternalTypeLoader<'_>| -> Vec<Type> {
            args.iter()
                .map(|arg| match &this.body.exprs[*arg] {
                    // Lambda/method refs are target-typed. Avoid inferring them without a target
                    // type to prevent spurious diagnostics; we can revisit once the callee is
                    // resolved and we know the parameter types.
                    HirExpr::Lambda { .. } => Type::Unknown,
                    HirExpr::MethodReference { receiver, .. }
                    | HirExpr::ConstructorReference { receiver, .. } => {
                        let _ = this.infer_expr(loader, *receiver);
                        Type::Unknown
                    }
                    // Best-effort poly-expression support: certain expressions (notably generic
                    // invocations like `Collections.emptyList()` and diamond class instance
                    // creation like `new ArrayList<>()`) can depend on the *target type* for
                    // inference. When they appear as arguments, we don't yet know the parameter
                    // target type until overload resolution succeeds.
                    //
                    // To avoid caching a non-target-typed result (e.g. `List<Object>`), we:
                    // - treat diamond `new` as `<unknown>` up-front
                    // - treat *generic* method invocations with placeholder arguments (`null`,
                    //   unknown/error) as `<unknown>` so we can later re-infer them once we know the
                    //   selected parameter type.
                    HirExpr::Call {
                        callee,
                        args: inner_args,
                        explicit_type_args: inner_type_args,
                        ..
                    } if inner_type_args.is_empty() => {
                        // Fast path: only bother with the poly-expression heuristic when the call's
                        // arguments provide weak/no constraints (no args or placeholder-ish args).
                        //
                        // Otherwise, generic inference from the arguments alone is usually
                        // sufficient and we can infer the call normally to help disambiguate
                        // overloads (e.g. `take(singletonList(\"x\"))`).
                        let mut inner_arg_tys = Vec::with_capacity(inner_args.len());
                        let mut has_placeholder_arg = inner_args.is_empty();
                        for inner_arg in inner_args {
                            let ty = match &this.body.exprs[*inner_arg] {
                                HirExpr::Lambda { .. } => {
                                    has_placeholder_arg = true;
                                    Type::Unknown
                                }
                                HirExpr::MethodReference { receiver, .. }
                                | HirExpr::ConstructorReference { receiver, .. } => {
                                    has_placeholder_arg = true;
                                    let _ = this.infer_expr(loader, *receiver);
                                    Type::Unknown
                                }
                                HirExpr::Null { .. } => {
                                    has_placeholder_arg = true;
                                    Type::Null
                                }
                                HirExpr::Missing { .. } => {
                                    has_placeholder_arg = true;
                                    Type::Unknown
                                }
                                HirExpr::New { class, .. }
                                    if is_diamond_type_ref_text(class.as_str()) =>
                                {
                                    has_placeholder_arg = true;
                                    Type::Unknown
                                }
                                _ => {
                                    let ty = this.infer_expr(loader, *inner_arg).ty;
                                    if ty.is_errorish() || ty == Type::Null {
                                        has_placeholder_arg = true;
                                    }
                                    ty
                                }
                            };
                            inner_arg_tys.push(ty);
                        }

                        if !has_placeholder_arg {
                            return this.infer_expr(loader, *arg).ty;
                        }

                        // For now, restrict the generic/poly heuristic to qualified calls like
                        // `Collections.emptyList()` where we can cheaply recover the receiver type
                        // and check whether the invoked method is actually generic.
                        let HirExpr::FieldAccess { receiver, name, .. } = &this.body.exprs[*callee]
                        else {
                            return this.infer_expr(loader, *arg).ty;
                        };

                        let recv_info = this.infer_expr(loader, *receiver);
                        let recv_ty = recv_info.ty.clone();
                        if recv_ty.is_errorish() {
                            return Type::Unknown;
                        }
                        this.ensure_type_loaded(loader, &recv_ty);

                        let call_kind = if recv_info.is_type_ref {
                            CallKind::Static
                        } else {
                            CallKind::Instance
                        };

                        let call = MethodCall {
                            receiver: recv_ty,
                            call_kind,
                            name: name.as_str(),
                            args: inner_arg_tys,
                            expected_return: None,
                            explicit_type_args: Vec::new(),
                        };

                        let env_ro: &dyn TypeEnv = &*loader.store;
                        let mut ctx = TyContext::new(env_ro);
                        match nova_types::resolve_method_call(&mut ctx, &call) {
                            MethodResolution::Found(method)
                                if !method.inferred_type_args.is_empty() =>
                            {
                                Type::Unknown
                            }
                            _ => this.infer_expr(loader, *arg).ty,
                        }
                    }
                    // Diamond class instance creation is also target-typed: inferring it without a
                    // target type will default type arguments (often to `Object`) which can cause
                    // overload resolution to fail in common code like:
                    // `take(new ArrayList<>())` where `take` expects `List<String>`.
                    HirExpr::New { class, .. } if is_diamond_type_ref_text(class.as_str()) => {
                        Type::Unknown
                    }
                    _ => this.infer_expr(loader, *arg).ty,
                })
                .collect()
        };

        let apply_arg_targets =
            |this: &mut Self, loader: &mut ExternalTypeLoader<'_>, method: &ResolvedMethod| {
                for (arg, param_ty) in args.iter().zip(method.params.iter()) {
                    // Target-typed expressions like lambdas and method references may need the full
                    // functional interface definition (SAM) available. Ensure the parameter type is
                    // loaded before attempting target typing.
                    this.ensure_type_loaded(loader, param_ty);
                    let _ = this.infer_expr_with_expected(loader, *arg, Some(param_ty));
                }
            };

        let mut resolved_explicit_type_args = Vec::with_capacity(explicit_type_args.len());
        let mut explicit_type_args_errorish = false;
        let mut explicit_type_args_span: Option<Span> = None;
        for (text, span) in explicit_type_args {
            let resolved = resolve_type_ref_text(
                self.resolver,
                self.scopes,
                self.scope_id,
                loader,
                &self.type_vars,
                text.as_str(),
                Some(*span),
            );
            explicit_type_args_errorish |=
                resolved.ty.is_errorish() || !resolved.diagnostics.is_empty();
            self.diagnostics.extend(resolved.diagnostics);
            resolved_explicit_type_args.push(resolved.ty);
            explicit_type_args_span = Some(match explicit_type_args_span {
                Some(existing) => {
                    Span::new(existing.start.min(span.start), existing.end.max(span.end))
                }
                None => *span,
            });
        }

        if explicit_type_args_errorish {
            self.diagnostics.push(Diagnostic::error(
                "invalid-type-args",
                "invalid explicit type arguments",
                explicit_type_args_span,
            ));
            resolved_explicit_type_args.clear();
            // Treat invalid type arguments as absent so we can recover via inference.
            explicit_type_args_errorish = false;
        }

        match &self.body.exprs[callee] {
            HirExpr::This { .. } => self.infer_explicit_constructor_invocation(
                loader,
                ExplicitConstructorInvocationKind::This,
                args,
                expr,
            ),
            HirExpr::Super { .. } => self.infer_explicit_constructor_invocation(
                loader,
                ExplicitConstructorInvocationKind::Super,
                args,
                expr,
            ),
            HirExpr::FieldAccess { receiver, name, .. } => {
                let recv_info = self.infer_expr(loader, *receiver);
                if recv_info.ty == Type::Error {
                    return ExprInfo {
                        ty: Type::Error,
                        is_type_ref: false,
                    };
                }
                let call_kind = if recv_info.is_type_ref {
                    CallKind::Static
                } else {
                    CallKind::Instance
                };
                let is_static_receiver = recv_info.is_type_ref;
                let recv_ty = recv_info.ty.clone();
                self.ensure_type_loaded(loader, &recv_ty);

                let arg_types = arg_types(self, loader);
                let call = MethodCall {
                    receiver: recv_ty,
                    call_kind,
                    name: name.as_str(),
                    args: arg_types,
                    expected_return: expected.cloned(),
                    explicit_type_args: resolved_explicit_type_args.clone(),
                };

                let env_ro: &dyn TypeEnv = &*loader.store;
                let mut ctx = TyContext::new(env_ro);
                match nova_types::resolve_method_call(&mut ctx, &call) {
                    MethodResolution::Found(method) => {
                        self.emit_method_warnings(
                            &method,
                            self.body.exprs[expr].range(),
                            call_kind == CallKind::Instance,
                        );
                        self.call_resolutions[expr.idx()] = Some(method.clone());
                        apply_arg_targets(self, loader, &method);
                        ExprInfo {
                            ty: method.return_type,
                            is_type_ref: false,
                        }
                    }
                    MethodResolution::Ambiguous(amb) => {
                        if !explicit_type_args_errorish {
                            self.diagnostics.push(self.ambiguous_call_diag(
                                env_ro,
                                name.as_str(),
                                &amb.candidates,
                                self.body.exprs[expr].range(),
                            ));
                        }
                        if let Some(first) = amb.candidates.first() {
                            self.call_resolutions[expr.idx()] = Some(first.clone());
                            apply_arg_targets(self, loader, first);
                            ExprInfo {
                                ty: first.return_type.clone(),
                                is_type_ref: false,
                            }
                        } else {
                            ExprInfo {
                                ty: Type::Unknown,
                                is_type_ref: false,
                            }
                        }
                    }
                    MethodResolution::NotFound(not_found) => {
                        if is_static_receiver && !explicit_type_args_errorish {
                            // Best-effort: if this call *would* resolve in an instance context, emit
                            // a more precise static-context diagnostic instead of `unresolved-method`.
                            let instance_call = MethodCall {
                                receiver: call.receiver.clone(),
                                call_kind: CallKind::Instance,
                                name: call.name,
                                args: call.args.clone(),
                                expected_return: call.expected_return.clone(),
                                explicit_type_args: call.explicit_type_args.clone(),
                            };
                            let mut ctx = TyContext::new(env_ro);
                            match nova_types::resolve_method_call(&mut ctx, &instance_call) {
                                MethodResolution::Found(_) | MethodResolution::Ambiguous(_) => {
                                    self.diagnostics.push(Diagnostic::error(
                                        "static-context",
                                        format!(
                                            "cannot call instance method `{}` from a static context",
                                            name.as_str()
                                        ),
                                        Some(self.body.exprs[expr].range()),
                                    ));
                                    return ExprInfo {
                                        ty: Type::Error,
                                        is_type_ref: false,
                                    };
                                }
                                MethodResolution::NotFound(_) => {}
                            }
                        }

                        if !explicit_type_args_errorish {
                            self.diagnostics.push(self.unresolved_method_diag(
                                env_ro,
                                &not_found,
                                self.body.exprs[expr].range(),
                            ));
                        }
                        ExprInfo {
                            ty: Type::Error,
                            is_type_ref: false,
                        }
                    }
                }
            }
            HirExpr::Name { name, range } => {
                let arg_types = arg_types(self, loader);

                // Unqualified calls like `foo()` are usually shorthand for `this.foo()`.
                // Resolve them against the enclosing class first (using the right
                // call kind for the current static/instance context), then fall back to
                // static-imported methods.
                let mut implicit_not_found: Option<MethodNotFound> = None;
                if let Some(receiver_ty) = self.enclosing_class_type(loader) {
                    self.ensure_type_loaded(loader, &receiver_ty);

                    let is_static_context = self.is_static_context();
                    let call_kind = if is_static_context {
                        CallKind::Static
                    } else {
                        CallKind::Instance
                    };

                    let call = MethodCall {
                        receiver: receiver_ty.clone(),
                        call_kind,
                        name: name.as_str(),
                        args: arg_types.clone(),
                        expected_return: expected.cloned(),
                        explicit_type_args: resolved_explicit_type_args.clone(),
                    };

                    let env_ro: &dyn TypeEnv = &*loader.store;
                    let mut ctx = TyContext::new(env_ro);
                    match nova_types::resolve_method_call(&mut ctx, &call) {
                        MethodResolution::Found(method) => {
                            // An unqualified call like `foo()` is not a static-access-via-instance
                            // scenario even in an instance method (unlike `obj.foo()`).
                            self.emit_method_warnings(
                                &method,
                                self.body.exprs[expr].range(),
                                false,
                            );
                            self.call_resolutions[expr.idx()] = Some(method.clone());
                            apply_arg_targets(self, loader, &method);
                            return ExprInfo {
                                ty: method.return_type,
                                is_type_ref: false,
                            };
                        }
                        MethodResolution::Ambiguous(amb) => {
                            if !explicit_type_args_errorish {
                                self.diagnostics.push(self.ambiguous_call_diag(
                                    env_ro,
                                    name.as_str(),
                                    &amb.candidates,
                                    self.body.exprs[expr].range(),
                                ));
                            }
                            if let Some(first) = amb.candidates.first() {
                                self.call_resolutions[expr.idx()] = Some(first.clone());
                                apply_arg_targets(self, loader, first);
                                return ExprInfo {
                                    ty: first.return_type.clone(),
                                    is_type_ref: false,
                                };
                            }
                            return ExprInfo {
                                ty: Type::Unknown,
                                is_type_ref: false,
                            };
                        }
                        MethodResolution::NotFound(not_found) => {
                            implicit_not_found = Some(not_found);
                        }
                    }

                    if is_static_context {
                        // Best-effort: if this call *would* resolve in an instance context, emit a
                        // more precise diagnostic instead of falling back to static imports.
                        let call = MethodCall {
                            receiver: receiver_ty,
                            call_kind: CallKind::Instance,
                            name: name.as_str(),
                            args: arg_types.clone(),
                            expected_return: None,
                            explicit_type_args: resolved_explicit_type_args.clone(),
                        };
                        let mut ctx = TyContext::new(env_ro);
                        match nova_types::resolve_method_call(&mut ctx, &call) {
                            MethodResolution::Found(_) | MethodResolution::Ambiguous(_) => {
                                if !explicit_type_args_errorish {
                                    self.diagnostics.push(Diagnostic::error(
                                        "static-context",
                                        format!(
                                            "cannot call instance method `{}` from a static context",
                                            name
                                        ),
                                        Some(self.body.exprs[expr].range()),
                                    ));
                                }
                                return ExprInfo {
                                    ty: Type::Error,
                                    is_type_ref: false,
                                };
                            }
                            MethodResolution::NotFound(_) => {}
                        }
                    }
                }

                // Handle static-imported methods.
                match self.resolver.resolve_name_detailed(
                    self.scopes,
                    self.scope_id,
                    &Name::from(name.as_str()),
                ) {
                    NameResolution::Resolved(Resolution::StaticMember(
                        StaticMemberResolution::External(id),
                    )) => {
                        let Some((owner, member)) = id.as_str().split_once("::") else {
                            return ExprInfo {
                                ty: Type::Unknown,
                                is_type_ref: false,
                            };
                        };

                        let recv_ty = self
                            .ensure_workspace_class(loader, owner)
                            .or_else(|| loader.ensure_class(owner))
                            .map(|id| Type::class(id, vec![]))
                            .unwrap_or_else(|| Type::Named(owner.to_string()));
                        self.ensure_type_loaded(loader, &recv_ty);

                        let call = MethodCall {
                            receiver: recv_ty,
                            call_kind: CallKind::Static,
                            name: member,
                            args: arg_types,
                            expected_return: expected.cloned(),
                            explicit_type_args: resolved_explicit_type_args.clone(),
                        };

                        let env_ro: &dyn TypeEnv = &*loader.store;
                        let mut ctx = TyContext::new(env_ro);
                        match nova_types::resolve_method_call(&mut ctx, &call) {
                            MethodResolution::Found(method) => {
                                self.emit_method_warnings(
                                    &method,
                                    self.body.exprs[expr].range(),
                                    false,
                                );
                                self.call_resolutions[expr.idx()] = Some(method.clone());
                                apply_arg_targets(self, loader, &method);
                                ExprInfo {
                                    ty: method.return_type,
                                    is_type_ref: false,
                                }
                            }
                            MethodResolution::Ambiguous(amb) => {
                                if !explicit_type_args_errorish {
                                    self.diagnostics.push(self.ambiguous_call_diag(
                                        env_ro,
                                        member,
                                        &amb.candidates,
                                        self.body.exprs[expr].range(),
                                    ));
                                }
                                if let Some(first) = amb.candidates.first() {
                                    self.call_resolutions[expr.idx()] = Some(first.clone());
                                    apply_arg_targets(self, loader, first);
                                    ExprInfo {
                                        ty: first.return_type.clone(),
                                        is_type_ref: false,
                                    }
                                } else {
                                    ExprInfo {
                                        ty: Type::Unknown,
                                        is_type_ref: false,
                                    }
                                }
                            }
                            MethodResolution::NotFound(not_found) => {
                                if !explicit_type_args_errorish {
                                    self.diagnostics.push(self.unresolved_method_diag(
                                        env_ro,
                                        &not_found,
                                        self.body.exprs[expr].range(),
                                    ));
                                }
                                ExprInfo {
                                    ty: Type::Error,
                                    is_type_ref: false,
                                }
                            }
                        }
                    }
                    // Static imports can import both fields and methods with the same name (they
                    // live in separate namespaces in Java). The resolver's static-member query is
                    // context-free and may return `SourceField` even when the call should resolve
                    // to a method; try resolving as a method call anyway.
                    NameResolution::Resolved(Resolution::StaticMember(
                        StaticMemberResolution::SourceField(field),
                    )) => {
                        let Some(owner) = self.field_owners.get(&field).cloned() else {
                            self.diagnostics.push(Diagnostic::error(
                                "unresolved-method",
                                format!("unresolved call `{}`", name),
                                Some(*range),
                            ));
                            return ExprInfo {
                                ty: Type::Unknown,
                                is_type_ref: false,
                            };
                        };
                        let recv_ty = self
                            .ensure_workspace_class(loader, &owner)
                            .or_else(|| loader.ensure_class(&owner))
                            .map(|id| Type::class(id, vec![]))
                            .unwrap_or_else(|| Type::Named(owner.clone()));
                        self.ensure_type_loaded(loader, &recv_ty);

                        let call = MethodCall {
                            receiver: recv_ty,
                            call_kind: CallKind::Static,
                            name: name.as_str(),
                            args: arg_types,
                            expected_return: expected.cloned(),
                            explicit_type_args: resolved_explicit_type_args.clone(),
                        };

                        let env_ro: &dyn TypeEnv = &*loader.store;
                        let mut ctx = TyContext::new(env_ro);
                        match nova_types::resolve_method_call(&mut ctx, &call) {
                            MethodResolution::Found(method) => {
                                self.emit_method_warnings(
                                    &method,
                                    self.body.exprs[expr].range(),
                                    false,
                                );
                                self.call_resolutions[expr.idx()] = Some(method.clone());
                                apply_arg_targets(self, loader, &method);
                                ExprInfo {
                                    ty: method.return_type,
                                    is_type_ref: false,
                                }
                            }
                            MethodResolution::Ambiguous(amb) => {
                                if !explicit_type_args_errorish {
                                    self.diagnostics.push(self.ambiguous_call_diag(
                                        env_ro,
                                        name.as_str(),
                                        &amb.candidates,
                                        self.body.exprs[expr].range(),
                                    ));
                                }
                                if let Some(first) = amb.candidates.first() {
                                    self.call_resolutions[expr.idx()] = Some(first.clone());
                                    apply_arg_targets(self, loader, first);
                                    ExprInfo {
                                        ty: first.return_type.clone(),
                                        is_type_ref: false,
                                    }
                                } else {
                                    ExprInfo {
                                        ty: Type::Unknown,
                                        is_type_ref: false,
                                    }
                                }
                            }
                            MethodResolution::NotFound(not_found) => {
                                if !explicit_type_args_errorish {
                                    self.diagnostics.push(self.unresolved_method_diag(
                                        env_ro,
                                        &not_found,
                                        self.body.exprs[expr].range(),
                                    ));
                                }
                                ExprInfo {
                                    ty: Type::Error,
                                    is_type_ref: false,
                                }
                            }
                        }
                    }
                    NameResolution::Resolved(Resolution::StaticMember(
                        StaticMemberResolution::SourceMethod(method),
                    )) => {
                        let Some(owner) = self.method_owners.get(&method).cloned() else {
                            self.diagnostics.push(Diagnostic::error(
                                "unresolved-method",
                                format!("unresolved call `{}`", name),
                                Some(*range),
                            ));
                            return ExprInfo {
                                ty: Type::Unknown,
                                is_type_ref: false,
                            };
                        };
                        let recv_ty = self
                            .ensure_workspace_class(loader, &owner)
                            .or_else(|| loader.ensure_class(&owner))
                            .map(|id| Type::class(id, vec![]))
                            .unwrap_or_else(|| Type::Named(owner.clone()));
                        self.ensure_type_loaded(loader, &recv_ty);

                        let call = MethodCall {
                            receiver: recv_ty,
                            call_kind: CallKind::Static,
                            name: name.as_str(),
                            args: arg_types,
                            expected_return: expected.cloned(),
                            explicit_type_args: resolved_explicit_type_args.clone(),
                        };

                        let env_ro: &dyn TypeEnv = &*loader.store;
                        let mut ctx = TyContext::new(env_ro);
                        match nova_types::resolve_method_call(&mut ctx, &call) {
                            MethodResolution::Found(method) => {
                                self.emit_method_warnings(
                                    &method,
                                    self.body.exprs[expr].range(),
                                    false,
                                );
                                self.call_resolutions[expr.idx()] = Some(method.clone());
                                apply_arg_targets(self, loader, &method);
                                ExprInfo {
                                    ty: method.return_type,
                                    is_type_ref: false,
                                }
                            }
                            MethodResolution::Ambiguous(amb) => {
                                if !explicit_type_args_errorish {
                                    self.diagnostics.push(self.ambiguous_call_diag(
                                        env_ro,
                                        name.as_str(),
                                        &amb.candidates,
                                        self.body.exprs[expr].range(),
                                    ));
                                }
                                if let Some(first) = amb.candidates.first() {
                                    self.call_resolutions[expr.idx()] = Some(first.clone());
                                    apply_arg_targets(self, loader, first);
                                    ExprInfo {
                                        ty: first.return_type.clone(),
                                        is_type_ref: false,
                                    }
                                } else {
                                    ExprInfo {
                                        ty: Type::Unknown,
                                        is_type_ref: false,
                                    }
                                }
                            }
                            MethodResolution::NotFound(not_found) => {
                                if !explicit_type_args_errorish {
                                    self.diagnostics.push(self.unresolved_method_diag(
                                        env_ro,
                                        &not_found,
                                        self.body.exprs[expr].range(),
                                    ));
                                }
                                ExprInfo {
                                    ty: Type::Error,
                                    is_type_ref: false,
                                }
                            }
                        }
                    }
                    NameResolution::Ambiguous(_) => {
                        self.diagnostics.push(Diagnostic::error(
                            "ambiguous-name",
                            format!("ambiguous reference `{}`", name),
                            Some(*range),
                        ));
                        ExprInfo {
                            ty: Type::Unknown,
                            is_type_ref: false,
                        }
                    }
                    _ => {
                        if let Some(not_found) = implicit_not_found {
                            let env_ro: &dyn TypeEnv = &*loader.store;
                            self.diagnostics.push(self.unresolved_method_diag(
                                env_ro,
                                &not_found,
                                self.body.exprs[expr].range(),
                            ));
                        } else {
                            self.diagnostics.push(Diagnostic::error(
                                "unresolved-method",
                                format!("unresolved call `{}`", name),
                                Some(*range),
                            ));
                        }
                        ExprInfo {
                            ty: Type::Unknown,
                            is_type_ref: false,
                        }
                    }
                }
            }
            _ => ExprInfo {
                ty: Type::Unknown,
                is_type_ref: false,
            },
        }
    }

    fn infer_explicit_constructor_invocation(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        kind: ExplicitConstructorInvocationKind,
        args: &[HirExprId],
        expr: HirExprId,
    ) -> ExprInfo {
        let arg_types = args
            .iter()
            .map(|arg| match &self.body.exprs[*arg] {
                // Lambda/method refs are target-typed. Avoid inferring them without a target
                // type to prevent spurious diagnostics; we can revisit once the constructor is
                // resolved and we know the parameter types.
                HirExpr::Lambda { .. } => Type::Unknown,
                HirExpr::MethodReference { receiver, .. }
                | HirExpr::ConstructorReference { receiver, .. } => {
                    let _ = self.infer_expr(loader, *receiver);
                    Type::Unknown
                }
                HirExpr::Call {
                    args: inner_args,
                    explicit_type_args: inner_type_args,
                    ..
                } if inner_args.is_empty() && inner_type_args.is_empty() => Type::Unknown,
                HirExpr::New { class, .. } if is_diamond_type_ref_text(class.as_str()) => {
                    Type::Unknown
                }
                _ => self.infer_expr(loader, *arg).ty,
            })
            .collect::<Vec<_>>();

        if !matches!(self.owner, DefWithBodyId::Constructor(_)) {
            self.diagnostics.push(Diagnostic::error(
                "invalid-constructor-invocation",
                "`this(...)`/`super(...)` constructor invocations are only allowed in constructors",
                Some(self.body.exprs[expr].range()),
            ));
            return ExprInfo {
                ty: Type::Error,
                is_type_ref: false,
            };
        }

        let target_ty = match kind {
            ExplicitConstructorInvocationKind::This => self.enclosing_class_type(loader),
            ExplicitConstructorInvocationKind::Super => {
                let object_ty = loader
                    .store
                    .lookup_class("java.lang.Object")
                    .map(|id| Type::class(id, vec![]))
                    .unwrap_or(Type::Unknown);

                let super_ty = match self.enclosing_class_type(loader) {
                    Some(enclosing) => {
                        self.ensure_type_loaded(loader, &enclosing);
                        match enclosing {
                            Type::Class(class_ty) => loader
                                .store
                                .class(class_ty.def)
                                .and_then(|def| def.super_class.clone())
                                .unwrap_or(object_ty),
                            _ => object_ty,
                        }
                    }
                    None => object_ty,
                };

                Some(super_ty)
            }
        };

        let Some(target_ty) = target_ty else {
            self.diagnostics.push(Diagnostic::error(
                "unresolved-constructor",
                format!(
                    "unable to resolve `{}` constructor invocation target type",
                    kind.as_str()
                ),
                Some(self.body.exprs[expr].range()),
            ));
            return ExprInfo {
                ty: Type::Error,
                is_type_ref: false,
            };
        };

        self.ensure_type_loaded(loader, &target_ty);

        let class_id = match &target_ty {
            Type::Class(class_ty) => Some(class_ty.def),
            Type::Named(name) => self
                .ensure_workspace_class(loader, name)
                .or_else(|| loader.ensure_class(name)),
            _ => None,
        };

        let Some(class_id) = class_id else {
            self.diagnostics.push(Diagnostic::error(
                "unresolved-constructor",
                format!(
                    "unable to resolve `{}` constructor invocation target type",
                    kind.as_str()
                ),
                Some(self.body.exprs[expr].range()),
            ));
            return ExprInfo {
                ty: Type::Error,
                is_type_ref: false,
            };
        };

        let expected_for_call = match &target_ty {
            Type::Class(nova_types::ClassType { def, .. }) if *def == class_id => Some(&target_ty),
            _ => None,
        };

        let env_ro: &dyn TypeEnv = &*loader.store;
        match nova_types::resolve_constructor_call(env_ro, class_id, &arg_types, expected_for_call)
        {
            MethodResolution::Found(method) => {
                for (arg, param_ty) in args.iter().zip(method.params.iter()) {
                    // Target-typed expressions like lambdas and method references may need the
                    // full functional interface definition (SAM) available. Ensure the parameter
                    // type is loaded before attempting target typing.
                    self.ensure_type_loaded(loader, param_ty);
                    let _ = self.infer_expr_with_expected(loader, *arg, Some(param_ty));
                }
                self.call_resolutions[expr.idx()] = Some(method);
                ExprInfo {
                    ty: Type::Void,
                    is_type_ref: false,
                }
            }
            MethodResolution::Ambiguous(amb) => {
                self.diagnostics.push(self.ambiguous_constructor_diag(
                    env_ro,
                    class_id,
                    &amb.candidates,
                    self.body.exprs[expr].range(),
                ));
                if let Some(first) = amb.candidates.first() {
                    for (arg, param_ty) in args.iter().zip(first.params.iter()) {
                        self.ensure_type_loaded(loader, param_ty);
                        let _ = self.infer_expr_with_expected(loader, *arg, Some(param_ty));
                    }
                    self.call_resolutions[expr.idx()] = Some(first.clone());
                }
                ExprInfo {
                    ty: Type::Void,
                    is_type_ref: false,
                }
            }
            MethodResolution::NotFound(not_found) => {
                self.diagnostics.push(self.unresolved_constructor_diag(
                    env_ro,
                    class_id,
                    &not_found,
                    self.body.exprs[expr].range(),
                ));
                ExprInfo {
                    ty: Type::Error,
                    is_type_ref: false,
                }
            }
        }
    }

    fn unresolved_method_diag(
        &self,
        env: &dyn TypeEnv,
        not_found: &MethodNotFound,
        span: Span,
    ) -> Diagnostic {
        let receiver = format_type(env, &not_found.receiver);
        let args = if not_found.args.is_empty() {
            "()".to_string()
        } else {
            let rendered = not_found
                .args
                .iter()
                .map(|t| format_type(env, t))
                .collect::<Vec<_>>();
            format!("({})", rendered.join(", "))
        };

        let mut message = format!(
            "unresolved method `{}` for receiver `{}` with arguments {}",
            not_found.name, receiver, args
        );

        if not_found.candidates.is_empty() {
            return Diagnostic::error("unresolved-method", message, Some(span));
        }

        message.push_str("\n\ncandidates:");
        for cand in not_found.candidates.iter().take(5) {
            message.push_str("\n  - ");
            message.push_str(&format_method_candidate_signature(env, &cand.candidate));

            if let Some(failure) = cand.failures.first() {
                message.push_str("\n    ");
                message.push_str(&format_method_candidate_failure_reason(
                    env,
                    &failure.reason,
                ));
            }
        }

        if not_found.candidates.len() > 5 {
            message.push_str(&format!(
                "\n  ... and {} more",
                not_found.candidates.len().saturating_sub(5)
            ));
        }

        Diagnostic::error("unresolved-method", message, Some(span))
    }

    fn unresolved_constructor_diag(
        &self,
        env: &dyn TypeEnv,
        class: nova_types::ClassId,
        not_found: &MethodNotFound,
        span: Span,
    ) -> Diagnostic {
        let ctor_name = format_type(env, &not_found.receiver);
        let args = if not_found.args.is_empty() {
            "()".to_string()
        } else {
            let rendered = not_found
                .args
                .iter()
                .map(|t| format_type(env, t))
                .collect::<Vec<_>>();
            format!("({})", rendered.join(", "))
        };

        let mut message = format!("unresolved constructor `{ctor_name}` with arguments {args}");

        if not_found.candidates.is_empty() {
            // `resolve_constructor_call` filters out inaccessible constructors. If we have
            // constructor metadata for the class and *all* constructors are marked inaccessible,
            // emit a clearer message than a generic "unresolved constructor".
            if let Some(def) = env.class(class) {
                if !def.constructors.is_empty() && def.constructors.iter().all(|c| !c.is_accessible)
                {
                    message =
                        format!("no accessible constructor `{ctor_name}` with arguments {args}");
                }
            }
            return Diagnostic::error("unresolved-constructor", message, Some(span));
        }

        message.push_str("\n\ncandidates:");
        for cand in not_found.candidates.iter().take(5) {
            message.push_str("\n  - ");
            message.push_str(&format_constructor_candidate_signature(
                env,
                &ctor_name,
                &cand.candidate,
            ));

            if let Some(failure) = cand.failures.first() {
                message.push_str("\n    ");
                message.push_str(&format_method_candidate_failure_reason(
                    env,
                    &failure.reason,
                ));
            }
        }

        if not_found.candidates.len() > 5 {
            message.push_str(&format!(
                "\n  ... and {} more",
                not_found.candidates.len().saturating_sub(5)
            ));
        }

        Diagnostic::error("unresolved-constructor", message, Some(span))
    }

    fn ambiguous_constructor_diag(
        &self,
        env: &dyn TypeEnv,
        class: nova_types::ClassId,
        candidates: &[ResolvedMethod],
        span: Span,
    ) -> Diagnostic {
        let ctor_name = candidates
            .first()
            .map(|c| format_type(env, &c.return_type))
            .unwrap_or_else(|| format_type(env, &Type::class(class, vec![])));

        let mut message = format!("ambiguous constructor call `{ctor_name}`");
        if candidates.is_empty() {
            return Diagnostic::error("ambiguous-constructor", message, Some(span));
        }

        message.push_str("\n\ncandidates:");
        for cand in candidates.iter().take(8) {
            message.push_str("\n  - ");
            message.push_str(&format_resolved_method(env, cand));
        }
        if candidates.len() > 8 {
            message.push_str(&format!(
                "\n  ... and {} more",
                candidates.len().saturating_sub(8)
            ));
        }

        Diagnostic::error("ambiguous-constructor", message, Some(span))
    }

    fn ambiguous_call_diag(
        &self,
        env: &dyn TypeEnv,
        name: &str,
        candidates: &[ResolvedMethod],
        span: Span,
    ) -> Diagnostic {
        let mut message = format!("ambiguous call `{name}`");
        if candidates.is_empty() {
            return Diagnostic::error("ambiguous-call", message, Some(span));
        }

        message.push_str("\n\ncandidates:");
        for cand in candidates.iter().take(8) {
            message.push_str("\n  - ");
            message.push_str(&format_resolved_method(env, cand));
        }
        if candidates.len() > 8 {
            message.push_str(&format!(
                "\n  ... and {} more",
                candidates.len().saturating_sub(8)
            ));
        }

        Diagnostic::error("ambiguous-call", message, Some(span))
    }

    fn is_static_context(&self) -> bool {
        match self.owner {
            DefWithBodyId::Method(m) => self.tree.method(m).modifiers.raw & Modifiers::STATIC != 0,
            DefWithBodyId::Constructor(_) => false,
            DefWithBodyId::Initializer(i) => self.tree.initializer(i).is_static,
        }
    }

    /// Determine whether `owner` has an implicit enclosing instance available at this body site.
    ///
    /// This is primarily used to reject unqualified references to instance members declared on an
    /// enclosing type when the current type is a `static`/implicitly-static member type.
    fn has_enclosing_instance_of(&self, owner: &str) -> bool {
        if self.is_static_context() {
            // Static bodies have no implicit `this`, and therefore no enclosing instances.
            return false;
        }

        let class_items = self.enclosing_class_items();
        if class_items.is_empty() {
            return false;
        }

        for idx in 0..class_items.len() {
            if let Some(name) = self.scopes.type_name(class_items[idx]) {
                if name.as_str() == owner {
                    return true;
                }
            }

            let Some(next) = class_items.get(idx + 1).copied() else {
                break;
            };
            if self.member_type_is_static(class_items[idx], next) {
                break;
            }
        }

        false
    }

    fn enclosing_class_items(&self) -> Vec<nova_hir::ids::ItemId> {
        let mut items = Vec::new();
        let mut scope = Some(self.scope_id);
        let mut steps = 0u32;
        while let Some(id) = scope {
            // Avoid panics and infinite loops if the scope graph is malformed.
            let Some(data) = self.scopes.scope_opt(id) else {
                break;
            };
            if let ScopeKind::Class { item } = data.kind() {
                items.push(*item);
            }

            scope = data.parent();
            steps = steps.wrapping_add(1);
            if steps > 256 {
                break;
            }
        }
        items
    }

    /// Returns `true` if `item` is a member type that is `static` (or implicitly static) with
    /// respect to its enclosing type `enclosing`.
    fn member_type_is_static(
        &self,
        item: nova_hir::ids::ItemId,
        enclosing: nova_hir::ids::ItemId,
    ) -> bool {
        use nova_hir::ids::ItemId::*;

        // Interfaces, annotations, enums, and records are implicitly static member types.
        if matches!(item, Interface(_) | Annotation(_) | Enum(_) | Record(_)) {
            return true;
        }

        // Any member type declared in an interface/annotation is implicitly static.
        if matches!(enclosing, Interface(_) | Annotation(_)) {
            return true;
        }

        match item {
            Class(id) => self.tree.class(id).modifiers.raw & Modifiers::STATIC != 0,
            // Covered above, but keep a catch-all for future variants.
            _ => false,
        }
    }

    fn var_inference_enabled(&self) -> bool {
        // `var` local variable type inference was added in Java 10 (JEP 286),
        // including support in enhanced-for loops.
        self.java_level.supports_var_local_inference()
    }

    fn enclosing_class_type(&self, loader: &mut ExternalTypeLoader<'_>) -> Option<Type> {
        let mut scope = Some(self.scope_id);
        let mut steps = 0u32;
        while let Some(id) = scope {
            // Avoid panics and infinite loops if the scope graph is malformed.
            let data = self.scopes.scope_opt(id)?;
            if let ScopeKind::Class { item } = data.kind() {
                let ty_name = self.scopes.type_name(*item)?;
                let class_id = loader.store.intern_class_id(ty_name.as_str());
                return Some(Type::class(class_id, Vec::new()));
            }

            scope = data.parent();
            steps = steps.wrapping_add(1);
            if steps > 256 {
                break;
            }
        }

        None
    }

    fn resolve_qualified_this_super_qualifier_type(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        range: Span,
        keyword: &str,
    ) -> Option<Type> {
        let file = def_file(self.owner);
        let text = self.db.file_content(file);
        let snippet = text.get(range.start..range.end)?;

        // Normalize by stripping whitespace so patterns like `Outer . this` are handled.
        let mut normalized = String::new();
        for ch in snippet.chars() {
            if !ch.is_whitespace() {
                normalized.push(ch);
            }
        }

        let suffix = format!(".{keyword}");
        let prefix = normalized.strip_suffix(&suffix)?;
        if prefix.is_empty() {
            return None;
        }

        let q = QualifiedName::from_dotted(prefix);
        let resolved =
            self.resolver
                .resolve_qualified_type_in_scope(self.scopes, self.scope_id, &q)?;
        let id = loader.store.intern_class_id(resolved.as_str());
        let ty = Type::class(id, Vec::new());

        // Qualified `this` / `super` expressions can only refer to an *enclosing* instance type.
        // In static (or implicitly static) member types, there may be no enclosing instance
        // available even if name resolution finds the type.
        if !self.has_enclosing_instance_of(resolved.as_str()) {
            let env_ro: &dyn TypeEnv = &*loader.store;
            self.diagnostics.push(Diagnostic::error(
                "static-context",
                format!(
                    "cannot use `{}.{}` from a static context",
                    format_type(env_ro, &ty),
                    keyword
                ),
                Some(range),
            ));
            return Some(Type::Error);
        }

        self.ensure_type_loaded(loader, &ty);
        Some(ty)
    }

    fn infer_binary(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        expr: HirExprId,
        op: BinaryOp,
        lhs: HirExprId,
        rhs: HirExprId,
    ) -> ExprInfo {
        let lhs_ty = self.infer_expr(loader, lhs).ty;
        let rhs_ty = self.infer_expr(loader, rhs).ty;

        let env_ro: &dyn TypeEnv = &*loader.store;
        let span = Some(self.body.exprs[expr].range());

        let string_ty = Type::class(loader.store.well_known().string, vec![]);
        let lhs_prim = primitive_like(env_ro, &lhs_ty);
        let rhs_prim = primitive_like(env_ro, &rhs_ty);

        let type_mismatch = |this: &mut Self| {
            let lhs_render = format_type(env_ro, &lhs_ty);
            let rhs_render = format_type(env_ro, &rhs_ty);
            this.diagnostics.push(Diagnostic::error(
                "type-mismatch",
                format!("type mismatch: cannot apply `{op:?}` to {lhs_render} and {rhs_render}"),
                span,
            ));
        };

        let ty = match op {
            // `==` and `!=` always produce boolean, but validate primitive operand pairs to
            // avoid silently accepting obvious mismatches (e.g. `1 == \"x\"`).
            BinaryOp::EqEq | BinaryOp::NotEq => {
                if lhs_ty.is_errorish() || rhs_ty.is_errorish() {
                    Type::boolean()
                } else {
                    let lhs_is_primitive = matches!(lhs_ty, Type::Primitive(_));
                    let rhs_is_primitive = matches!(rhs_ty, Type::Primitive(_));

                    // If either operand is a primitive, Java uses primitive equality rules
                    // (possibly with unboxing of the other operand).
                    if lhs_is_primitive || rhs_is_primitive {
                        match (lhs_prim, rhs_prim) {
                            (Some(a), Some(b)) => {
                                let ok = (a.is_numeric() && b.is_numeric())
                                    || (a == PrimitiveType::Boolean && b == PrimitiveType::Boolean);
                                if ok {
                                    Type::boolean()
                                } else {
                                    type_mismatch(self);
                                    Type::Error
                                }
                            }
                            _ => {
                                type_mismatch(self);
                                Type::Error
                            }
                        }
                    } else if matches!((&lhs_ty, &rhs_ty), (Type::Null, _) | (_, Type::Null)) {
                        Type::boolean()
                    } else if lhs_ty.is_reference() && rhs_ty.is_reference() {
                        // Reference equality (JLS 15.21.3): requires that a cast conversion exists
                        // between the operand types (or they are identical).
                        if lhs_ty == rhs_ty
                            || cast_conversion(env_ro, &lhs_ty, &rhs_ty).is_some()
                            || cast_conversion(env_ro, &rhs_ty, &lhs_ty).is_some()
                        {
                            Type::boolean()
                        } else {
                            type_mismatch(self);
                            Type::Error
                        }
                    } else {
                        type_mismatch(self);
                        Type::Error
                    }
                }
            }

            // Relational operators always produce boolean.
            BinaryOp::Less | BinaryOp::LessEq | BinaryOp::Greater | BinaryOp::GreaterEq => {
                if lhs_ty.is_errorish() || rhs_ty.is_errorish() {
                    Type::boolean()
                } else {
                    let ok = matches!((lhs_prim, rhs_prim), (Some(a), Some(b)) if a.is_numeric() && b.is_numeric());
                    if ok {
                        Type::boolean()
                    } else {
                        type_mismatch(self);
                        Type::Error
                    }
                }
            }

            // `&&` / `||` always produce boolean.
            BinaryOp::AndAnd | BinaryOp::OrOr => {
                if lhs_ty.is_errorish() || rhs_ty.is_errorish() {
                    Type::boolean()
                } else {
                    let ok = matches!(
                        (lhs_prim, rhs_prim),
                        (Some(PrimitiveType::Boolean), Some(PrimitiveType::Boolean))
                    );
                    if ok {
                        Type::boolean()
                    } else {
                        type_mismatch(self);
                        Type::Error
                    }
                }
            }

            // `+` is special: numeric addition or string concatenation.
            BinaryOp::Add => {
                if lhs_ty == string_ty || rhs_ty == string_ty {
                    // String concatenation is only defined for non-void expressions.
                    if lhs_ty == Type::Void || rhs_ty == Type::Void {
                        type_mismatch(self);
                        Type::Error
                    } else {
                        string_ty.clone()
                    }
                } else if lhs_ty.is_errorish() || rhs_ty.is_errorish() {
                    Type::Unknown
                } else {
                    match (lhs_prim, rhs_prim) {
                        (Some(a), Some(b)) if a.is_numeric() && b.is_numeric() => {
                            binary_numeric_promotion(a, b)
                                .map(Type::Primitive)
                                .unwrap_or(Type::Unknown)
                        }
                        _ => {
                            type_mismatch(self);
                            Type::Error
                        }
                    }
                }
            }

            // Numeric operators.
            BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                if lhs_ty.is_errorish() || rhs_ty.is_errorish() {
                    Type::Unknown
                } else {
                    match (lhs_prim, rhs_prim) {
                        (Some(a), Some(b)) if a.is_numeric() && b.is_numeric() => {
                            binary_numeric_promotion(a, b)
                                .map(Type::Primitive)
                                .unwrap_or(Type::Unknown)
                        }
                        _ => {
                            type_mismatch(self);
                            Type::Error
                        }
                    }
                }
            }

            // Bitwise operators.
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                if lhs_ty.is_errorish() || rhs_ty.is_errorish() {
                    Type::Unknown
                } else {
                    match (lhs_prim, rhs_prim) {
                        (Some(PrimitiveType::Boolean), Some(PrimitiveType::Boolean)) => {
                            Type::boolean()
                        }
                        (Some(a), Some(b))
                            if matches!(
                                a,
                                PrimitiveType::Byte
                                    | PrimitiveType::Short
                                    | PrimitiveType::Char
                                    | PrimitiveType::Int
                                    | PrimitiveType::Long
                            ) && matches!(
                                b,
                                PrimitiveType::Byte
                                    | PrimitiveType::Short
                                    | PrimitiveType::Char
                                    | PrimitiveType::Int
                                    | PrimitiveType::Long
                            ) =>
                        {
                            binary_numeric_promotion(a, b)
                                .map(Type::Primitive)
                                .unwrap_or(Type::Unknown)
                        }
                        _ => {
                            type_mismatch(self);
                            Type::Error
                        }
                    }
                }
            }

            // Shift operators.
            BinaryOp::Shl | BinaryOp::Shr | BinaryOp::UShr => {
                if lhs_ty.is_errorish() || rhs_ty.is_errorish() {
                    Type::Unknown
                } else {
                    match (lhs_prim, rhs_prim) {
                        (Some(a), Some(b))
                            if matches!(
                                a,
                                PrimitiveType::Byte
                                    | PrimitiveType::Short
                                    | PrimitiveType::Char
                                    | PrimitiveType::Int
                                    | PrimitiveType::Long
                            ) && matches!(
                                b,
                                PrimitiveType::Byte
                                    | PrimitiveType::Short
                                    | PrimitiveType::Char
                                    | PrimitiveType::Int
                                    | PrimitiveType::Long
                            ) =>
                        {
                            match a {
                                // Unary numeric promotion for shift operations.
                                PrimitiveType::Long => Type::Primitive(PrimitiveType::Long),
                                PrimitiveType::Byte
                                | PrimitiveType::Short
                                | PrimitiveType::Char
                                | PrimitiveType::Int => Type::Primitive(PrimitiveType::Int),
                                PrimitiveType::Float
                                | PrimitiveType::Double
                                | PrimitiveType::Boolean => Type::Unknown,
                            }
                        }
                        _ => {
                            type_mismatch(self);
                            Type::Error
                        }
                    }
                }
            }
        };

        ExprInfo {
            ty,
            is_type_ref: false,
        }
    }

    fn class_literal_array_dims(&self, range: Span) -> usize {
        let file = def_file(self.owner);
        let file_text = self.db.file_content(file);
        let file_text: &str = file_text.as_ref();
        let len = file_text.len();
        let start = range.start.min(len);
        let end = range.end.min(len);
        if start >= end {
            return 0;
        }
        let Some(slice) = file_text.get(start..end) else {
            return 0;
        };

        // Tokenize the `T[].class` slice and count trailing `[]` pairs before `.class`.
        let tokens = nova_syntax::lex(slice);
        let mut kinds: Vec<SyntaxKind> = tokens
            .iter()
            .filter_map(|t| {
                let kind = t.kind;
                if kind.is_trivia() || kind == SyntaxKind::Eof {
                    None
                } else {
                    Some(kind)
                }
            })
            .collect();

        // Remove the trailing `. class` tokens.
        if kinds.len() < 2 {
            return 0;
        }
        let last = kinds.len() - 1;
        if kinds[last] != SyntaxKind::ClassKw || kinds[last - 1] != SyntaxKind::Dot {
            return 0;
        }
        kinds.truncate(last - 1);

        let mut dims = 0usize;
        while kinds.len() >= 2 {
            let last = kinds.len() - 1;
            if kinds[last] != SyntaxKind::RBracket || kinds[last - 1] != SyntaxKind::LBracket {
                break;
            }
            dims += 1;
            kinds.truncate(last - 1);
        }

        dims
    }

    fn resolve_source_type(
        &mut self,
        loader: &mut ExternalTypeLoader<'_>,
        text: &str,
        base_span: Option<Span>,
    ) -> Type {
        let resolved = resolve_type_ref_text(
            self.resolver,
            self.scopes,
            self.scope_id,
            loader,
            &self.type_vars,
            text,
            base_span,
        );
        extend_type_ref_diagnostics(
            &mut self.diagnostics,
            self.file_tokens,
            self.file_text,
            resolved.diagnostics,
        );
        resolved.ty
    }

    fn tick(&mut self) {
        cancel::checkpoint_cancelled_every(self.db, self.steps, 256);
        self.steps = self.steps.wrapping_add(1);
    }
}

fn peel_array_dims(mut ty: &Type) -> (&Type, usize) {
    let mut dims = 0usize;
    while let Type::Array(inner) = ty {
        dims += 1;
        ty = inner.as_ref();
    }
    (ty, dims)
}

fn primitive_like(env: &dyn TypeEnv, ty: &Type) -> Option<PrimitiveType> {
    primitive_like_inner(env, ty, 8)
}

fn boxed_primitive_type(loader: &mut ExternalTypeLoader<'_>, prim: PrimitiveType) -> Type {
    let name = match prim {
        PrimitiveType::Boolean => "java.lang.Boolean",
        PrimitiveType::Byte => "java.lang.Byte",
        PrimitiveType::Short => "java.lang.Short",
        PrimitiveType::Char => "java.lang.Character",
        PrimitiveType::Int => "java.lang.Integer",
        PrimitiveType::Long => "java.lang.Long",
        PrimitiveType::Float => "java.lang.Float",
        PrimitiveType::Double => "java.lang.Double",
    };
    let id = loader.store.intern_class_id(name);
    Type::class(id, vec![])
}

fn primitive_like_inner(env: &dyn TypeEnv, ty: &Type, depth: u8) -> Option<PrimitiveType> {
    if depth == 0 {
        return None;
    }

    match ty {
        Type::Primitive(p) => Some(*p),
        Type::Class(nova_types::ClassType { def, .. }) => {
            env.class(*def).and_then(|c| unbox_class_name(&c.name))
        }
        Type::Named(name) => unbox_class_name(name),
        Type::TypeVar(id) => env.type_param(*id).and_then(|tp| {
            tp.upper_bounds
                .iter()
                .find_map(|b| primitive_like_inner(env, b, depth.saturating_sub(1)))
        }),
        Type::Intersection(types) => types
            .iter()
            .find_map(|t| primitive_like_inner(env, t, depth.saturating_sub(1))),
        _ => None,
    }
}

fn unbox_class_name(name: &str) -> Option<PrimitiveType> {
    Some(match name {
        "java.lang.Boolean" => PrimitiveType::Boolean,
        "java.lang.Byte" => PrimitiveType::Byte,
        "java.lang.Short" => PrimitiveType::Short,
        "java.lang.Character" => PrimitiveType::Char,
        "java.lang.Integer" => PrimitiveType::Int,
        "java.lang.Long" => PrimitiveType::Long,
        "java.lang.Float" => PrimitiveType::Float,
        "java.lang.Double" => PrimitiveType::Double,
        _ => return None,
    })
}
fn params_for_owner(tree: &nova_hir::item_tree::ItemTree, owner: DefWithBodyId) -> Vec<ParamId> {
    match owner {
        DefWithBodyId::Method(m) => tree
            .method(m)
            .params
            .iter()
            .enumerate()
            .map(|(idx, _)| ParamId::new(owner, idx as u32))
            .collect(),
        DefWithBodyId::Constructor(c) => tree
            .constructor(c)
            .params
            .iter()
            .enumerate()
            .map(|(idx, _)| ParamId::new(owner, idx as u32))
            .collect(),
        DefWithBodyId::Initializer(_) => Vec::new(),
    }
}

fn resolve_expected_return_type<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    tree: &nova_hir::item_tree::ItemTree,
    owner: DefWithBodyId,
    type_vars: &HashMap<String, TypeVarId>,
    loader: &mut ExternalTypeLoader<'_>,
) -> (Type, Vec<Diagnostic>) {
    match owner {
        DefWithBodyId::Method(m) => {
            let method = tree.method(m);
            let resolved = resolve_type_ref_text(
                resolver,
                scopes,
                scope_id,
                loader,
                type_vars,
                &method.return_ty,
                Some(method.return_ty_range),
            );
            (resolved.ty, resolved.diagnostics)
        }
        DefWithBodyId::Constructor(_) | DefWithBodyId::Initializer(_) => (Type::Void, Vec::new()),
    }
}

fn resolve_param_types<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    tree: &nova_hir::item_tree::ItemTree,
    owner: DefWithBodyId,
    type_vars: &HashMap<String, TypeVarId>,
    loader: &mut ExternalTypeLoader<'_>,
) -> (Vec<Type>, Vec<Diagnostic>) {
    let mut out = Vec::new();
    let mut diags = Vec::new();

    let params = match owner {
        DefWithBodyId::Method(m) => tree.method(m).params.as_slice(),
        DefWithBodyId::Constructor(c) => tree.constructor(c).params.as_slice(),
        DefWithBodyId::Initializer(_) => &[],
    };

    let is_varargs = params
        .last()
        .is_some_and(|p| p.is_varargs || p.ty.trim().contains("..."));

    for (idx, param) in params.iter().enumerate() {
        let is_varargs_param = is_varargs && idx + 1 == params.len();
        let ty_text = param.ty.trim_end();
        let ty_text_has_ellipsis = ty_text.contains("...");

        let resolved = resolve_type_ref_text(
            resolver,
            scopes,
            scope_id,
            loader,
            type_vars,
            ty_text,
            Some(param.ty_range),
        );
        diags.extend(resolved.diagnostics);

        if resolved.ty == Type::Void {
            diags.push(Diagnostic::error(
                "void-parameter-type",
                "`void` is not a valid parameter type",
                Some(param.ty_range),
            ));
            diags.push(Diagnostic::error(
                "invalid-void-type",
                "`void` is not a valid parameter type",
                Some(param.ty_range),
            ));
            out.push(Type::Error);
            continue;
        }

        // If the signature text already includes the `...` token, `resolve_type_ref_text` will
        // model it as an array dimension. Only synthesize an extra array layer when we tracked
        // varargs out-of-band (via `Param::is_varargs`).
        let ty = if is_varargs_param && !ty_text_has_ellipsis {
            Type::Array(Box::new(resolved.ty))
        } else {
            resolved.ty
        };
        out.push(ty);
    }

    (out, diags)
}

fn resolve_owner_type_param_bounds<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    tree: &nova_hir::item_tree::ItemTree,
    owner: DefWithBodyId,
    type_vars: &HashMap<String, TypeVarId>,
    loader: &mut ExternalTypeLoader<'_>,
) -> Vec<Diagnostic> {
    let type_params = match owner {
        DefWithBodyId::Method(m) => tree.method(m).type_params.as_slice(),
        DefWithBodyId::Constructor(c) => tree.constructor(c).type_params.as_slice(),
        DefWithBodyId::Initializer(_) => &[],
    };

    let mut diags = Vec::new();
    for tp in type_params {
        for (idx, bound) in tp.bounds.iter().enumerate() {
            let bound_range = tp.bounds_ranges.get(idx).copied();
            let resolved = resolve_type_ref_text(
                resolver,
                scopes,
                scope_id,
                loader,
                type_vars,
                bound,
                bound_range,
            );
            diags.extend(resolved.diagnostics);
        }
    }

    diags
}

fn resolve_owner_throws_clause_types<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    tree: &nova_hir::item_tree::ItemTree,
    owner: DefWithBodyId,
    type_vars: &HashMap<String, TypeVarId>,
    loader: &mut ExternalTypeLoader<'_>,
) -> Vec<Diagnostic> {
    let (throws, throws_ranges) = match owner {
        DefWithBodyId::Method(m) => {
            let method = tree.method(m);
            (method.throws.as_slice(), method.throws_ranges.as_slice())
        }
        DefWithBodyId::Constructor(c) => {
            let ctor = tree.constructor(c);
            (ctor.throws.as_slice(), ctor.throws_ranges.as_slice())
        }
        DefWithBodyId::Initializer(_) => (&[][..], &[][..]),
    };

    let mut diags = Vec::new();
    for (idx, thrown) in throws.iter().enumerate() {
        let range = throws_ranges.get(idx).copied();
        let resolved =
            resolve_type_ref_text(resolver, scopes, scope_id, loader, type_vars, thrown, range);
        diags.extend(resolved.diagnostics);
    }

    diags
}

fn resolve_type_ref_text<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    loader: &mut ExternalTypeLoader<'_>,
    type_vars: &HashMap<String, TypeVarId>,
    text: &str,
    base_span: Option<Span>,
) -> nova_resolve::type_ref::ResolvedType {
    // Preload referenced types so the type-ref resolver can map them into `Type::Class` where
    // possible.
    //
    // We still pass the original text into `resolve_type_ref_text` so the parser can correctly
    // skip type-use annotations even when `TypeRef.text` is whitespace-stripped (e.g. `@A String`
    // becomes `@AString`). Type-use annotations don't currently contribute to `nova_types::Type`,
    // but we still resolve their annotation type names for signature diagnostics.
    preload_type_names(resolver, scopes, scope_id, loader, text);
    let mut resolved = nova_resolve::type_ref::resolve_type_ref_text(
        resolver,
        scopes,
        scope_id,
        &*loader.store,
        type_vars,
        text,
        base_span,
    );
    strip_type_use_annotation_type_diagnostics(text, base_span, &mut resolved.diagnostics);
    resolved
}

fn strip_type_use_annotation_type_diagnostics(
    text: &str,
    base_span: Option<Span>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(base_span) = base_span else {
        return;
    };
    if !text.as_bytes().contains(&b'@') {
        return;
    }

    diagnostics.retain(|diag| {
        if diag.code.as_ref() != "unresolved-type" {
            return true;
        }
        let Some(span) = diag.span else {
            return true;
        };
        if span.start < base_span.start {
            return true;
        }
        let local_start = span.start - base_span.start;
        if local_start == 0 || local_start > text.len() || !text.is_char_boundary(local_start) {
            return true;
        }

        // Type-use annotations are currently syntax-only in Nova; suppress unresolved-type errors
        // for the annotation *type* name (e.g. `List<@Missing String>`). We keep the underlying
        // type reference diagnostics (e.g. unresolved `List` or `String`) intact.
        let prev_non_ws = text[..local_start]
            .chars()
            .rev()
            .find(|ch| !ch.is_whitespace());
        !matches!(prev_non_ws, Some('@'))
    });
}

#[derive(Debug, Default)]
pub struct SourceTypes {
    field_types: HashMap<FieldId, Type>,
    field_owners: HashMap<FieldId, String>,
    method_owners: HashMap<MethodId, String>,
    source_type_vars: SourceTypeVars,
}

impl SourceTypes {
    /// Approximate heap memory usage of this structure in bytes.
    ///
    /// This intentionally favors stable, cheap accounting over precision and mirrors the
    /// `TypeStore::estimated_bytes` philosophy.
    #[must_use]
    fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        const AVG_OWNER_NAME_BYTES: u64 = 64;
        const AVG_TYPE_PARAM_NAME_BYTES: u64 = 16;
        const AVG_TYPE_PARAMS_PER_ITEM: u64 = 2;

        let mut bytes = 0u64;

        // `HashMap` backing storage (best-effort; ignores per-entry overhead beyond key/value).
        bytes = bytes
            .saturating_add((self.field_types.capacity() * size_of::<(FieldId, Type)>()) as u64);
        bytes = bytes.saturating_add(self.field_types.capacity() as u64); // control bytes heuristic

        bytes = bytes
            .saturating_add((self.field_owners.capacity() * size_of::<(FieldId, String)>()) as u64);
        bytes = bytes.saturating_add(self.field_owners.capacity() as u64);
        bytes = bytes.saturating_add(self.field_owners.capacity() as u64 * AVG_OWNER_NAME_BYTES);

        bytes = bytes.saturating_add(
            (self.method_owners.capacity() * size_of::<(MethodId, String)>()) as u64,
        );
        bytes = bytes.saturating_add(self.method_owners.capacity() as u64);
        bytes = bytes.saturating_add(self.method_owners.capacity() as u64 * AVG_OWNER_NAME_BYTES);

        bytes = bytes.saturating_add(
            (self.source_type_vars.classes.capacity()
                * size_of::<(nova_hir::ids::ItemId, Vec<(String, TypeVarId)>)>())
                as u64,
        );
        bytes = bytes.saturating_add(self.source_type_vars.classes.capacity() as u64);

        bytes = bytes.saturating_add(
            (self.source_type_vars.methods.capacity()
                * size_of::<(MethodId, Vec<(String, TypeVarId)>)>()) as u64,
        );
        bytes = bytes.saturating_add(self.source_type_vars.methods.capacity() as u64);

        // Inner vectors + names (best-effort heuristic).
        let class_items = self.source_type_vars.classes.len() as u64;
        let method_items = self.source_type_vars.methods.len() as u64;
        let approx_param_entries = class_items
            .saturating_add(method_items)
            .saturating_mul(AVG_TYPE_PARAMS_PER_ITEM);
        bytes = bytes.saturating_add(
            approx_param_entries
                * (size_of::<(String, TypeVarId)>() as u64 + AVG_TYPE_PARAM_NAME_BYTES),
        );

        bytes
    }

    fn extend(&mut self, other: SourceTypes) {
        self.field_types.extend(other.field_types);
        self.field_owners.extend(other.field_owners);
        self.method_owners.extend(other.method_owners);
        self.source_type_vars
            .classes
            .extend(other.source_type_vars.classes);
        self.source_type_vars
            .methods
            .extend(other.source_type_vars.methods);
    }
}

fn define_workspace_source_types<'idx>(
    db: &dyn NovaTypeck,
    project: ProjectId,
    from_file: FileId,
    resolver: &nova_resolve::Resolver<'idx>,
    loader: &mut ExternalTypeLoader<'_>,
) -> SourceTypes {
    let files = db.project_files(project);

    fn jpms_workspace_type_accessible(
        graph: &nova_modules::ModuleGraph,
        workspace: &nova_resolve::WorkspaceDefMap,
        from: &ModuleName,
        item: nova_hir::ids::ItemId,
        binary_name: &str,
    ) -> bool {
        let to = workspace
            .module_for_item(item)
            .cloned()
            .unwrap_or_else(ModuleName::unnamed);
        if !graph.can_read(from, &to) {
            return false;
        }

        let package = binary_name
            .rsplit_once('.')
            .map(|(pkg, _)| pkg)
            .unwrap_or("");
        match graph.get(&to) {
            Some(info) => info.exports_package_to(package, from),
            // Unknown modules default to accessible so partial graphs don't cascade into
            // false-negative resolution failures (mirror `JpmsProjectIndex`).
            None => true,
        }
    }

    let workspace = db.workspace_def_map(project);

    // First pass: intern ids for every workspace type in deterministic order so cross-file
    // references can resolve to `Type::Class` during member signature resolution.
    //
    // In JPMS mode we still intern ids broadly for stability, but we *do not* define member-bearing
    // `ClassDef`s for unreadable/unexported types below.
    let jpms_env = db.jpms_compilation_env(project);
    let (from_module, graph) = jpms_env
        .as_deref()
        .map(|env| {
            let cfg = db.project_config(project);
            let file_rel = db.file_rel_path(from_file);
            let from = module_for_file(&cfg, file_rel.as_str());
            (from, &env.env.graph)
        })
        .unzip();

    let mut accessible_files: Option<HashSet<FileId>> = None;
    if let (Some(from), Some(graph)) = (from_module.as_ref(), graph.as_ref()) {
        let mut set = HashSet::new();
        set.insert(from_file);
        for (idx, name) in workspace.iter_type_names().enumerate() {
            cancel::checkpoint_cancelled_every(db, idx as u32, 4096);
            let binary = name.as_str();
            loader.store.intern_class_id(binary);
            let Some(item) = workspace.item_by_type_name(name) else {
                continue;
            };
            if jpms_workspace_type_accessible(graph, &workspace, from, item, binary) {
                set.insert(item.file());
            }
        }
        accessible_files = Some(set);
    } else {
        for (idx, name) in workspace.iter_type_names().enumerate() {
            cancel::checkpoint_cancelled_every(db, idx as u32, 4096);
            loader.store.intern_class_id(name.as_str());
        }
    }

    // Second pass: define skeleton class defs + collect member typing/ownership info.
    //
    // In JPMS mode, only expose members for workspace types that are accessible from the module
    // owning `from_file`. This prevents member resolution from "rescuing" otherwise
    // `unresolved-type` references to unreadable/unexported workspace modules.
    let mut out = SourceTypes::default();
    for (idx, file) in files.iter().copied().enumerate() {
        cancel::checkpoint_cancelled_every(db, idx as u32, 32);
        if let Some(allowed) = accessible_files.as_ref() {
            // In JPMS mode, only materialize member-bearing `ClassDef`s for workspace source types
            // that are actually accessible from the current module. Unreadable/unexported types
            // remain as inert placeholders created by `intern_class_id`, preventing member lookup
            // from "rescuing" unresolved types.
            if !allowed.contains(&file) {
                continue;
            }
        }
        let tree = db.hir_item_tree(file);
        let scopes = db.scope_graph(file);
        out.extend(define_source_types(resolver, &scopes, &tree, loader));
    }

    out
}

fn param_name_lookup(tree: &nova_hir::item_tree::ItemTree, id: ParamId) -> Name {
    match id.owner {
        DefWithBodyId::Method(m) => tree
            .method(m)
            .params
            .get(id.index as usize)
            .map(|p| Name::from(p.name.as_str()))
            .unwrap_or_else(|| Name::from("<param>")),
        DefWithBodyId::Constructor(c) => tree
            .constructor(c)
            .params
            .get(id.index as usize)
            .map(|p| Name::from(p.name.as_str()))
            .unwrap_or_else(|| Name::from("<param>")),
        DefWithBodyId::Initializer(_) => Name::from("<param>"),
    }
}

#[derive(Debug, Default)]
pub struct SourceTypeVars {
    classes: HashMap<nova_hir::ids::ItemId, Vec<(String, TypeVarId)>>,
    methods: HashMap<MethodId, Vec<(String, TypeVarId)>>,
}

fn enclosing_class_item(
    scopes: &nova_resolve::ScopeGraph,
    mut scope_id: nova_resolve::ScopeId,
) -> Option<nova_hir::ids::ItemId> {
    loop {
        match scopes.scope(scope_id).kind() {
            ScopeKind::Class { item } => return Some(*item),
            _ => scope_id = scopes.scope(scope_id).parent()?,
        }
    }
}

fn type_vars_for_owner<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    owner: DefWithBodyId,
    body_scope: nova_resolve::ScopeId,
    scopes: &nova_resolve::ScopeGraph,
    tree: &nova_hir::item_tree::ItemTree,
    loader: &mut ExternalTypeLoader<'_>,
    source_type_vars: &SourceTypeVars,
) -> HashMap<String, TypeVarId> {
    let mut vars = HashMap::new();

    if let Some(class_item) = enclosing_class_item(scopes, body_scope) {
        if let Some(type_params) = source_type_vars.classes.get(&class_item) {
            for (name, id) in type_params {
                vars.insert(name.clone(), *id);
            }
        }
    }

    match owner {
        DefWithBodyId::Method(m) => {
            if let Some(type_params) = source_type_vars.methods.get(&m) {
                for (name, id) in type_params {
                    vars.insert(name.clone(), *id);
                }
            }
        }
        DefWithBodyId::Constructor(c) => {
            let object_ty = Type::class(loader.store.well_known().object, vec![]);
            let _ = allocate_type_params(
                resolver,
                scopes,
                body_scope,
                loader,
                &object_ty,
                &tree.constructor(c).type_params,
                &mut vars,
            );
        }
        DefWithBodyId::Initializer(_) => {}
    }

    vars
}

fn item_type_params<'a>(
    tree: &'a nova_hir::item_tree::ItemTree,
    item: nova_hir::ids::ItemId,
) -> &'a [nova_hir::item_tree::TypeParam] {
    match item {
        nova_hir::ids::ItemId::Class(id) => tree.class(id).type_params.as_slice(),
        nova_hir::ids::ItemId::Interface(id) => tree.interface(id).type_params.as_slice(),
        nova_hir::ids::ItemId::Record(id) => tree.record(id).type_params.as_slice(),
        _ => &[],
    }
}

fn allocate_type_params<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    loader: &mut ExternalTypeLoader<'_>,
    default_bound: &Type,
    type_params: &[nova_hir::item_tree::TypeParam],
    vars: &mut HashMap<String, TypeVarId>,
) -> Vec<(String, TypeVarId)> {
    let mut allocated = Vec::new();

    // First pass: allocate ids so bounds can refer to any type param in the list (including
    // self-referential ones like `E extends Enum<E>`).
    for tp in type_params {
        let id = loader
            .store
            .add_type_param(tp.name.clone(), vec![default_bound.clone()]);
        vars.insert(tp.name.clone(), id);
        allocated.push((tp.name.clone(), id));
    }

    // Second pass: resolve bounds and overwrite the placeholder definitions.
    for tp in type_params {
        let id = match vars.get(&tp.name) {
            Some(id) => *id,
            None => continue,
        };

        let mut upper_bounds = Vec::new();
        if tp.bounds.is_empty() {
            upper_bounds.push(default_bound.clone());
        } else {
            for bound in &tp.bounds {
                preload_type_names(resolver, scopes, scope_id, loader, bound);
                let ty = nova_resolve::type_ref::resolve_type_ref_text(
                    resolver,
                    scopes,
                    scope_id,
                    &*loader.store,
                    vars,
                    bound,
                    None,
                )
                .ty;
                upper_bounds.push(ty);
            }
        }

        if upper_bounds.is_empty() {
            upper_bounds.push(default_bound.clone());
        }

        loader.store.define_type_param(
            id,
            TypeParamDef {
                name: tp.name.clone(),
                upper_bounds,
                lower_bound: None,
            },
        );
    }

    allocated
}

fn source_item_supertypes<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    loader: &mut ExternalTypeLoader<'_>,
    type_vars: &HashMap<String, TypeVarId>,
    tree: &nova_hir::item_tree::ItemTree,
    item: nova_hir::ids::ItemId,
    binary_name: &str,
    self_class_id: ClassId,
) -> (ClassKind, Option<Type>, Vec<Type>) {
    let object_ty = Type::class(loader.store.well_known().object, vec![]);
    fn ensure_non_placeholder(
        loader: &mut ExternalTypeLoader<'_>,
        binary_name: &str,
    ) -> Option<ClassId> {
        let id = loader.ensure_class(binary_name)?;
        let def = loader.store.class(id)?;
        (!is_placeholder_class_def(def)).then_some(id)
    }

    let mut kind = match item {
        nova_hir::ids::ItemId::Interface(_) | nova_hir::ids::ItemId::Annotation(_) => {
            ClassKind::Interface
        }
        _ => ClassKind::Class,
    };

    let mut super_class: Option<Type> = None;
    let mut interfaces: Vec<Type> = Vec::new();

    // Only accept "real" class/interface types for supertypes. In broken code, `resolve_type_ref_text`
    // can yield primitives/arrays/etc (e.g. `extends int`), and unresolved names yield `Type::Named`
    // plus an `unresolved-type` diagnostic. For IDE resilience, treat those as missing and fall back
    // to the normal defaults (`Object` for classes, none for interfaces).
    let accept_supertype = |resolved: nova_resolve::type_ref::ResolvedType| -> Option<Type> {
        if resolved.ty.is_errorish() {
            return None;
        }

        // `unresolved-type` diagnostics may originate from inside type arguments
        // (e.g. `extends ArrayList<Missing>`). Those cases should still keep the
        // outer supertype so inherited members can be discovered.
        //
        // Only reject when the *supertype itself* is unresolved (which typically
        // yields `Type::Named` and an `unresolved-type` diagnostic).
        let has_unresolved = resolved
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type");

        match resolved.ty {
            Type::Class(_) | Type::VirtualInner { .. } => Some(resolved.ty),
            Type::Named(_) if !has_unresolved => Some(resolved.ty),
            Type::Named(_) => None,
            _ => None,
        }
    };

    match item {
        nova_hir::ids::ItemId::Class(id) => {
            let class = tree.class(id);

            if let Some(ext) = class.extends.first() {
                let base_span = class.extends_ranges.first().copied();
                let resolved = resolve_type_ref_text(
                    resolver, scopes, scope_id, loader, type_vars, ext, base_span,
                );
                if let Some(ty) = accept_supertype(resolved) {
                    super_class = Some(ty);
                }
            }

            if super_class.is_none() && binary_name != "java.lang.Object" {
                super_class = Some(object_ty.clone());
            }

            for (idx, imp) in class.implements.iter().enumerate() {
                let base_span = class.implements_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver, scopes, scope_id, loader, type_vars, imp, base_span,
                );
                if let Some(ty) = accept_supertype(resolved) {
                    interfaces.push(ty);
                }
            }
        }
        nova_hir::ids::ItemId::Interface(id) => {
            kind = ClassKind::Interface;
            let iface = tree.interface(id);
            for (idx, ext) in iface.extends.iter().enumerate() {
                let base_span = iface.extends_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver, scopes, scope_id, loader, type_vars, ext, base_span,
                );
                if let Some(ty) = accept_supertype(resolved) {
                    interfaces.push(ty);
                }
            }
            super_class = None;
        }
        nova_hir::ids::ItemId::Annotation(_) => {
            kind = ClassKind::Interface;
            super_class = None;

            // Best-effort: annotation types implicitly extend `java.lang.annotation.Annotation`.
            if let Some(ann_id) = ensure_non_placeholder(loader, "java.lang.annotation.Annotation")
            {
                interfaces.push(Type::class(ann_id, vec![]));
            }
        }
        nova_hir::ids::ItemId::Enum(id) => {
            kind = ClassKind::Class;

            // Best-effort: enums implicitly extend `java.lang.Enum<Self>`.
            if let Some(enum_id) = ensure_non_placeholder(loader, "java.lang.Enum") {
                let self_ty = Type::class(self_class_id, vec![]);
                super_class = Some(Type::class(enum_id, vec![self_ty]));
            }

            let enm = tree.enum_(id);
            for (idx, imp) in enm.implements.iter().enumerate() {
                let base_span = enm.implements_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver, scopes, scope_id, loader, type_vars, imp, base_span,
                );
                if let Some(ty) = accept_supertype(resolved) {
                    interfaces.push(ty);
                }
            }
        }
        nova_hir::ids::ItemId::Record(id) => {
            kind = ClassKind::Class;

            // Best-effort: records implicitly extend `java.lang.Record`.
            if let Some(record_id) = ensure_non_placeholder(loader, "java.lang.Record") {
                super_class = Some(Type::class(record_id, vec![]));
            }

            let record = tree.record(id);
            for (idx, imp) in record.implements.iter().enumerate() {
                let base_span = record.implements_ranges.get(idx).copied();
                let resolved = resolve_type_ref_text(
                    resolver, scopes, scope_id, loader, type_vars, imp, base_span,
                );
                if let Some(ty) = accept_supertype(resolved) {
                    interfaces.push(ty);
                }
            }
        }
    }

    // Preserve `Object` as the default supertype for classes if we failed to resolve an explicit
    // superclass due to errors.
    if kind == ClassKind::Class && super_class.is_none() && binary_name != "java.lang.Object" {
        super_class = Some(object_ty);
    }

    (kind, super_class, interfaces)
}

fn define_source_types<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ItemTreeScopeBuildResult,
    tree: &nova_hir::item_tree::ItemTree,
    loader: &mut ExternalTypeLoader<'_>,
) -> SourceTypes {
    let mut items = Vec::new();
    for item in &tree.items {
        collect_item_ids(tree, *item, &mut items);
    }

    // First pass: intern ids.
    for item in &items {
        if let Some(name) = scopes.scopes.type_name(*item) {
            loader.store.intern_class_id(name.as_str());
        }
    }

    let mut field_types = HashMap::new();
    let mut field_owners = HashMap::new();
    let mut method_owners = HashMap::new();
    let mut source_type_vars = SourceTypeVars::default();

    // Second pass: define skeleton class defs.
    for item in items {
        let Some(name) = scopes
            .scopes
            .type_name(item)
            .map(|t| t.as_str().to_string())
        else {
            continue;
        };

        // Mirror the resolver's `java.*` handling: application class loaders cannot define
        // `java.*` packages, so even if a workspace source file declares a `java.*` type we
        // must not expose its members to downstream type checking (it would otherwise be able to
        // "rescue" unresolved `java.*` references).
        //
        // Keep the placeholder `ClassDef` allocated by `intern_class_id` so `ClassId`s remain
        // stable for any already-interned references, but do not define a real class body.
        if name.starts_with("java.") {
            continue;
        }

        let class_id = loader.store.intern_class_id(&name);
        let object_ty = Type::class(loader.store.well_known().object, vec![]);

        let class_scope = scopes
            .class_scopes
            .get(&item)
            .copied()
            .unwrap_or(scopes.file_scope);

        let mut class_vars = HashMap::new();
        let class_type_params = item_type_params(tree, item);
        let class_type_params = allocate_type_params(
            resolver,
            &scopes.scopes,
            class_scope,
            loader,
            &object_ty,
            class_type_params,
            &mut class_vars,
        );
        source_type_vars
            .classes
            .insert(item, class_type_params.clone());
        let (kind, super_class, interfaces) = source_item_supertypes(
            resolver,
            &scopes.scopes,
            class_scope,
            loader,
            &class_vars,
            tree,
            item,
            &name,
            class_id,
        );

        let mut fields = Vec::new();
        let mut constructors = Vec::new();
        let mut methods = Vec::new();
        for member in item_members(tree, item) {
            match member {
                nova_hir::item_tree::Member::Field(fid) => {
                    let field = tree.field(*fid);
                    preload_type_names(resolver, &scopes.scopes, class_scope, loader, &field.ty);
                    let ty = nova_resolve::type_ref::resolve_type_ref_text(
                        resolver,
                        &scopes.scopes,
                        class_scope,
                        &*loader.store,
                        &class_vars,
                        &field.ty,
                        Some(field.ty_range),
                    )
                    .ty;
                    field_types.insert(*fid, ty.clone());
                    field_owners.insert(*fid, name.clone());
                    let is_implicitly_static =
                        field.kind == FieldKind::EnumConstant || kind == ClassKind::Interface;
                    let is_static = is_implicitly_static
                        || field.modifiers.raw & nova_hir::item_tree::Modifiers::STATIC != 0;
                    let is_final = is_implicitly_static
                        || field.modifiers.raw & nova_hir::item_tree::Modifiers::FINAL != 0;
                    fields.push(FieldDef {
                        name: field.name.clone(),
                        ty,
                        is_static,
                        is_final,
                    });
                }
                nova_hir::item_tree::Member::Method(mid) => {
                    let method = tree.method(*mid);
                    let scope = scopes
                        .method_scopes
                        .get(mid)
                        .copied()
                        .unwrap_or(class_scope);
                    let mut vars = class_vars.clone();
                    let type_params = allocate_type_params(
                        resolver,
                        &scopes.scopes,
                        scope,
                        loader,
                        &object_ty,
                        &method.type_params,
                        &mut vars,
                    );
                    source_type_vars.methods.insert(*mid, type_params.clone());
                    let method_type_param_ids: Vec<TypeVarId> =
                        type_params.iter().map(|(_, id)| *id).collect();

                    let is_varargs = method
                        .params
                        .last()
                        .is_some_and(|p| p.is_varargs || p.ty.trim().contains("..."));

                    let params = method
                        .params
                        .iter()
                        .enumerate()
                        .map(|(idx, p)| {
                            let is_varargs_param = is_varargs && idx + 1 == method.params.len();
                            let ty_text = p.ty.trim_end();
                            let ty_text_has_ellipsis = ty_text.contains("...");

                            preload_type_names(resolver, &scopes.scopes, scope, loader, ty_text);
                            let ty = nova_resolve::type_ref::resolve_type_ref_text(
                                resolver,
                                &scopes.scopes,
                                scope,
                                &*loader.store,
                                &vars,
                                ty_text,
                                Some(p.ty_range),
                            )
                            .ty;
                            if is_varargs_param && !ty_text_has_ellipsis {
                                Type::Array(Box::new(ty))
                            } else {
                                ty
                            }
                        })
                        .collect::<Vec<_>>();

                    preload_type_names(resolver, &scopes.scopes, scope, loader, &method.return_ty);
                    let return_type = nova_resolve::type_ref::resolve_type_ref_text(
                        resolver,
                        &scopes.scopes,
                        scope,
                        &*loader.store,
                        &vars,
                        &method.return_ty,
                        Some(method.return_ty_range),
                    )
                    .ty;
                    method_owners.insert(*mid, name.clone());
                    let is_static =
                        method.modifiers.raw & nova_hir::item_tree::Modifiers::STATIC != 0;

                    methods.push(MethodDef {
                        name: method.name.clone(),
                        type_params: method_type_param_ids,
                        params,
                        return_type,
                        is_static,
                        is_varargs,
                        is_abstract: method.body.is_none(),
                    });
                }
                nova_hir::item_tree::Member::Constructor(cid) => {
                    let ctor = tree.constructor(*cid);
                    let scope = scopes
                        .constructor_scopes
                        .get(cid)
                        .copied()
                        .unwrap_or(class_scope);
                    // Constructors can refer to the enclosing class type parameters.
                    let vars = class_vars.clone();

                    let is_varargs = ctor
                        .params
                        .last()
                        .is_some_and(|p| p.is_varargs || p.ty.trim().contains("..."));

                    let params = ctor
                        .params
                        .iter()
                        .enumerate()
                        .map(|(idx, p)| {
                            let is_varargs_param = is_varargs && idx + 1 == ctor.params.len();
                            let ty_text = p.ty.trim_end();
                            let ty_text_has_ellipsis = ty_text.contains("...");

                            preload_type_names(resolver, &scopes.scopes, scope, loader, ty_text);
                            let ty = nova_resolve::type_ref::resolve_type_ref_text(
                                resolver,
                                &scopes.scopes,
                                scope,
                                &*loader.store,
                                &vars,
                                ty_text,
                                Some(p.ty_range),
                            )
                            .ty;
                            if is_varargs_param && !ty_text_has_ellipsis {
                                Type::Array(Box::new(ty))
                            } else {
                                ty
                            }
                        })
                        .collect::<Vec<_>>();

                    let is_accessible = ctor.modifiers.raw & Modifiers::PRIVATE == 0;
                    constructors.push(ConstructorDef {
                        params,
                        is_varargs,
                        is_accessible,
                    });
                }
                _ => {}
            }
        }

        // Best-effort: Java implicit constructors.
        //
        // - Classes with no declared constructors get an implicit no-arg constructor.
        // - Records always have a canonical constructor matching their components; if none was
        //   declared (or if only non-canonical ctors were declared), add it.
        match item {
            nova_hir::ids::ItemId::Class(_) if constructors.is_empty() => {
                constructors.push(ConstructorDef {
                    params: Vec::new(),
                    is_varargs: false,
                    is_accessible: true,
                });
            }
            nova_hir::ids::ItemId::Record(id) => {
                let record = tree.record(id);
                let canonical_params = record
                    .components
                    .iter()
                    .map(|component| {
                        preload_type_names(
                            resolver,
                            &scopes.scopes,
                            class_scope,
                            loader,
                            &component.ty,
                        );
                        nova_resolve::type_ref::resolve_type_ref_text(
                            resolver,
                            &scopes.scopes,
                            class_scope,
                            &*loader.store,
                            &class_vars,
                            &component.ty,
                            Some(component.ty_range),
                        )
                        .ty
                    })
                    .collect::<Vec<_>>();

                let used_ellipsis = record
                    .components
                    .last()
                    .is_some_and(|component| component.ty.trim().contains("..."));
                let last_is_array = canonical_params
                    .last()
                    .is_some_and(|t| matches!(t, Type::Array(_)));
                let canonical_is_varargs = used_ellipsis && last_is_array;

                let canonical_exists = constructors.iter().any(|ctor| {
                    ctor.params == canonical_params && ctor.is_varargs == canonical_is_varargs
                });
                if !canonical_exists {
                    let is_accessible = record.modifiers.raw & Modifiers::PRIVATE == 0;
                    constructors.push(ConstructorDef {
                        params: canonical_params,
                        is_varargs: canonical_is_varargs,
                        is_accessible,
                    });
                }
            }
            _ => {}
        }

        loader.store.define_class(
            class_id,
            ClassDef {
                name,
                kind,
                type_params: class_type_params.iter().map(|(_, id)| *id).collect(),
                super_class,
                interfaces,
                fields,
                constructors,
                methods,
            },
        );
    }

    SourceTypes {
        field_types,
        field_owners,
        method_owners,
        source_type_vars,
    }
}

fn item_members<'a>(
    tree: &'a nova_hir::item_tree::ItemTree,
    item: nova_hir::ids::ItemId,
) -> &'a [nova_hir::item_tree::Member] {
    match item {
        nova_hir::ids::ItemId::Class(id) => &tree.class(id).members,
        nova_hir::ids::ItemId::Interface(id) => &tree.interface(id).members,
        nova_hir::ids::ItemId::Enum(id) => &tree.enum_(id).members,
        nova_hir::ids::ItemId::Record(id) => &tree.record(id).members,
        nova_hir::ids::ItemId::Annotation(id) => &tree.annotation(id).members,
    }
}

fn collect_item_ids(
    tree: &nova_hir::item_tree::ItemTree,
    item: nova_hir::item_tree::Item,
    out: &mut Vec<nova_hir::ids::ItemId>,
) {
    let id = match item {
        nova_hir::item_tree::Item::Class(id) => nova_hir::ids::ItemId::Class(id),
        nova_hir::item_tree::Item::Interface(id) => nova_hir::ids::ItemId::Interface(id),
        nova_hir::item_tree::Item::Enum(id) => nova_hir::ids::ItemId::Enum(id),
        nova_hir::item_tree::Item::Record(id) => nova_hir::ids::ItemId::Record(id),
        nova_hir::item_tree::Item::Annotation(id) => nova_hir::ids::ItemId::Annotation(id),
    };
    out.push(id);
    for member in item_members(tree, id) {
        if let nova_hir::item_tree::Member::Type(child) = member {
            collect_item_ids(tree, *child, out);
        }
    }
}

fn collect_body_owners(tree: &nova_hir::item_tree::ItemTree) -> Vec<DefWithBodyId> {
    let mut owners = Vec::new();
    for item in &tree.items {
        collect_body_owners_in_item(tree, *item, &mut owners);
    }
    owners
}

fn collect_body_owners_in_item(
    tree: &nova_hir::item_tree::ItemTree,
    item: nova_hir::item_tree::Item,
    out: &mut Vec<DefWithBodyId>,
) {
    let id = match item {
        nova_hir::item_tree::Item::Class(id) => nova_hir::ids::ItemId::Class(id),
        nova_hir::item_tree::Item::Interface(id) => nova_hir::ids::ItemId::Interface(id),
        nova_hir::item_tree::Item::Enum(id) => nova_hir::ids::ItemId::Enum(id),
        nova_hir::item_tree::Item::Record(id) => nova_hir::ids::ItemId::Record(id),
        nova_hir::item_tree::Item::Annotation(id) => nova_hir::ids::ItemId::Annotation(id),
    };

    for member in item_members(tree, id) {
        match *member {
            nova_hir::item_tree::Member::Method(m) => {
                if tree.method(m).body.is_some() {
                    out.push(DefWithBodyId::Method(m));
                }
            }
            nova_hir::item_tree::Member::Constructor(c) => out.push(DefWithBodyId::Constructor(c)),
            nova_hir::item_tree::Member::Initializer(i) => out.push(DefWithBodyId::Initializer(i)),
            nova_hir::item_tree::Member::Type(child) => {
                collect_body_owners_in_item(tree, child, out)
            }
            nova_hir::item_tree::Member::Field(_) => {}
        }
    }
}

fn expr_qualified_name_from_field_access(
    body: &HirBody,
    receiver: HirExprId,
    name: &str,
) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    segments.push(name);

    let mut current = receiver;
    loop {
        match &body.exprs[current] {
            HirExpr::Name { name, .. } => {
                segments.push(name.as_str());
                break;
            }
            HirExpr::FieldAccess { receiver, name, .. } => {
                segments.push(name.as_str());
                current = *receiver;
            }
            _ => return None,
        }
    }

    segments.reverse();
    Some(segments.join("."))
}

fn def_file(def: DefWithBodyId) -> FileId {
    match def {
        DefWithBodyId::Method(m) => m.file,
        DefWithBodyId::Constructor(c) => c.file,
        DefWithBodyId::Initializer(i) => i.file,
    }
}

fn seed_lambda_params_from_target<'a, 'idx>(
    checker: &mut BodyChecker<'a, 'idx>,
    loader: &mut ExternalTypeLoader<'_>,
    lambda_expr: HirExprId,
    target: &Type,
) {
    let HirExpr::Lambda { params, .. } = &checker.body.exprs[lambda_expr] else {
        return;
    };

    checker.ensure_type_loaded(loader, target);
    let env_ro: &dyn TypeEnv = &*loader.store;
    if let Some(sig) = nova_types::infer_lambda_sam_signature(env_ro, target) {
        if sig.params.len() != params.len() {
            checker.diagnostics.push(Diagnostic::error(
                "lambda-arity-mismatch",
                format!(
                    "lambda arity mismatch: expected {}, found {}",
                    sig.params.len(),
                    params.len()
                ),
                Some(checker.body.exprs[lambda_expr].range()),
            ));
        }

        for (param, ty) in params.iter().zip(sig.params.into_iter()) {
            checker.local_types[param.local.idx()] = ty;
            checker.local_ty_states[param.local.idx()] = LocalTyState::Computed;
        }
    }
}

fn find_best_expr_in_stmt(
    body: &HirBody,
    stmt: nova_hir::hir::StmtId,
    offset: usize,
    owner: DefWithBodyId,
    best: &mut Option<(DefWithBodyId, HirExprId, usize)>,
) {
    let stmt_range = match &body.stmts[stmt] {
        HirStmt::Block { range, .. }
        | HirStmt::Let { range, .. }
        | HirStmt::Expr { range, .. }
        | HirStmt::Yield { range, .. }
        | HirStmt::Assert { range, .. }
        | HirStmt::Return { range, .. }
        | HirStmt::If { range, .. }
        | HirStmt::While { range, .. }
        | HirStmt::For { range, .. }
        | HirStmt::ForEach { range, .. }
        | HirStmt::Synchronized { range, .. }
        | HirStmt::Switch { range, .. }
        | HirStmt::Try { range, .. }
        | HirStmt::Throw { range, .. }
        | HirStmt::Break { range, .. }
        | HirStmt::Continue { range, .. }
        | HirStmt::Empty { range, .. } => *range,
    };

    // Fast path: if the cursor isn't inside this statement's range, none of its descendants can
    // contain the target offset. Keep this best-effort by only pruning when we have a non-empty
    // range (parse recovery can yield empty/degenerate spans).
    if !stmt_range.is_empty() && !(stmt_range.start <= offset && offset < stmt_range.end) {
        return;
    }

    match &body.stmts[stmt] {
        HirStmt::Block { statements, .. } => {
            for &stmt in statements {
                find_best_expr_in_stmt(body, stmt, offset, owner, best);
            }
        }
        HirStmt::Let { initializer, .. } => {
            if let Some(expr) = initializer {
                find_best_expr_in_expr(body, *expr, offset, owner, best);
            }
        }
        HirStmt::Expr { expr, .. } => find_best_expr_in_expr(body, *expr, offset, owner, best),
        HirStmt::Assert {
            condition, message, ..
        } => {
            find_best_expr_in_expr(body, *condition, offset, owner, best);
            if let Some(expr) = message {
                find_best_expr_in_expr(body, *expr, offset, owner, best);
            }
        }
        HirStmt::Yield { expr, .. } => {
            if let Some(expr) = expr {
                find_best_expr_in_expr(body, *expr, offset, owner, best);
            }
        }
        HirStmt::Return { expr, .. } => {
            if let Some(expr) = expr {
                find_best_expr_in_expr(body, *expr, offset, owner, best);
            }
        }
        HirStmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            find_best_expr_in_expr(body, *condition, offset, owner, best);
            find_best_expr_in_stmt(body, *then_branch, offset, owner, best);
            if let Some(else_branch) = else_branch {
                find_best_expr_in_stmt(body, *else_branch, offset, owner, best);
            }
        }
        HirStmt::While {
            condition,
            body: loop_body,
            ..
        } => {
            find_best_expr_in_expr(body, *condition, offset, owner, best);
            find_best_expr_in_stmt(body, *loop_body, offset, owner, best);
        }
        HirStmt::For {
            init,
            condition,
            update,
            body: for_body,
            ..
        } => {
            for stmt in init {
                find_best_expr_in_stmt(body, *stmt, offset, owner, best);
            }
            if let Some(condition) = condition {
                find_best_expr_in_expr(body, *condition, offset, owner, best);
            }
            for expr in update {
                find_best_expr_in_expr(body, *expr, offset, owner, best);
            }
            find_best_expr_in_stmt(body, *for_body, offset, owner, best);
        }
        HirStmt::ForEach {
            iterable,
            body: foreach_body,
            ..
        } => {
            find_best_expr_in_expr(body, *iterable, offset, owner, best);
            find_best_expr_in_stmt(body, *foreach_body, offset, owner, best);
        }
        HirStmt::Synchronized {
            expr,
            body: sync_body,
            ..
        } => {
            find_best_expr_in_expr(body, *expr, offset, owner, best);
            find_best_expr_in_stmt(body, *sync_body, offset, owner, best);
        }
        HirStmt::Switch {
            selector,
            body: switch_body,
            ..
        } => {
            find_best_expr_in_expr(body, *selector, offset, owner, best);
            find_best_expr_in_stmt(body, *switch_body, offset, owner, best);
        }
        HirStmt::Try {
            body: try_body,
            catches,
            finally,
            ..
        } => {
            find_best_expr_in_stmt(body, *try_body, offset, owner, best);
            for catch in catches {
                find_best_expr_in_stmt(body, catch.body, offset, owner, best);
            }
            if let Some(finally) = finally {
                find_best_expr_in_stmt(body, *finally, offset, owner, best);
            }
        }
        HirStmt::Throw { expr, .. } => find_best_expr_in_expr(body, *expr, offset, owner, best),
        HirStmt::Break { .. } | HirStmt::Continue { .. } => {}
        HirStmt::Empty { .. } => {}
    }
}

fn contains_expr_in_stmt(body: &HirBody, stmt: nova_hir::hir::StmtId, target: HirExprId) -> bool {
    match &body.stmts[stmt] {
        HirStmt::Block { statements, .. } => statements
            .iter()
            .any(|stmt| contains_expr_in_stmt(body, *stmt, target)),
        HirStmt::Let { initializer, .. } => initializer
            .as_ref()
            .is_some_and(|expr| contains_expr_in_expr(body, *expr, target)),
        HirStmt::Expr { expr, .. } => contains_expr_in_expr(body, *expr, target),
        HirStmt::Yield { expr, .. } => expr
            .as_ref()
            .is_some_and(|expr| contains_expr_in_expr(body, *expr, target)),
        HirStmt::Return { expr, .. } => expr
            .as_ref()
            .is_some_and(|expr| contains_expr_in_expr(body, *expr, target)),
        HirStmt::Assert {
            condition, message, ..
        } => {
            contains_expr_in_expr(body, *condition, target)
                || message
                    .as_ref()
                    .is_some_and(|expr| contains_expr_in_expr(body, *expr, target))
        }
        HirStmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            contains_expr_in_expr(body, *condition, target)
                || contains_expr_in_stmt(body, *then_branch, target)
                || else_branch
                    .as_ref()
                    .is_some_and(|branch| contains_expr_in_stmt(body, *branch, target))
        }
        HirStmt::While {
            condition, body: b, ..
        } => {
            contains_expr_in_expr(body, *condition, target)
                || contains_expr_in_stmt(body, *b, target)
        }
        HirStmt::For {
            init,
            condition,
            update,
            body: b,
            ..
        } => {
            init.iter()
                .any(|stmt| contains_expr_in_stmt(body, *stmt, target))
                || condition
                    .as_ref()
                    .is_some_and(|expr| contains_expr_in_expr(body, *expr, target))
                || update
                    .iter()
                    .any(|expr| contains_expr_in_expr(body, *expr, target))
                || contains_expr_in_stmt(body, *b, target)
        }
        HirStmt::ForEach {
            iterable, body: b, ..
        } => {
            contains_expr_in_expr(body, *iterable, target)
                || contains_expr_in_stmt(body, *b, target)
        }
        HirStmt::Synchronized { expr, body: b, .. } => {
            contains_expr_in_expr(body, *expr, target) || contains_expr_in_stmt(body, *b, target)
        }
        HirStmt::Switch {
            selector, body: b, ..
        } => {
            contains_expr_in_expr(body, *selector, target)
                || contains_expr_in_stmt(body, *b, target)
        }
        HirStmt::Try {
            body: b,
            catches,
            finally,
            ..
        } => {
            contains_expr_in_stmt(body, *b, target)
                || catches
                    .iter()
                    .any(|catch| contains_expr_in_stmt(body, catch.body, target))
                || finally
                    .as_ref()
                    .is_some_and(|finally| contains_expr_in_stmt(body, *finally, target))
        }
        HirStmt::Throw { expr, .. } => contains_expr_in_expr(body, *expr, target),
        HirStmt::Break { .. } | HirStmt::Continue { .. } | HirStmt::Empty { .. } => false,
    }
}

fn contains_expr_in_expr(body: &HirBody, expr: HirExprId, target: HirExprId) -> bool {
    if expr == target {
        return true;
    }

    match &body.exprs[expr] {
        HirExpr::Cast { expr: inner, .. } => contains_expr_in_expr(body, *inner, target),
        HirExpr::FieldAccess { receiver, .. } => contains_expr_in_expr(body, *receiver, target),
        HirExpr::ArrayAccess { array, index, .. } => {
            contains_expr_in_expr(body, *array, target)
                || contains_expr_in_expr(body, *index, target)
        }
        HirExpr::MethodReference { receiver, .. } => contains_expr_in_expr(body, *receiver, target),
        HirExpr::ConstructorReference { receiver, .. } => {
            contains_expr_in_expr(body, *receiver, target)
        }
        HirExpr::ClassLiteral { ty, .. } => contains_expr_in_expr(body, *ty, target),
        HirExpr::Call { callee, args, .. } => {
            contains_expr_in_expr(body, *callee, target)
                || args
                    .iter()
                    .any(|expr| contains_expr_in_expr(body, *expr, target))
        }
        HirExpr::New { args, .. } => args
            .iter()
            .any(|expr| contains_expr_in_expr(body, *expr, target)),
        HirExpr::ArrayInitializer { items, .. } => items
            .iter()
            .any(|expr| contains_expr_in_expr(body, *expr, target)),
        HirExpr::ArrayCreation {
            dim_exprs,
            initializer,
            ..
        } => {
            dim_exprs
                .iter()
                .any(|expr| contains_expr_in_expr(body, *expr, target))
                || initializer
                    .as_ref()
                    .is_some_and(|init| contains_expr_in_expr(body, *init, target))
        }
        HirExpr::Unary { expr, .. } => contains_expr_in_expr(body, *expr, target),
        HirExpr::Instanceof { expr, .. } => contains_expr_in_expr(body, *expr, target),
        HirExpr::Binary { lhs, rhs, .. } => {
            contains_expr_in_expr(body, *lhs, target) || contains_expr_in_expr(body, *rhs, target)
        }
        HirExpr::Assign { lhs, rhs, .. } => {
            contains_expr_in_expr(body, *lhs, target) || contains_expr_in_expr(body, *rhs, target)
        }
        HirExpr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } => {
            contains_expr_in_expr(body, *condition, target)
                || contains_expr_in_expr(body, *then_expr, target)
                || contains_expr_in_expr(body, *else_expr, target)
        }
        HirExpr::Switch { selector, arms, .. } => {
            contains_expr_in_expr(body, *selector, target)
                || arms.iter().any(|arm| {
                    arm.labels.iter().any(|label| match label {
                        SwitchLabel::Case { values, .. } => values
                            .iter()
                            .any(|value| contains_expr_in_expr(body, *value, target)),
                        SwitchLabel::Default { .. } => false,
                    }) || match &arm.body {
                        SwitchArmBody::Expr(expr) => contains_expr_in_expr(body, *expr, target),
                        SwitchArmBody::Block(stmt) | SwitchArmBody::Stmt(stmt) => {
                            contains_expr_in_stmt(body, *stmt, target)
                        }
                    }
                })
        }
        HirExpr::Lambda { body: b, .. } => match b {
            LambdaBody::Expr(expr) => contains_expr_in_expr(body, *expr, target),
            LambdaBody::Block(stmt) => contains_expr_in_stmt(body, *stmt, target),
        },
        HirExpr::Invalid { children, .. } => children
            .iter()
            .any(|expr| contains_expr_in_expr(body, *expr, target)),
        HirExpr::Name { .. }
        | HirExpr::Literal { .. }
        | HirExpr::Null { .. }
        | HirExpr::This { .. }
        | HirExpr::Super { .. }
        | HirExpr::Missing { .. } => false,
    }
}

fn find_enclosing_target_typed_expr_in_stmt(
    body: &HirBody,
    stmt: nova_hir::hir::StmtId,
    target: HirExprId,
    best: &mut Option<(HirExprId, usize)>,
) {
    let target_range = body.exprs[target].range();
    find_enclosing_target_typed_expr_in_stmt_inner(body, stmt, target, target_range, best);
}

fn find_enclosing_target_typed_expr_in_stmt_inner(
    body: &HirBody,
    stmt: nova_hir::hir::StmtId,
    target: HirExprId,
    target_range: Span,
    best: &mut Option<(HirExprId, usize)>,
) {
    let stmt_range = match &body.stmts[stmt] {
        HirStmt::Block { range, .. }
        | HirStmt::Let { range, .. }
        | HirStmt::Expr { range, .. }
        | HirStmt::Yield { range, .. }
        | HirStmt::Assert { range, .. }
        | HirStmt::Yield { range, .. }
        | HirStmt::Return { range, .. }
        | HirStmt::If { range, .. }
        | HirStmt::While { range, .. }
        | HirStmt::For { range, .. }
        | HirStmt::ForEach { range, .. }
        | HirStmt::Synchronized { range, .. }
        | HirStmt::Switch { range, .. }
        | HirStmt::Try { range, .. }
        | HirStmt::Throw { range, .. }
        | HirStmt::Break { range, .. }
        | HirStmt::Continue { range, .. }
        | HirStmt::Empty { range, .. } => *range,
    };
    // Best-effort pruning: only skip when we have a non-empty range and the target start is
    // clearly outside it. Parse recovery can produce degenerate spans that don't strictly contain
    // children.
    if !stmt_range.is_empty()
        && !(stmt_range.start <= target_range.start && target_range.start < stmt_range.end)
    {
        return;
    }

    match &body.stmts[stmt] {
        HirStmt::Block { statements, .. } => {
            for stmt in statements {
                find_enclosing_target_typed_expr_in_stmt_inner(
                    body,
                    *stmt,
                    target,
                    target_range,
                    best,
                );
            }
        }
        HirStmt::Let { initializer, .. } => {
            if let Some(expr) = initializer {
                find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
            }
        }
        HirStmt::Expr { expr, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
        }
        HirStmt::Assert {
            condition, message, ..
        } => {
            find_enclosing_target_typed_expr_in_expr(body, *condition, target, target_range, best);
            if let Some(expr) = message {
                find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
            }
        }
        HirStmt::Yield { expr, .. } => {
            if let Some(expr) = expr {
                find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
            }
        }
        HirStmt::Return { expr, .. } => {
            if let Some(expr) = expr {
                find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
            }
        }
        HirStmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            find_enclosing_target_typed_expr_in_expr(body, *condition, target, target_range, best);
            find_enclosing_target_typed_expr_in_stmt_inner(
                body,
                *then_branch,
                target,
                target_range,
                best,
            );
            if let Some(branch) = else_branch {
                find_enclosing_target_typed_expr_in_stmt_inner(
                    body,
                    *branch,
                    target,
                    target_range,
                    best,
                );
            }
        }
        HirStmt::While {
            condition, body: b, ..
        } => {
            find_enclosing_target_typed_expr_in_expr(body, *condition, target, target_range, best);
            find_enclosing_target_typed_expr_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::For {
            init,
            condition,
            update,
            body: b,
            ..
        } => {
            for stmt in init {
                find_enclosing_target_typed_expr_in_stmt_inner(
                    body,
                    *stmt,
                    target,
                    target_range,
                    best,
                );
            }
            if let Some(expr) = condition {
                find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
            }
            for expr in update {
                find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
            }
            find_enclosing_target_typed_expr_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::ForEach {
            iterable, body: b, ..
        } => {
            find_enclosing_target_typed_expr_in_expr(body, *iterable, target, target_range, best);
            find_enclosing_target_typed_expr_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::Synchronized { expr, body: b, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
            find_enclosing_target_typed_expr_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::Switch {
            selector, body: b, ..
        } => {
            find_enclosing_target_typed_expr_in_expr(body, *selector, target, target_range, best);
            find_enclosing_target_typed_expr_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::Try {
            body: b,
            catches,
            finally,
            ..
        } => {
            find_enclosing_target_typed_expr_in_stmt_inner(body, *b, target, target_range, best);
            for catch in catches {
                find_enclosing_target_typed_expr_in_stmt_inner(
                    body,
                    catch.body,
                    target,
                    target_range,
                    best,
                );
            }
            if let Some(finally) = finally {
                find_enclosing_target_typed_expr_in_stmt_inner(
                    body,
                    *finally,
                    target,
                    target_range,
                    best,
                );
            }
        }
        HirStmt::Throw { expr, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
        }
        HirStmt::Break { .. } | HirStmt::Continue { .. } | HirStmt::Empty { .. } => {}
    }
}

fn find_enclosing_target_typed_expr_in_expr(
    body: &HirBody,
    expr: HirExprId,
    target: HirExprId,
    target_range: Span,
    best: &mut Option<(HirExprId, usize)>,
) {
    let expr_node = &body.exprs[expr];
    let range = expr_node.range();

    // Best-effort pruning: avoid pruning `Invalid` nodes (parse recovery can yield surprising
    // spans) and only skip when we have a non-empty range and the target start is clearly outside.
    let can_prune = !matches!(expr_node, HirExpr::Invalid { .. });
    if can_prune
        && !range.is_empty()
        && !(range.start <= target_range.start && target_range.start < range.end)
    {
        return;
    }

    if matches!(
        body.exprs[expr],
        HirExpr::Lambda { .. }
            | HirExpr::MethodReference { .. }
            | HirExpr::ConstructorReference { .. }
    ) && contains_expr_in_expr(body, expr, target)
    {
        let len = range.len();
        let replace = best.map(|(_, best_len)| len < best_len).unwrap_or(true);
        if replace {
            *best = Some((expr, len));
        }
    }

    match &body.exprs[expr] {
        HirExpr::Cast { expr: inner, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *inner, target, target_range, best);
        }
        HirExpr::FieldAccess { receiver, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *receiver, target, target_range, best);
        }
        HirExpr::ArrayAccess { array, index, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *array, target, target_range, best);
            find_enclosing_target_typed_expr_in_expr(body, *index, target, target_range, best);
        }
        HirExpr::MethodReference { receiver, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *receiver, target, target_range, best);
        }
        HirExpr::ConstructorReference { receiver, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *receiver, target, target_range, best);
        }
        HirExpr::ClassLiteral { ty, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *ty, target, target_range, best);
        }
        HirExpr::Call { callee, args, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *callee, target, target_range, best);
            for arg in args {
                find_enclosing_target_typed_expr_in_expr(body, *arg, target, target_range, best);
            }
        }
        HirExpr::New { args, .. } => {
            for arg in args {
                find_enclosing_target_typed_expr_in_expr(body, *arg, target, target_range, best);
            }
        }
        HirExpr::ArrayInitializer { items, .. } => {
            for item in items {
                find_enclosing_target_typed_expr_in_expr(body, *item, target, target_range, best);
            }
        }
        HirExpr::ArrayCreation {
            dim_exprs,
            initializer,
            ..
        } => {
            for dim_expr in dim_exprs {
                find_enclosing_target_typed_expr_in_expr(
                    body,
                    *dim_expr,
                    target,
                    target_range,
                    best,
                );
            }
            if let Some(init_expr) = initializer {
                find_enclosing_target_typed_expr_in_expr(
                    body,
                    *init_expr,
                    target,
                    target_range,
                    best,
                );
            }
        }
        HirExpr::ArrayInitializer { items, .. } => {
            for item in items {
                find_enclosing_target_typed_expr_in_expr(body, *item, target, target_range, best);
            }
        }
        HirExpr::Unary { expr, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
        }
        HirExpr::Instanceof { expr, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
        }
        HirExpr::Binary { lhs, rhs, .. } | HirExpr::Assign { lhs, rhs, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *lhs, target, target_range, best);
            find_enclosing_target_typed_expr_in_expr(body, *rhs, target, target_range, best);
        }
        HirExpr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } => {
            find_enclosing_target_typed_expr_in_expr(body, *condition, target, target_range, best);
            find_enclosing_target_typed_expr_in_expr(body, *then_expr, target, target_range, best);
            find_enclosing_target_typed_expr_in_expr(body, *else_expr, target, target_range, best);
        }
        HirExpr::Switch { selector, arms, .. } => {
            find_enclosing_target_typed_expr_in_expr(body, *selector, target, target_range, best);
            for arm in arms {
                for label in &arm.labels {
                    if let SwitchLabel::Case { values, .. } = label {
                        for value in values {
                            find_enclosing_target_typed_expr_in_expr(
                                body,
                                *value,
                                target,
                                target_range,
                                best,
                            );
                        }
                    }
                }
                match &arm.body {
                    SwitchArmBody::Expr(expr) => {
                        find_enclosing_target_typed_expr_in_expr(
                            body,
                            *expr,
                            target,
                            target_range,
                            best,
                        );
                    }
                    SwitchArmBody::Block(stmt) | SwitchArmBody::Stmt(stmt) => {
                        find_enclosing_target_typed_expr_in_stmt_inner(
                            body,
                            *stmt,
                            target,
                            target_range,
                            best,
                        );
                    }
                }
            }
        }
        HirExpr::Lambda { body: b, .. } => match b {
            LambdaBody::Expr(expr) => {
                find_enclosing_target_typed_expr_in_expr(body, *expr, target, target_range, best);
            }
            LambdaBody::Block(stmt) => {
                find_enclosing_target_typed_expr_in_stmt_inner(
                    body,
                    *stmt,
                    target,
                    target_range,
                    best,
                );
            }
        },
        HirExpr::Invalid { children, .. } => {
            for child in children {
                find_enclosing_target_typed_expr_in_expr(body, *child, target, target_range, best);
            }
        }
        HirExpr::Name { .. }
        | HirExpr::Literal { .. }
        | HirExpr::Null { .. }
        | HirExpr::This { .. }
        | HirExpr::Super { .. }
        | HirExpr::Missing { .. } => {}
    }
}

fn find_enclosing_call_with_arg_in_stmt(
    body: &HirBody,
    stmt: nova_hir::hir::StmtId,
    target: HirExprId,
    best: &mut Option<(HirExprId, usize)>,
) {
    let target_range = body.exprs[target].range();
    find_enclosing_call_with_arg_in_stmt_inner(body, stmt, target, target_range, best);
}

fn find_enclosing_call_with_arg_in_stmt_inner(
    body: &HirBody,
    stmt: nova_hir::hir::StmtId,
    target: HirExprId,
    target_range: Span,
    best: &mut Option<(HirExprId, usize)>,
) {
    let stmt_range = match &body.stmts[stmt] {
        HirStmt::Block { range, .. }
        | HirStmt::Let { range, .. }
        | HirStmt::Expr { range, .. }
        | HirStmt::Yield { range, .. }
        | HirStmt::Assert { range, .. }
        | HirStmt::Return { range, .. }
        | HirStmt::If { range, .. }
        | HirStmt::While { range, .. }
        | HirStmt::For { range, .. }
        | HirStmt::ForEach { range, .. }
        | HirStmt::Synchronized { range, .. }
        | HirStmt::Switch { range, .. }
        | HirStmt::Try { range, .. }
        | HirStmt::Throw { range, .. }
        | HirStmt::Break { range, .. }
        | HirStmt::Continue { range, .. }
        | HirStmt::Empty { range, .. } => *range,
    };
    // Best-effort pruning: only skip when we have a non-empty range and the target start is
    // clearly outside it. Parse recovery can produce degenerate spans that don't strictly contain
    // children.
    if !stmt_range.is_empty()
        && !(stmt_range.start <= target_range.start && target_range.start < stmt_range.end)
    {
        return;
    }

    match &body.stmts[stmt] {
        HirStmt::Block { statements, .. } => {
            for stmt in statements {
                find_enclosing_call_with_arg_in_stmt_inner(body, *stmt, target, target_range, best);
            }
        }
        HirStmt::Let { initializer, .. } => {
            if let Some(expr) = initializer {
                find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
            }
        }
        HirStmt::Expr { expr, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
        }
        HirStmt::Assert {
            condition, message, ..
        } => {
            find_enclosing_call_with_arg_in_expr(body, *condition, target, target_range, best);
            if let Some(expr) = message {
                find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
            }
        }
        HirStmt::Return { expr, .. } => {
            if let Some(expr) = expr {
                find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
            }
        }
        HirStmt::Yield { expr, .. } => {
            if let Some(expr) = expr {
                find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
            }
        }
        HirStmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            find_enclosing_call_with_arg_in_expr(body, *condition, target, target_range, best);
            find_enclosing_call_with_arg_in_stmt_inner(
                body,
                *then_branch,
                target,
                target_range,
                best,
            );
            if let Some(branch) = else_branch {
                find_enclosing_call_with_arg_in_stmt_inner(
                    body,
                    *branch,
                    target,
                    target_range,
                    best,
                );
            }
        }
        HirStmt::While {
            condition, body: b, ..
        } => {
            find_enclosing_call_with_arg_in_expr(body, *condition, target, target_range, best);
            find_enclosing_call_with_arg_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::For {
            init,
            condition,
            update,
            body: b,
            ..
        } => {
            for stmt in init {
                find_enclosing_call_with_arg_in_stmt_inner(body, *stmt, target, target_range, best);
            }
            if let Some(expr) = condition {
                find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
            }
            for expr in update {
                find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
            }
            find_enclosing_call_with_arg_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::ForEach {
            iterable, body: b, ..
        } => {
            find_enclosing_call_with_arg_in_expr(body, *iterable, target, target_range, best);
            find_enclosing_call_with_arg_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::Synchronized { expr, body: b, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
            find_enclosing_call_with_arg_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::Switch {
            selector, body: b, ..
        } => {
            find_enclosing_call_with_arg_in_expr(body, *selector, target, target_range, best);
            find_enclosing_call_with_arg_in_stmt_inner(body, *b, target, target_range, best);
        }
        HirStmt::Try {
            body: b,
            catches,
            finally,
            ..
        } => {
            find_enclosing_call_with_arg_in_stmt_inner(body, *b, target, target_range, best);
            for catch in catches {
                find_enclosing_call_with_arg_in_stmt_inner(
                    body,
                    catch.body,
                    target,
                    target_range,
                    best,
                );
            }
            if let Some(finally) = finally {
                find_enclosing_call_with_arg_in_stmt_inner(
                    body,
                    *finally,
                    target,
                    target_range,
                    best,
                );
            }
        }
        HirStmt::Throw { expr, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
        }
        HirStmt::Break { .. } | HirStmt::Continue { .. } | HirStmt::Empty { .. } => {}
    }
}

fn find_enclosing_call_with_arg_in_expr(
    body: &HirBody,
    expr: HirExprId,
    target: HirExprId,
    target_range: Span,
    best: &mut Option<(HirExprId, usize)>,
) {
    let expr_node = &body.exprs[expr];
    let range = expr_node.range();

    // Best-effort pruning: avoid pruning `Invalid` nodes (parse recovery can yield surprising
    // spans) and only skip when we have a non-empty range and the target start is clearly outside.
    let can_prune = !matches!(expr_node, HirExpr::Invalid { .. });
    if can_prune
        && !range.is_empty()
        && !(range.start <= target_range.start && target_range.start < range.end)
    {
        return;
    }

    match &body.exprs[expr] {
        HirExpr::Cast { expr: inner, .. } => {
            if contains_expr_in_expr(body, *inner, target) {
                let len = range.len();
                let replace = best.map(|(_, best_len)| len < best_len).unwrap_or(true);
                if replace {
                    *best = Some((expr, len));
                }
            }
            find_enclosing_call_with_arg_in_expr(body, *inner, target, target_range, best);
        }
        HirExpr::FieldAccess { receiver, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *receiver, target, target_range, best);
        }
        HirExpr::ArrayAccess { array, index, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *array, target, target_range, best);
            find_enclosing_call_with_arg_in_expr(body, *index, target, target_range, best);
        }
        HirExpr::MethodReference { receiver, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *receiver, target, target_range, best);
        }
        HirExpr::ConstructorReference { receiver, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *receiver, target, target_range, best);
        }
        HirExpr::ClassLiteral { ty, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *ty, target, target_range, best);
        }
        HirExpr::Call { callee, args, .. } => {
            if args
                .iter()
                .any(|arg| contains_expr_in_expr(body, *arg, target))
            {
                let len = range.len();
                let replace = best.map(|(_, best_len)| len < best_len).unwrap_or(true);
                if replace {
                    *best = Some((expr, len));
                }
            }

            find_enclosing_call_with_arg_in_expr(body, *callee, target, target_range, best);
            for arg in args {
                find_enclosing_call_with_arg_in_expr(body, *arg, target, target_range, best);
            }
        }
        HirExpr::New { args, .. } => {
            if args
                .iter()
                .any(|arg| contains_expr_in_expr(body, *arg, target))
            {
                let len = range.len();
                let replace = best.map(|(_, best_len)| len < best_len).unwrap_or(true);
                if replace {
                    *best = Some((expr, len));
                }
            }
            for arg in args {
                find_enclosing_call_with_arg_in_expr(body, *arg, target, target_range, best);
            }
        }
        HirExpr::ArrayCreation {
            dim_exprs,
            initializer,
            ..
        } => {
            for dim_expr in dim_exprs {
                find_enclosing_call_with_arg_in_expr(body, *dim_expr, target, target_range, best);
            }
            if let Some(init) = initializer {
                find_enclosing_call_with_arg_in_expr(body, *init, target, target_range, best);
            }
        }
        HirExpr::ArrayInitializer { items, .. } => {
            for item in items {
                find_enclosing_call_with_arg_in_expr(body, *item, target, target_range, best);
            }
        }
        HirExpr::Unary { expr, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
        }
        HirExpr::Binary { lhs, rhs, .. } | HirExpr::Assign { lhs, rhs, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *lhs, target, target_range, best);
            find_enclosing_call_with_arg_in_expr(body, *rhs, target, target_range, best);
        }
        HirExpr::Instanceof { expr, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
        }
        HirExpr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } => {
            find_enclosing_call_with_arg_in_expr(body, *condition, target, target_range, best);
            find_enclosing_call_with_arg_in_expr(body, *then_expr, target, target_range, best);
            find_enclosing_call_with_arg_in_expr(body, *else_expr, target, target_range, best);
        }
        HirExpr::Switch { selector, arms, .. } => {
            find_enclosing_call_with_arg_in_expr(body, *selector, target, target_range, best);
            for arm in arms {
                for label in &arm.labels {
                    if let SwitchLabel::Case { values, .. } = label {
                        for value in values {
                            find_enclosing_call_with_arg_in_expr(
                                body,
                                *value,
                                target,
                                target_range,
                                best,
                            );
                        }
                    }
                }

                match &arm.body {
                    SwitchArmBody::Expr(expr) => {
                        find_enclosing_call_with_arg_in_expr(
                            body,
                            *expr,
                            target,
                            target_range,
                            best,
                        );
                    }
                    SwitchArmBody::Block(stmt) | SwitchArmBody::Stmt(stmt) => {
                        find_enclosing_call_with_arg_in_stmt_inner(
                            body,
                            *stmt,
                            target,
                            target_range,
                            best,
                        );
                    }
                }
            }
        }
        HirExpr::Lambda { body: b, .. } => match b {
            LambdaBody::Expr(expr) => {
                find_enclosing_call_with_arg_in_expr(body, *expr, target, target_range, best);
            }
            LambdaBody::Block(stmt) => {
                find_enclosing_call_with_arg_in_stmt_inner(body, *stmt, target, target_range, best);
            }
        },
        HirExpr::Invalid { children, .. } => {
            for child in children {
                find_enclosing_call_with_arg_in_expr(body, *child, target, target_range, best);
            }
        }
        HirExpr::Name { .. }
        | HirExpr::Literal { .. }
        | HirExpr::Null { .. }
        | HirExpr::This { .. }
        | HirExpr::Super { .. }
        | HirExpr::Missing { .. } => {}
    }
}

fn find_best_expr_in_expr(
    body: &HirBody,
    expr: HirExprId,
    offset: usize,
    owner: DefWithBodyId,
    best: &mut Option<(DefWithBodyId, HirExprId, usize)>,
) {
    let expr_node = &body.exprs[expr];
    let range = expr_node.range();

    // Best-effort pruning: in well-formed HIR, child expression ranges are nested inside their
    // parents. However, parse recovery can produce `Invalid` nodes with surprising spans; avoid
    // pruning those so we still find a best match in broken code.
    let can_prune = !matches!(expr_node, HirExpr::Invalid { .. });
    if can_prune && !range.is_empty() && !(range.start <= offset && offset < range.end) {
        return;
    }
    // `Span` uses end-exclusive semantics (mirrors `text_size::TextRange`).
    if range.start <= offset && offset < range.end {
        let len = range.len();
        let replace = best.map(|(_, _, best_len)| len < best_len).unwrap_or(true);
        if replace {
            *best = Some((owner, expr, len));
        }
    }

    match &body.exprs[expr] {
        HirExpr::Cast { expr: inner, .. } => {
            find_best_expr_in_expr(body, *inner, offset, owner, best);
        }
        HirExpr::FieldAccess { receiver, .. } => {
            find_best_expr_in_expr(body, *receiver, offset, owner, best);
        }
        HirExpr::ArrayAccess { array, index, .. } => {
            find_best_expr_in_expr(body, *array, offset, owner, best);
            find_best_expr_in_expr(body, *index, offset, owner, best);
        }
        HirExpr::MethodReference { receiver, .. } => {
            find_best_expr_in_expr(body, *receiver, offset, owner, best);
        }
        HirExpr::ConstructorReference { receiver, .. } => {
            find_best_expr_in_expr(body, *receiver, offset, owner, best);
        }
        HirExpr::ClassLiteral { ty, .. } => {
            find_best_expr_in_expr(body, *ty, offset, owner, best);
        }
        HirExpr::Invalid { children, .. } => {
            for child in children {
                find_best_expr_in_expr(body, *child, offset, owner, best);
            }
        }
        HirExpr::Call { callee, args, .. } => {
            find_best_expr_in_expr(body, *callee, offset, owner, best);
            for arg in args {
                find_best_expr_in_expr(body, *arg, offset, owner, best);
            }
        }
        HirExpr::New { args, .. } => {
            for arg in args {
                find_best_expr_in_expr(body, *arg, offset, owner, best);
            }
        }
        HirExpr::ArrayCreation {
            dim_exprs,
            initializer,
            ..
        } => {
            for dim_expr in dim_exprs {
                find_best_expr_in_expr(body, *dim_expr, offset, owner, best);
            }
            if let Some(init) = initializer {
                find_best_expr_in_expr(body, *init, offset, owner, best);
            }
        }
        HirExpr::ArrayInitializer { items, .. } => {
            for item in items {
                find_best_expr_in_expr(body, *item, offset, owner, best);
            }
        }
        HirExpr::Unary { expr, .. } => find_best_expr_in_expr(body, *expr, offset, owner, best),
        HirExpr::Binary { lhs, rhs, .. } => {
            find_best_expr_in_expr(body, *lhs, offset, owner, best);
            find_best_expr_in_expr(body, *rhs, offset, owner, best);
        }
        HirExpr::Instanceof { expr, .. } => {
            find_best_expr_in_expr(body, *expr, offset, owner, best);
        }
        HirExpr::Assign { lhs, rhs, .. } => {
            find_best_expr_in_expr(body, *lhs, offset, owner, best);
            find_best_expr_in_expr(body, *rhs, offset, owner, best);
        }
        HirExpr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } => {
            find_best_expr_in_expr(body, *condition, offset, owner, best);
            find_best_expr_in_expr(body, *then_expr, offset, owner, best);
            find_best_expr_in_expr(body, *else_expr, offset, owner, best);
        }
        HirExpr::Switch { selector, arms, .. } => {
            find_best_expr_in_expr(body, *selector, offset, owner, best);
            for arm in arms {
                for label in &arm.labels {
                    if let SwitchLabel::Case { values, .. } = label {
                        for value in values {
                            find_best_expr_in_expr(body, *value, offset, owner, best);
                        }
                    }
                }

                match &arm.body {
                    SwitchArmBody::Expr(expr) => {
                        find_best_expr_in_expr(body, *expr, offset, owner, best)
                    }
                    SwitchArmBody::Block(stmt) | SwitchArmBody::Stmt(stmt) => {
                        find_best_expr_in_stmt(body, *stmt, offset, owner, best)
                    }
                }
            }
        }
        HirExpr::Lambda {
            body: lambda_body, ..
        } => match lambda_body {
            LambdaBody::Expr(expr) => find_best_expr_in_expr(body, *expr, offset, owner, best),
            LambdaBody::Block(stmt) => find_best_expr_in_stmt(body, *stmt, offset, owner, best),
        },
        HirExpr::Name { .. }
        | HirExpr::Literal { .. }
        | HirExpr::Null { .. }
        | HirExpr::This { .. }
        | HirExpr::Super { .. }
        | HirExpr::Missing { .. } => {}
    }
}

fn preload_type_names<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    loader: &mut ExternalTypeLoader<'_>,
    text: &str,
) {
    for_each_resolved_type_name(resolver, scopes, scope_id, text, |name| {
        loader.store.intern_class_id(name);
    });
}

fn collect_resolved_type_names<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    text: &str,
    out: &mut Vec<String>,
) {
    for_each_resolved_type_name(resolver, scopes, scope_id, text, |name| {
        out.push(name.to_string());
    });
}
fn for_each_resolved_type_name<'idx>(
    resolver: &nova_resolve::Resolver<'idx>,
    scopes: &nova_resolve::ScopeGraph,
    scope_id: nova_resolve::ScopeId,
    text: &str,
    mut f: impl FnMut(&str),
) {
    let mut i = 0usize;
    let bytes = text.as_bytes();

    while i < bytes.len() {
        let b = bytes[i];
        if !is_ident_start(b) {
            i += 1;
            continue;
        }

        let start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i]) {
            i += 1;
        }

        let mut end = i;
        while end < bytes.len() && bytes[end] == b'.' {
            let seg_start = end + 1;
            if seg_start >= bytes.len() || !is_ident_start(bytes[seg_start]) {
                break;
            }
            end = seg_start + 1;
            while end < bytes.len() && is_ident_continue(bytes[end]) {
                end += 1;
            }
        }

        let candidate = &text[start..end];
        i = end;

        if is_primitive_or_keyword(candidate) {
            continue;
        }

        let q = QualifiedName::from_dotted(candidate);
        let Some(resolved) = resolver.resolve_qualified_type_in_scope(scopes, scope_id, &q) else {
            continue;
        };
        f(resolved.as_str());
    }
}

fn is_ident_start(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'$')
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || matches!(b, b'0'..=b'9')
}

fn is_primitive_or_keyword(word: &str) -> bool {
    matches!(
        word,
        "boolean"
            | "byte"
            | "short"
            | "int"
            | "long"
            | "char"
            | "float"
            | "double"
            | "void"
            | "extends"
            | "super"
            | "var"
    )
}
fn type_binary_name(env: &TypeStore, ty: &Type) -> Option<String> {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => env.class(*def).map(|c| c.name.clone()),
        Type::Named(name) => Some(name.clone()),
        _ => None,
    }
}

fn format_method_candidate_signature(
    env: &dyn TypeEnv,
    cand: &nova_types::MethodCandidate,
) -> String {
    let mut out = String::new();
    out.push_str(&format_type(env, &cand.return_type));
    out.push(' ');
    out.push_str(&cand.name);
    out.push('(');
    for (idx, param) in cand.params.iter().enumerate() {
        if idx != 0 {
            out.push_str(", ");
        }

        if cand.is_varargs && idx == cand.params.len().saturating_sub(1) {
            match param {
                Type::Array(elem) => out.push_str(&format_type(env, elem)),
                other => out.push_str(&format_type(env, other)),
            }
            out.push_str("...");
        } else {
            out.push_str(&format_type(env, param));
        }
    }
    out.push(')');
    out
}

fn format_constructor_candidate_signature(
    env: &dyn TypeEnv,
    ctor_name: &str,
    cand: &nova_types::MethodCandidate,
) -> String {
    let mut out = String::new();
    out.push_str(ctor_name);
    out.push('(');
    for (idx, param) in cand.params.iter().enumerate() {
        if idx != 0 {
            out.push_str(", ");
        }

        if cand.is_varargs && idx == cand.params.len().saturating_sub(1) {
            match param {
                Type::Array(elem) => out.push_str(&format_type(env, elem)),
                other => out.push_str(&format_type(env, other)),
            }
            out.push_str("...");
        } else {
            out.push_str(&format_type(env, param));
        }
    }
    out.push(')');
    out
}

fn format_method_candidate_failure_reason(
    env: &dyn TypeEnv,
    reason: &MethodCandidateFailureReason,
) -> String {
    match reason {
        MethodCandidateFailureReason::WrongCallKind { call_kind } => match call_kind {
            CallKind::Static => "method is not static".to_string(),
            CallKind::Instance => "method is static".to_string(),
        },
        MethodCandidateFailureReason::WrongArity {
            expected,
            found,
            is_varargs,
        } => {
            let suffix = if *is_varargs { " (varargs)" } else { "" };
            format!("wrong arity: expected {expected}, found {found}{suffix}")
        }
        MethodCandidateFailureReason::ExplicitTypeArgCountMismatch { expected, found } => {
            format!("wrong number of type arguments: expected {expected}, found {found}")
        }
        MethodCandidateFailureReason::TypeArgOutOfBounds {
            type_param,
            type_arg,
            upper_bound,
        } => {
            let tv = format_type(env, &Type::TypeVar(*type_param));
            let arg = format_type(env, type_arg);
            let ub = format_type(env, upper_bound);
            format!("type argument {arg} is not within bounds of {tv}: {ub}")
        }
        MethodCandidateFailureReason::ArgumentConversion {
            arg_index,
            from,
            to,
        } => {
            let from = format_type(env, from);
            let to = format_type(env, to);
            // Present as 1-based for user display.
            format!(
                "argument {}: cannot convert from {from} to {to}",
                arg_index + 1
            )
        }
    }
}

fn is_integral_primitive(p: PrimitiveType) -> bool {
    matches!(
        p,
        PrimitiveType::Byte
            | PrimitiveType::Short
            | PrimitiveType::Char
            | PrimitiveType::Int
            | PrimitiveType::Long
    )
}

fn unary_numeric_promotion(p: PrimitiveType) -> PrimitiveType {
    match p {
        PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char => PrimitiveType::Int,
        other => other,
    }
}

fn is_java_lang_string(store: &TypeStore, ty: &Type) -> bool {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => *def == store.well_known().string,
        Type::Named(name) => name == "java.lang.String",
        _ => false,
    }
}

fn is_diamond_type_ref_text(text: &str) -> bool {
    let text = text.trim();
    if !text.ends_with('>') {
        return false;
    }

    let Some(lt) = text.rfind('<') else {
        return false;
    };

    text[lt + 1..text.len().saturating_sub(1)].trim().is_empty()
}
