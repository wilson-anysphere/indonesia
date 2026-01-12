use std::collections::{HashMap, HashSet};

use nova_core::{Name, PackageId, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_hir::hir;
use nova_hir::ids::{ConstructorId, FieldId, InitializerId, ItemId, MethodId};

use crate::diagnostics::{
    ambiguous_import_diagnostic, duplicate_import_diagnostic, unresolved_import_diagnostic,
};
use crate::import_map::ImportMap;
use crate::scopes::{ScopeGraph, ScopeId, ScopeKind};
use crate::{TypeKind, WorkspaceDefMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyOwner {
    Method(MethodId),
    Constructor(ConstructorId),
    Initializer(InitializerId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalRef {
    pub owner: BodyOwner,
    pub local: hir::LocalId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamOwner {
    Method(MethodId),
    Constructor(ConstructorId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamRef {
    pub owner: ParamOwner,
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeResolution {
    Source(ItemId),
    External(TypeName),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeLookup {
    Found(TypeName),
    Ambiguous(Vec<TypeName>),
    NotFound,
}

/// Result of resolving a simple name in the *type namespace*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeNameResolution {
    Resolved(TypeResolution),
    Unresolved,
    Ambiguous(Vec<TypeResolution>),
}

impl TypeNameResolution {
    #[must_use]
    pub fn into_option(self) -> Option<TypeResolution> {
        match self {
            TypeNameResolution::Resolved(res) => Some(res),
            TypeNameResolution::Unresolved | TypeNameResolution::Ambiguous(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticLookup {
    Found(StaticMemberId),
    Ambiguous(Vec<StaticMemberId>),
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StaticMemberResolution {
    SourceField(FieldId),
    SourceMethod(MethodId),
    External(StaticMemberId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Local(LocalRef),
    Parameter(ParamRef),
    Field(FieldId),
    Methods(Vec<MethodId>),
    Constructors(Vec<ConstructorId>),
    Type(TypeResolution),
    Package(PackageId),
    StaticMember(StaticMemberResolution),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameResolution {
    Resolved(Resolution),
    Unresolved,
    Ambiguous(Vec<Resolution>),
}

impl NameResolution {
    #[must_use]
    pub fn into_option(self) -> Option<Resolution> {
        match self {
            NameResolution::Resolved(res) => Some(res),
            NameResolution::Unresolved | NameResolution::Ambiguous(_) => None,
        }
    }
}

/// A name resolver that consults the JDK index and an optional project/classpath index.
pub struct Resolver<'a> {
    jdk: &'a dyn TypeIndex,
    classpath: Option<&'a dyn TypeIndex>,
    workspace: Option<&'a WorkspaceDefMap>,
}

impl<'a> Resolver<'a> {
    #[must_use]
    pub fn new(jdk: &'a dyn TypeIndex) -> Self {
        Self {
            jdk,
            classpath: None,
            workspace: None,
        }
    }

    #[must_use]
    pub fn with_classpath(mut self, classpath: &'a dyn TypeIndex) -> Self {
        self.classpath = Some(classpath);
        self
    }

    /// Attach a workspace definition map used to prefer source types over
    /// classpath/JDK types and to surface `TypeResolution::Source` results.
    #[must_use]
    pub fn with_workspace(mut self, workspace: &'a WorkspaceDefMap) -> Self {
        self.workspace = Some(workspace);
        self
    }

    fn type_resolution_from_name(&self, ty: TypeName) -> TypeResolution {
        // Mirror the JVM restriction that application class loaders cannot define
        // `java.*` types. Even if the workspace contains a "shadowing" definition
        // of `java.lang.String`, the JDK type should win for name resolution.
        if ty.as_str().starts_with("java.") {
            return TypeResolution::External(ty);
        }
        if let Some(workspace) = self.workspace {
            if let Some(item) = workspace.item_by_type_name(&ty) {
                return TypeResolution::Source(item);
            }
        }
        TypeResolution::External(ty)
    }

    fn type_name_for_source(&self, scopes: &ScopeGraph, item: ItemId) -> Option<TypeName> {
        scopes.type_name(item).cloned().or_else(|| {
            self.workspace
                .and_then(|workspace| workspace.type_name(item).cloned())
        })
    }

    /// Convert a [`TypeResolution`] to a concrete [`TypeName`], when possible.
    ///
    /// This is lossy for unresolved source items (when the workspace map does not know the fully
    /// qualified name), but allows higher layers to report diagnostics that include candidates.
    pub fn type_name_for_resolution(
        &self,
        scopes: &ScopeGraph,
        resolution: &TypeResolution,
    ) -> Option<TypeName> {
        match resolution {
            TypeResolution::External(name) => Some(name.clone()),
            TypeResolution::Source(item) => self.type_name_for_source(scopes, *item),
        }
    }

    fn static_member_resolution_from_id(&self, member: StaticMemberId) -> StaticMemberResolution {
        let Some(workspace) = self.workspace else {
            return StaticMemberResolution::External(member);
        };

        let (owner, name) = match member.as_str().split_once("::") {
            Some((owner, name)) => (owner, name),
            None => return StaticMemberResolution::External(member),
        };

        let owner = TypeName::new(owner);
        let name = Name::from(name);
        let Some(item) = workspace.item_by_type_name(&owner) else {
            return StaticMemberResolution::External(member);
        };
        let Some(ty) = workspace.type_def(item) else {
            return StaticMemberResolution::External(member);
        };

        if let Some(field) = ty.fields.get(&name) {
            if field.is_static {
                return StaticMemberResolution::SourceField(field.id);
            }
        }
        if let Some(methods) = ty.methods.get(&name) {
            if let Some(first) = methods.iter().find(|m| m.is_static) {
                return StaticMemberResolution::SourceMethod(first.id);
            }
        }

        StaticMemberResolution::External(member)
    }

    fn workspace_allows_static_import_of_member_type(&self, ty: &TypeName) -> bool {
        let Some(workspace) = self.workspace else {
            // Best-effort: external indices do not expose member-type staticness, so assume the
            // imported member type is static. This mirrors the existing best-effort behavior for
            // external static-member lookup (fields/methods).
            return true;
        };
        let Some(item) = workspace.item_by_type_name(ty) else {
            // External type (classpath/JDK): assume static.
            return true;
        };
        let Some(def) = workspace.type_def(item) else {
            return true;
        };
        let Some(enclosing) = def.enclosing else {
            // Only member types may be statically imported (JLS 7.5.3).
            return false;
        };

        // Explicitly `static` member types are OK.
        if def.is_static {
            return true;
        }

        // Implicitly-static member types are OK.
        if matches!(
            def.kind,
            TypeKind::Interface | TypeKind::Annotation | TypeKind::Enum | TypeKind::Record
        ) {
            return true;
        }

        // Additional best-effort: any member type declared in an interface/annotation is
        // implicitly static (JLS 9.1.1.3 / 9.6.1.1).
        workspace.type_def(enclosing).is_some_and(|enclosing_def| {
            matches!(
                enclosing_def.kind,
                TypeKind::Interface | TypeKind::Annotation
            )
        })
    }

    fn resolve_static_imported_member_type(
        &self,
        owner: &QualifiedName,
        member: &Name,
    ) -> Option<TypeName> {
        // Static imports use a `TypeName` as their qualifier; require the qualifier to resolve as
        // a type before treating `owner.member` as a member type.
        let owner_ty = self.resolve_type_in_index(owner)?;

        // Prefer resolving the member as a binary nested name (`Outer$Inner`) so we don't
        // accidentally treat a subpackage type (`Outer.Inner`) as a member type.
        let binary_candidate =
            QualifiedName::from_dotted(&format!("{}${}", owner_ty.as_str(), member.as_str()));
        let ty = self
            .resolve_type_in_index_exact(&binary_candidate)
            .or_else(|| {
                // Best-effort fallback: allow indices that only resolve dotted names through the
                // nesting heuristic, but ensure the resolved type is actually nested under `owner`
                // (i.e. `owner$Member`).
                let path = if owner.segments().is_empty() {
                    member.as_str().to_string()
                } else {
                    format!("{}.{}", owner.to_dotted(), member.as_str())
                };
                let ty = self.resolve_type_in_index(&QualifiedName::from_dotted(&path))?;
                ty.as_str()
                    .starts_with(&format!("{}$", owner_ty.as_str()))
                    .then_some(ty)
            })?;

        self.workspace_allows_static_import_of_member_type(&ty)
            .then_some(ty)
    }

    fn resolve_type_in_index(&self, name: &QualifiedName) -> Option<TypeName> {
        // The runtime forbids application class loaders from defining `java.*` packages. Mirror
        // that behavior here so "fake" `java.lang.Foo` classes on the classpath don't affect
        // name resolution (and so tests can model this accurately).
        if is_java_qualified_name(name) {
            return resolve_type_with_nesting(self.jdk, name);
        }

        if let Some(classpath) = self.classpath {
            if let Some(ty) = resolve_type_with_nesting(classpath, name) {
                return Some(ty);
            }
        }

        resolve_type_with_nesting(self.jdk, name)
    }

    /// Resolve a fully-qualified type name in the configured indices *without* applying any
    /// dot-to-`$` nesting heuristics.
    ///
    /// This is useful in contexts where the caller is intentionally constructing a binary name
    /// (e.g. `Outer$Inner`) and does not want `resolve_type_with_nesting` to reinterpret dotted
    /// segments as nested separators.
    fn resolve_type_in_index_exact(&self, name: &QualifiedName) -> Option<TypeName> {
        if is_java_qualified_name(name) {
            return self.jdk.resolve_type(name);
        }
        if let Some(classpath) = self.classpath {
            if let Some(ty) = classpath.resolve_type(name) {
                return Some(ty);
            }
        }
        self.jdk.resolve_type(name)
    }

    fn resolve_type_in_package_index(
        &self,
        package: &PackageName,
        name: &Name,
    ) -> Option<TypeName> {
        if is_java_package(package) {
            return self.jdk.resolve_type_in_package(package, name);
        }

        self.classpath
            .and_then(|cp| cp.resolve_type_in_package(package, name))
            .or_else(|| self.jdk.resolve_type_in_package(package, name))
    }

    fn resolve_type_in_java_lang(&self, name: &Name) -> Option<TypeName> {
        // `java.lang.*` is implicitly imported by the language. We intentionally only consult the
        // JDK index here: user/classpath-provided types in `java.lang` are not considered part of
        // the implicit import set (and would otherwise mask ambiguity diagnostics for star imports
        // in tests).
        self.jdk
            .resolve_type_in_package(&PackageName::from_dotted("java.lang"), name)
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        if is_java_package(package) {
            return self.jdk.package_exists(package);
        }

        self.classpath.is_some_and(|cp| cp.package_exists(package))
            || self.jdk.package_exists(package)
    }

    fn resolve_static_member_in_index(
        &self,
        owner: &TypeName,
        name: &Name,
    ) -> Option<StaticMemberId> {
        self.classpath
            .and_then(|cp| cp.resolve_static_member(owner, name))
            .or_else(|| self.jdk.resolve_static_member(owner, name))
    }

    /// Resolve a simple name against static imports and report ambiguity.
    ///
    /// Follows JLS 7.5.4-style precedence:
    /// - Single static imports take precedence over static-on-demand imports.
    /// - If multiple imports introduce different members for the same name, the
    ///   result is ambiguous.
    #[must_use]
    pub fn resolve_static_imports_detailed(
        &self,
        imports: &ImportMap,
        name: &Name,
    ) -> StaticLookup {
        // 1) Explicit single static member imports (shadow star imports).
        let mut candidates = Vec::<StaticMemberId>::new();
        for import in &imports.static_single {
            if &import.imported != name {
                continue;
            }

            let Some(owner) = self.resolve_type_in_index(&import.ty) else {
                continue;
            };
            let Some(static_member) = self.resolve_static_member_in_index(&owner, &import.member)
            else {
                continue;
            };
            if !candidates.contains(&static_member) {
                candidates.push(static_member);
            }
        }

        if !candidates.is_empty() {
            return if candidates.len() == 1 {
                StaticLookup::Found(candidates.remove(0))
            } else {
                StaticLookup::Ambiguous(candidates)
            };
        }

        // 2) Static star imports.
        for import in &imports.static_star {
            let Some(owner) = self.resolve_type_in_index(&import.ty) else {
                continue;
            };
            let Some(static_member) = self.resolve_static_member_in_index(&owner, name) else {
                continue;
            };
            if !candidates.contains(&static_member) {
                candidates.push(static_member);
            }
        }

        match candidates.len() {
            0 => StaticLookup::NotFound,
            1 => StaticLookup::Found(candidates.remove(0)),
            _ => StaticLookup::Ambiguous(candidates),
        }
    }

    /// Resolve a simple type name via imports and the current package, following
    /// the JLS precedence rules (6.5 / 7.5) for the type namespace.
    ///
    /// Order:
    /// 1) Single-type imports
    /// 2) Same-package types
    /// 3) Type-import-on-demand (`.*`) imports, including implicit `java.lang.*`
    ///    (ambiguity is reported)
    ///
    /// This reports ambiguity (e.g. multiple star imports providing the same
    /// simple name) instead of picking an arbitrary match.
    #[must_use]
    pub fn resolve_import_detailed(
        &self,
        imports: &ImportMap,
        package: Option<&PackageName>,
        name: &Name,
    ) -> TypeLookup {
        match self.resolve_single_type_imports_detailed(imports, name) {
            TypeLookup::Found(ty) => return TypeLookup::Found(ty),
            TypeLookup::Ambiguous(types) => return TypeLookup::Ambiguous(types),
            TypeLookup::NotFound => {}
        }

        if let Some(pkg) = package {
            if let Some(ty) = self.resolve_type_in_package_index(pkg, name) {
                return TypeLookup::Found(ty);
            }
        }

        self.resolve_on_demand_type_imports_detailed(imports, name)
    }

    /// Compatibility wrapper over [`Resolver::resolve_import_detailed`].
    #[must_use]
    pub fn resolve_import(
        &self,
        imports: &ImportMap,
        package: Option<&PackageName>,
        name: &Name,
    ) -> Option<TypeName> {
        match self.resolve_import_detailed(imports, package, name) {
            TypeLookup::Found(ty) => Some(ty),
            TypeLookup::Ambiguous(_) | TypeLookup::NotFound => None,
        }
    }

    /// Best-effort validation of import declarations.
    ///
    /// The resolver is resilient by design: broken/unknown imports should not
    /// prevent resolution of the rest of the file. This helper is a lightweight
    /// diagnostic hook for higher layers (IDE, tests).
    #[must_use]
    pub fn diagnose_imports(&self, imports: &ImportMap) -> Vec<nova_types::Diagnostic> {
        let mut diags = Vec::new();

        // Duplicate single-type imports (`import a.Foo; import b.Foo;`).
        let mut single_type_by_name: HashMap<Name, Vec<String>> = HashMap::new();
        let mut single_type_span: HashMap<Name, nova_types::Span> = HashMap::new();
        let mut single_type_seen: HashMap<Name, HashSet<String>> = HashMap::new();
        for import in &imports.type_single {
            single_type_span
                .entry(import.imported.clone())
                .or_insert(import.range);

            let Some(ty) = self.resolve_type_in_index(&import.path) else {
                continue;
            };
            let target = ty.as_str().to_string();
            if !single_type_seen
                .entry(import.imported.clone())
                .or_default()
                .insert(target.clone())
            {
                diags.push(duplicate_import_diagnostic(import.range, &target));
                continue;
            }
            let targets = single_type_by_name
                .entry(import.imported.clone())
                .or_default();
            targets.push(target);
        }
        for (name, paths) in single_type_by_name {
            if paths.len() <= 1 {
                continue;
            }
            let span = single_type_span
                .get(&name)
                .copied()
                .unwrap_or_else(|| nova_types::Span::new(0, 0));
            diags.push(ambiguous_import_diagnostic(span, name.as_str(), &paths));
        }

        for import in &imports.type_single {
            if self.resolve_type_in_index(&import.path).is_none() {
                diags.push(unresolved_import_diagnostic(
                    import.range,
                    &import.path.to_dotted(),
                ));
            }
        }

        for import in &imports.type_star {
            let package = package_name_from_qualified(&import.path);

            // `import X.*;` is valid if `X` names either:
            // - a package (importing all types declared in that package), or
            // - a type (importing all member types declared in that type).
            if self.package_exists(&package) {
                continue;
            }

            if self.resolve_type_in_index(&import.path).is_none() {
                let prefix = import.path.to_dotted();
                diags.push(unresolved_import_diagnostic(
                    import.range,
                    &format!("{prefix}.*"),
                ));
            }
        }

        // Duplicate static single imports (`import static a.Foo.x; import static b.Bar.x;`).
        //
        // Note: static imports can introduce both *values* (fields/methods) and *types*
        // (member types). This diagnostic pass treats both as valid static-import targets.
        let mut static_single_by_name: HashMap<Name, Vec<String>> = HashMap::new();
        let mut static_single_span: HashMap<Name, nova_types::Span> = HashMap::new();
        let mut static_single_seen: HashMap<Name, HashSet<String>> = HashMap::new();
        for import in &imports.static_single {
            static_single_span
                .entry(import.imported.clone())
                .or_insert(import.range);

            let Some(owner) = self.resolve_type_in_index(&import.ty) else {
                continue;
            };

            let imports_member = self
                .resolve_static_member_in_index(&owner, &import.member)
                .is_some()
                || self
                    .resolve_static_imported_member_type(&import.ty, &import.member)
                    .is_some();
            if !imports_member {
                continue;
            }

            let target = format!("{}.{}", owner.as_str(), import.member.as_str());
            if !static_single_seen
                .entry(import.imported.clone())
                .or_default()
                .insert(target.clone())
            {
                diags.push(duplicate_import_diagnostic(
                    import.range,
                    &format!("static {target}"),
                ));
                continue;
            }
            let targets = static_single_by_name
                .entry(import.imported.clone())
                .or_default();
            targets.push(target);
        }
        for (name, paths) in static_single_by_name {
            if paths.len() <= 1 {
                continue;
            }
            let span = static_single_span
                .get(&name)
                .copied()
                .unwrap_or_else(|| nova_types::Span::new(0, 0));
            diags.push(ambiguous_import_diagnostic(span, name.as_str(), &paths));
        }

        for import in &imports.static_single {
            let Some(owner) = self.resolve_type_in_index(&import.ty) else {
                diags.push(unresolved_import_diagnostic(
                    import.range,
                    &format!("static {}.{}", import.ty.to_dotted(), import.member),
                ));
                continue;
            };

            let resolves = self
                .resolve_static_member_in_index(&owner, &import.member)
                .is_some()
                || self
                    .resolve_static_imported_member_type(&import.ty, &import.member)
                    .is_some();

            if !resolves {
                diags.push(unresolved_import_diagnostic(
                    import.range,
                    &format!("static {}.{}", import.ty.to_dotted(), import.member),
                ));
            }
        }

        for import in &imports.static_star {
            if self.resolve_type_in_index(&import.ty).is_none() {
                diags.push(unresolved_import_diagnostic(
                    import.range,
                    &format!("static {}.*", import.ty.to_dotted()),
                ));
            }
        }

        // Ensure deterministic diagnostic ordering. `diagnose_imports` uses
        // `HashMap`-based grouping for ambiguous imports, which would otherwise
        // produce non-deterministic output ordering due to randomized hashing.
        //
        // Sort by span (source order), then by code/message for stable tie-breaks.
        diags.sort_by(|a, b| {
            let a_span = a.span.unwrap_or(nova_types::Span::new(0, 0));
            let b_span = b.span.unwrap_or(nova_types::Span::new(0, 0));
            (
                a_span.start,
                a_span.end,
                a.code.as_ref(),
                a.message.as_str(),
            )
                .cmp(&(
                    b_span.start,
                    b_span.end,
                    b.code.as_ref(),
                    b.message.as_str(),
                ))
        });

        diags
    }

    /// Resolve a qualified name as a type using the external indexes.
    #[must_use]
    pub fn resolve_qualified_name(&self, path: &QualifiedName) -> Option<TypeName> {
        self.resolve_type_in_index(path)
    }

    /// Resolve a qualified type name in the context of a scope.
    ///
    /// This is an important extension over [`Resolver::resolve_qualified_name`]:
    /// Java code frequently refers to nested types through an imported/enclosing
    /// outer type, e.g. `Map.Entry`, where `Map` is a simple name resolved via
    /// imports/same-package/java.lang.
    ///
    /// The algorithm is:
    /// 1. Try resolving the path as a fully-qualified name (fast path, supports
    ///    `java.util.Map.Entry`).
    /// 2. Otherwise resolve the first segment as a simple type name in `scope`,
    ///    then append remaining segments as binary nested type separators (`$`).
    pub fn resolve_qualified_type_in_scope(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        path: &QualifiedName,
    ) -> Option<TypeName> {
        let resolved = self.resolve_qualified_type_resolution_in_scope(scopes, scope, path)?;
        match resolved {
            TypeResolution::External(ty) => Some(ty),
            TypeResolution::Source(item) => self.type_name_for_source(scopes, item),
        }
    }

    /// Resolve a simple name in the *type namespace* against a given scope.
    ///
    /// This intentionally ignores the value namespace (locals/params/fields/
    /// methods) to match Java's name resolution rules in type contexts (JLS 6.5).
    ///
    /// Import-scope lookup follows the same precedence as [`Resolver::resolve_import_detailed`]:
    /// 1) Single-type imports
    /// 2) Same-package types
    /// 3) Type-import-on-demand (`.*`) imports, including the implicit `java.lang.*`
    ///
    /// If the on-demand import set yields multiple distinct candidates (including `java.lang`),
    /// the reference is ambiguous and this returns `None`. Use
    /// [`Resolver::resolve_type_name_detailed`] to preserve ambiguity information for
    /// diagnostics.
    pub fn resolve_type_name(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> Option<TypeResolution> {
        self.resolve_type_name_detailed(scopes, scope, name)
            .into_option()
    }

    /// Like [`Resolver::resolve_type_name`], but preserves ambiguity.
    pub fn resolve_type_name_detailed(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> TypeNameResolution {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = scopes.scope(id);

            if let Some(ty) = data.types.get(name) {
                return TypeNameResolution::Resolved(ty.clone());
            }

            match &data.kind {
                ScopeKind::Import { imports, package } => {
                    match self.resolve_single_type_imports_detailed(imports, name) {
                        TypeLookup::Found(ty) => {
                            return TypeNameResolution::Resolved(
                                self.type_resolution_from_name(ty),
                            );
                        }
                        TypeLookup::Ambiguous(types) => {
                            return TypeNameResolution::Ambiguous(
                                types
                                    .into_iter()
                                    .map(|ty| self.type_resolution_from_name(ty))
                                    .collect(),
                            );
                        }
                        TypeLookup::NotFound => {}
                    }

                    if let Some(pkg) = package {
                        if let Some(ty) = self.resolve_type_in_package_index(pkg, name) {
                            return TypeNameResolution::Resolved(
                                self.type_resolution_from_name(ty),
                            );
                        }
                    }

                    match self.resolve_on_demand_type_imports_detailed(imports, name) {
                        TypeLookup::Found(ty) => {
                            return TypeNameResolution::Resolved(
                                self.type_resolution_from_name(ty),
                            );
                        }
                        TypeLookup::Ambiguous(types) => {
                            return TypeNameResolution::Ambiguous(
                                types
                                    .into_iter()
                                    .map(|ty| self.type_resolution_from_name(ty))
                                    .collect(),
                            );
                        }
                        TypeLookup::NotFound => {}
                    }
                }
                ScopeKind::Universe => {
                    // `java.lang.*` is always implicitly available.
                    if let Some(ty) = self.resolve_type_in_java_lang(name) {
                        return TypeNameResolution::Resolved(self.type_resolution_from_name(ty));
                    }
                }
                _ => {}
            }

            current = data.parent;
        }

        TypeNameResolution::Unresolved
    }

    /// Like [`Resolver::resolve_qualified_type_in_scope`], but preserves whether
    /// the resolved type is sourced from the current file (`ItemTree`) or from
    /// an external index (JDK/classpath).
    pub fn resolve_qualified_type_resolution_in_scope(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        path: &QualifiedName,
    ) -> Option<TypeResolution> {
        if let Some(ty) = self.resolve_qualified_name(path) {
            return Some(self.type_resolution_from_name(ty));
        }

        let segments = path.segments();
        let (first, rest) = segments.split_first()?;

        if rest.is_empty() {
            return self.resolve_type_name(scopes, scope, first);
        }

        let owner = self.resolve_type_name(scopes, scope, first)?;

        let owner_name = match &owner {
            TypeResolution::External(ty) => ty.as_str().to_string(),
            TypeResolution::Source(item) => self
                .type_name_for_source(scopes, *item)?
                .as_str()
                .to_string(),
        };

        // 1) Prefer local ItemTree types using binary names (`$` separators).
        let mut candidate_binary = owner_name.clone();
        for seg in rest {
            candidate_binary.push('$');
            candidate_binary.push_str(seg.as_str());
        }
        if let Some(item) = scopes.item_by_type_name(&TypeName::new(candidate_binary.clone())) {
            return Some(TypeResolution::Source(item));
        }

        // 2) External indices vary in how they model nested types:
        //    - some accept source-like `Outer.Inner` names directly
        //    - others use binary names (`Outer$Inner`)
        //
        // Prefer a binary lookup (`Outer$Inner...`) to avoid accidentally resolving types from a
        // subpackage (`Outer.Inner...`) when the qualifier resolves as a type.
        let expected_binary = candidate_binary;
        if let Some(ty) =
            self.resolve_type_in_index_exact(&QualifiedName::from_dotted(&expected_binary))
        {
            return Some(self.type_resolution_from_name(ty));
        }

        // Fallback: pass a dotted candidate (`Outer.Inner...`) through `resolve_type_in_index`,
        // which in turn uses `resolve_type_with_nesting` to translate to `$` as needed.
        let mut candidate_dotted = owner_name.replace('$', ".");
        for seg in rest {
            candidate_dotted.push('.');
            candidate_dotted.push_str(seg.as_str());
        }

        self.resolve_type_in_index(&QualifiedName::from_dotted(&candidate_dotted))
            .and_then(|ty| {
                (ty.as_str() == expected_binary).then_some(self.type_resolution_from_name(ty))
            })
    }

    /// Resolve a simple name against a given scope.
    ///
    /// Note: this performs a best-effort lookup across *non-method* namespaces:
    /// - value namespace (locals/parameters/fields)
    /// - type namespace (imports, same-package, `java.lang.*`, ...)
    /// - static imports (as `Resolution::StaticMember`)
    /// - package segments (for qualified name resolution)
    ///
    /// For unqualified method invocation lookup (`foo(...)`), use
    /// [`Resolver::resolve_method_name`] instead.
    pub fn resolve_name(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> Option<Resolution> {
        self.resolve_name_detailed(scopes, scope, name)
            .into_option()
    }

    /// Like [`Resolver::resolve_name`], but reports ambiguity.
    pub fn resolve_name_detailed(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> NameResolution {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = scopes.scope(id);

            if let Some(value) = data.values.get(name) {
                return NameResolution::Resolved(value.clone());
            }

            if let Some(ty) = data.types.get(name) {
                return NameResolution::Resolved(Resolution::Type(ty.clone()));
            }

            match &data.kind {
                ScopeKind::Import { imports, package } => {
                    match self.resolve_static_imports(imports, name) {
                        NameResolution::Resolved(res) => return NameResolution::Resolved(res),
                        NameResolution::Ambiguous(candidates) => {
                            return NameResolution::Ambiguous(candidates)
                        }
                        NameResolution::Unresolved => {}
                    }

                    // Type name lookup order mirrors `resolve_import_detailed`:
                    // 1) single-type imports
                    // 2) same-package types
                    // 3) on-demand imports (star imports, including implicit `java.lang.*`;
                    //    ambiguity is reported)
                    match self.resolve_single_type_imports(imports, name) {
                        NameResolution::Resolved(res) => return NameResolution::Resolved(res),
                        NameResolution::Ambiguous(candidates) => {
                            return NameResolution::Ambiguous(candidates)
                        }
                        NameResolution::Unresolved => {}
                    }

                    if let Some(pkg) = package {
                        if let Some(ty) = self.resolve_type_in_package_index(pkg, name) {
                            return NameResolution::Resolved(Resolution::Type(
                                self.type_resolution_from_name(ty),
                            ));
                        }
                    }

                    match self.resolve_on_demand_type_imports_detailed(imports, name) {
                        TypeLookup::Found(ty) => {
                            return NameResolution::Resolved(Resolution::Type(
                                self.type_resolution_from_name(ty),
                            ));
                        }
                        TypeLookup::Ambiguous(types) => {
                            return NameResolution::Ambiguous(
                                types
                                    .into_iter()
                                    .map(|ty| Resolution::Type(self.type_resolution_from_name(ty)))
                                    .collect(),
                            );
                        }
                        TypeLookup::NotFound => {}
                    }
                }
                ScopeKind::Package { package: Some(pkg) } => {
                    // Allow resolving subpackages in a qualified name context.
                    let next = append_package(pkg, name);
                    if self.package_exists(&next) {
                        return NameResolution::Resolved(Resolution::Package(PackageId::new(
                            next.to_dotted(),
                        )));
                    }
                }
                ScopeKind::Package { package: None } => {}
                ScopeKind::Universe => {
                    // `java.lang.*` is always implicitly available.
                    if let Some(ty) = self.resolve_type_in_java_lang(name) {
                        return NameResolution::Resolved(Resolution::Type(
                            self.type_resolution_from_name(ty),
                        ));
                    }
                }
                _ => {}
            }

            current = data.parent;
        }
        NameResolution::Unresolved
    }

    /// Resolve a simple name in the *value* namespace (locals/parameters/fields),
    /// plus static imports.
    ///
    /// This is suitable for resolving `hir::Expr::Name` nodes that are not part
    /// of a method invocation.
    pub fn resolve_value_name(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> Option<Resolution> {
        self.resolve_value_name_detailed(scopes, scope, name)
            .into_option()
    }

    /// Like [`Resolver::resolve_value_name`], but reports ambiguity from static
    /// imports.
    pub fn resolve_value_name_detailed(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> NameResolution {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = scopes.scope(id);

            if let Some(value) = data.values.get(name) {
                return NameResolution::Resolved(value.clone());
            }

            if let ScopeKind::Import { imports, .. } = &data.kind {
                match self.resolve_static_imports(imports, name) {
                    NameResolution::Resolved(res) => return NameResolution::Resolved(res),
                    NameResolution::Ambiguous(candidates) => {
                        return NameResolution::Ambiguous(candidates)
                    }
                    NameResolution::Unresolved => {}
                }
            }

            current = data.parent;
        }
        NameResolution::Unresolved
    }

    /// Resolve a simple name in the *method* namespace (unqualified method
    /// invocation), plus static imports.
    ///
    /// This is suitable for resolving the callee identifier in expressions like
    /// `foo(...)` (where the callee is not a field access / qualified expression).
    pub fn resolve_method_name(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> Option<Resolution> {
        self.resolve_method_name_detailed(scopes, scope, name)
            .into_option()
    }

    /// Like [`Resolver::resolve_method_name`], but reports ambiguity from static
    /// imports.
    pub fn resolve_method_name_detailed(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> NameResolution {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = scopes.scope(id);

            if let Some(methods) = data.methods.get(name) {
                return NameResolution::Resolved(Resolution::Methods(methods.clone()));
            }

            if let ScopeKind::Import { imports, .. } = &data.kind {
                match self.resolve_static_imports(imports, name) {
                    NameResolution::Resolved(res) => return NameResolution::Resolved(res),
                    NameResolution::Ambiguous(candidates) => {
                        return NameResolution::Ambiguous(candidates)
                    }
                    NameResolution::Unresolved => {}
                }
            }

            current = data.parent;
        }
        NameResolution::Unresolved
    }

    fn resolve_single_type_imports(&self, imports: &ImportMap, name: &Name) -> NameResolution {
        match self.resolve_single_type_imports_detailed(imports, name) {
            TypeLookup::Found(ty) => {
                NameResolution::Resolved(Resolution::Type(self.type_resolution_from_name(ty)))
            }
            TypeLookup::Ambiguous(types) => NameResolution::Ambiguous(
                types
                    .into_iter()
                    .map(|ty| Resolution::Type(self.type_resolution_from_name(ty)))
                    .collect(),
            ),
            TypeLookup::NotFound => NameResolution::Unresolved,
        }
    }

    fn resolve_single_type_imports_detailed(&self, imports: &ImportMap, name: &Name) -> TypeLookup {
        let mut candidates = Vec::<TypeName>::new();
        for import in &imports.type_single {
            if &import.imported != name {
                continue;
            }
            if let Some(ty) = self.resolve_type_in_index(&import.path) {
                if !candidates.contains(&ty) {
                    candidates.push(ty);
                }
            }
        }

        for import in &imports.static_single {
            if &import.imported != name {
                continue;
            }

            if let Some(ty) = self.resolve_static_imported_member_type(&import.ty, &import.member) {
                if !candidates.contains(&ty) {
                    candidates.push(ty);
                }
            }
        }

        match candidates.len() {
            0 => TypeLookup::NotFound,
            1 => TypeLookup::Found(candidates.remove(0)),
            _ => TypeLookup::Ambiguous(candidates),
        }
    }

    fn resolve_static_star_type_imports_detailed(
        &self,
        imports: &ImportMap,
        name: &Name,
    ) -> TypeLookup {
        let mut seen = HashSet::<TypeName>::new();
        let mut candidates = Vec::<TypeName>::new();

        for import in &imports.static_star {
            if let Some(ty) = self.resolve_static_imported_member_type(&import.ty, name) {
                if seen.insert(ty.clone()) {
                    candidates.push(ty);
                }
            }
        }

        match candidates.len() {
            0 => TypeLookup::NotFound,
            1 => TypeLookup::Found(candidates.remove(0)),
            _ => TypeLookup::Ambiguous(candidates),
        }
    }

    fn resolve_star_type_imports_detailed(&self, imports: &ImportMap, name: &Name) -> TypeLookup {
        let mut seen = HashSet::<TypeName>::new();
        let mut candidates = Vec::<TypeName>::new();

        for import in &imports.type_star {
            // JLS 7.5.2: `import X.*;` imports from either:
            // - type `X` (all accessible member types declared in the type), or
            // - package `X` (all accessible types declared in the package).
            //
            // If `X` resolves as a type name, it is treated as a type-import-on-demand, even if a
            // package of the same name exists (matches `javac` behavior when class/package names
            // overlap in bytecode).
            if let Some(owner) = self.resolve_type_in_index(&import.path) {
                // Prefer resolving the member as a binary nested name (`Outer$Inner`) so we don't
                // accidentally treat a subpackage type (`Outer.Inner`) as a member type.
                let binary_candidate =
                    QualifiedName::from_dotted(&format!("{}${}", owner.as_str(), name.as_str()));
                if let Some(ty) = self.resolve_type_in_index_exact(&binary_candidate) {
                    if seen.insert(ty.clone()) {
                        candidates.push(ty);
                    }
                    continue;
                }

                // Best-effort fallback: allow indices that only resolve dotted names through the
                // nesting heuristic, but ensure the resolved type is actually nested under `owner`
                // (i.e. `owner$Name`).
                let prefix = import.path.to_dotted();
                let dotted_candidate =
                    QualifiedName::from_dotted(&format!("{prefix}.{}", name.as_str()));
                if let Some(ty) = self.resolve_type_in_index(&dotted_candidate) {
                    if ty.as_str().starts_with(&format!("{}$", owner.as_str()))
                        && seen.insert(ty.clone())
                    {
                        candidates.push(ty);
                    }
                }
                continue;
            }

            let package = package_name_from_qualified(&import.path);
            if let Some(ty) = self.resolve_type_in_package_index(&package, name) {
                if seen.insert(ty.clone()) {
                    candidates.push(ty);
                }
            }
        }

        match candidates.len() {
            0 => TypeLookup::NotFound,
            1 => TypeLookup::Found(candidates.remove(0)),
            _ => TypeLookup::Ambiguous(candidates),
        }
    }

    fn resolve_on_demand_type_imports_detailed(
        &self,
        imports: &ImportMap,
        name: &Name,
    ) -> TypeLookup {
        // JLS 7.5.2: `java.lang.*` is implicitly imported by every compilation unit and
        // participates in the same on-demand import set as explicit `import p.*;` declarations.
        //
        // This means that if both an explicit star import and the implicit `java.lang.*` import
        // introduce different types with the same simple name, the reference is ambiguous.
        let mut seen = HashSet::<TypeName>::new();
        let mut candidates = Vec::<TypeName>::new();

        match self.resolve_star_type_imports_detailed(imports, name) {
            TypeLookup::Found(ty) => {
                if seen.insert(ty.clone()) {
                    candidates.push(ty);
                }
            }
            TypeLookup::Ambiguous(types) => {
                for ty in types {
                    if seen.insert(ty.clone()) {
                        candidates.push(ty);
                    }
                }
            }
            TypeLookup::NotFound => {}
        }

        match self.resolve_static_star_type_imports_detailed(imports, name) {
            TypeLookup::Found(ty) => {
                if seen.insert(ty.clone()) {
                    candidates.push(ty);
                }
            }
            TypeLookup::Ambiguous(types) => {
                for ty in types {
                    if seen.insert(ty.clone()) {
                        candidates.push(ty);
                    }
                }
            }
            TypeLookup::NotFound => {}
        }

        if let Some(ty) = self.resolve_type_in_java_lang(name) {
            if seen.insert(ty.clone()) {
                candidates.push(ty);
            }
        }

        match candidates.len() {
            0 => TypeLookup::NotFound,
            1 => TypeLookup::Found(candidates.remove(0)),
            _ => TypeLookup::Ambiguous(candidates),
        }
    }

    fn resolve_static_imports(&self, imports: &ImportMap, name: &Name) -> NameResolution {
        match self.resolve_static_imports_detailed(imports, name) {
            StaticLookup::Found(member) => NameResolution::Resolved(Resolution::StaticMember(
                self.static_member_resolution_from_id(member),
            )),
            StaticLookup::Ambiguous(members) => NameResolution::Ambiguous(
                members
                    .into_iter()
                    .map(|m| Resolution::StaticMember(self.static_member_resolution_from_id(m)))
                    .collect(),
            ),
            StaticLookup::NotFound => NameResolution::Unresolved,
        }
    }
}

pub(crate) fn append_package(base: &PackageName, name: &Name) -> PackageName {
    let mut next = base.clone();
    next.push(name.clone());
    next
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    use nova_core::FileId;
    use nova_hir::item_tree;
    use nova_jdk::JdkIndex;
    use nova_types::Span;

    use crate::import_map::{StaticSingleImport, StaticStarImport};
    use crate::scopes::build_scopes_for_item_tree;

    #[derive(Default)]
    struct TestIndex {
        types: HashMap<String, TypeName>,
        package_to_types: HashMap<String, HashMap<String, TypeName>>,
        packages: HashSet<String>,
        static_members: HashMap<String, HashMap<String, StaticMemberId>>,
    }

    impl TestIndex {
        fn add_type(&mut self, package: &str, name: &str) -> TypeName {
            let fq = if package.is_empty() {
                name.to_string()
            } else {
                format!("{package}.{name}")
            };
            let id = TypeName::new(fq.clone());
            self.types.insert(fq, id.clone());
            self.packages.insert(package.to_string());
            self.package_to_types
                .entry(package.to_string())
                .or_default()
                .insert(name.to_string(), id.clone());
            id
        }

        fn add_static_member(&mut self, owner: &str, name: &str) -> StaticMemberId {
            let id = StaticMemberId::new(format!("{owner}::{name}"));
            self.static_members
                .entry(owner.to_string())
                .or_default()
                .insert(name.to_string(), id.clone());
            id
        }
    }

    impl TypeIndex for TestIndex {
        fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
            self.types.get(&name.to_dotted()).cloned()
        }

        fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
            self.package_to_types
                .get(&package.to_dotted())
                .and_then(|m| m.get(name.as_str()))
                .cloned()
        }

        fn package_exists(&self, package: &PackageName) -> bool {
            self.packages.contains(&package.to_dotted())
        }

        fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
            self.static_members
                .get(owner.as_str())
                .and_then(|m| m.get(name.as_str()))
                .cloned()
        }
    }

    #[test]
    fn static_star_import_detects_ambiguity() {
        let jdk = JdkIndex::new();
        let mut index = TestIndex::default();
        index.add_type("q", "Util");
        index.add_static_member("q.Util", "max");

        let resolver = Resolver::new(&jdk).with_classpath(&index);

        let mut imports = ImportMap::default();
        imports.static_star.push(StaticStarImport {
            ty: QualifiedName::from_dotted("java.lang.Math"),
            range: Span::new(0, 0),
        });
        imports.static_star.push(StaticStarImport {
            ty: QualifiedName::from_dotted("q.Util"),
            range: Span::new(0, 0),
        });

        assert_eq!(
            resolver.resolve_static_imports_detailed(&imports, &Name::from("max")),
            StaticLookup::Ambiguous(vec![
                StaticMemberId::new("java.lang.Math::max"),
                StaticMemberId::new("q.Util::max"),
            ])
        );
    }

    #[test]
    fn static_single_import_detects_ambiguity() {
        let jdk = JdkIndex::new();
        let mut index = TestIndex::default();
        index.add_type("q", "Util");
        index.add_static_member("q.Util", "max");

        let resolver = Resolver::new(&jdk).with_classpath(&index);

        let mut imports = ImportMap::default();
        imports.static_single.push(StaticSingleImport {
            ty: QualifiedName::from_dotted("java.lang.Math"),
            member: Name::from("max"),
            imported: Name::from("max"),
            range: Span::new(0, 0),
        });
        imports.static_single.push(StaticSingleImport {
            ty: QualifiedName::from_dotted("q.Util"),
            member: Name::from("max"),
            imported: Name::from("max"),
            range: Span::new(0, 0),
        });

        assert_eq!(
            resolver.resolve_static_imports_detailed(&imports, &Name::from("max")),
            StaticLookup::Ambiguous(vec![
                StaticMemberId::new("java.lang.Math::max"),
                StaticMemberId::new("q.Util::max"),
            ])
        );
    }

    #[test]
    fn same_package_beats_star_import() {
        let jdk = JdkIndex::new();
        let mut index = TestIndex::default();
        let same = index.add_type("p", "Foo");
        index.add_type("q", "Foo");

        let resolver = Resolver::new(&jdk).with_classpath(&index);

        let mut tree = item_tree::ItemTree {
            package: Some(item_tree::PackageDecl {
                name: "p".to_string(),
                range: Span::new(0, 0),
            }),
            ..Default::default()
        };
        tree.imports.push(item_tree::Import {
            is_static: false,
            is_star: true,
            path: "q".to_string(),
            range: Span::new(0, 0),
        });

        let scope_result = build_scopes_for_item_tree(FileId::new(0), &tree);
        assert_eq!(
            resolver.resolve_name(
                &scope_result.scopes,
                scope_result.file_scope,
                &Name::from("Foo")
            ),
            Some(Resolution::Type(TypeResolution::External(same.clone())))
        );

        let imports = ImportMap::from_item_tree(&tree);
        let pkg = PackageName::from_dotted("p");
        assert_eq!(
            resolver.resolve_import_detailed(&imports, Some(&pkg), &Name::from("Foo")),
            TypeLookup::Found(same.clone())
        );
        assert_eq!(
            resolver.resolve_import(&imports, Some(&pkg), &Name::from("Foo")),
            Some(same)
        );
    }

    #[test]
    fn ambiguous_star_import_is_detected() {
        let jdk = JdkIndex::new();
        let mut index = TestIndex::default();
        let foo_a = index.add_type("a", "Foo");
        let foo_b = index.add_type("b", "Foo");

        let resolver = Resolver::new(&jdk).with_classpath(&index);

        let mut tree = item_tree::ItemTree::default();
        tree.imports.push(item_tree::Import {
            is_static: false,
            is_star: true,
            path: "a".to_string(),
            range: Span::new(0, 0),
        });
        tree.imports.push(item_tree::Import {
            is_static: false,
            is_star: true,
            path: "b".to_string(),
            range: Span::new(0, 0),
        });

        let scope_result = build_scopes_for_item_tree(FileId::new(0), &tree);
        assert_eq!(
            resolver.resolve_name_detailed(
                &scope_result.scopes,
                scope_result.file_scope,
                &Name::from("Foo")
            ),
            NameResolution::Ambiguous(vec![
                Resolution::Type(TypeResolution::External(foo_a.clone())),
                Resolution::Type(TypeResolution::External(foo_b.clone())),
            ])
        );
        assert_eq!(
            resolver.resolve_name(
                &scope_result.scopes,
                scope_result.file_scope,
                &Name::from("Foo")
            ),
            None
        );

        let imports = ImportMap::from_item_tree(&tree);
        assert_eq!(
            resolver.resolve_import_detailed(&imports, None, &Name::from("Foo")),
            TypeLookup::Ambiguous(vec![foo_a.clone(), foo_b.clone()])
        );
        assert_eq!(
            resolver.resolve_import(&imports, None, &Name::from("Foo")),
            None
        );
    }
}

fn is_java_package(package: &PackageName) -> bool {
    package
        .segments()
        .first()
        .is_some_and(|seg| seg.as_str() == "java")
}

fn is_java_qualified_name(name: &QualifiedName) -> bool {
    name.segments()
        .first()
        .is_some_and(|seg| seg.as_str() == "java")
}

fn package_name_from_qualified(path: &QualifiedName) -> PackageName {
    let mut package = PackageName::root();
    for seg in path.segments() {
        package.push(seg.clone());
    }
    package
}

pub(crate) fn resolve_type_with_nesting(
    index: &dyn TypeIndex,
    name: &QualifiedName,
) -> Option<TypeName> {
    index
        .resolve_type(name)
        .or_else(|| resolve_nested_type(index, name))
}

fn resolve_nested_type(index: &dyn TypeIndex, name: &QualifiedName) -> Option<TypeName> {
    // Java source refers to nested classes as `Outer.Inner`, but classpath/JDK
    // indices tend to use binary names (`Outer$Inner`). When a qualified name
    // fails to resolve as-is, try progressively treating the rightmost segments
    // as nested types.
    let segments = name.segments();
    if segments.len() < 2 {
        return None;
    }

    // Prefer longer package prefixes first (e.g. `java.util.Map.Entry` should try
    // `java.util.Map$Entry` before `java$util$Map$Entry`).
    for split_at in (0..segments.len() - 1).rev() {
        let type_segments = &segments[split_at..];
        if type_segments.len() < 2 {
            continue;
        }

        let mut candidate = String::new();
        if split_at > 0 {
            for (idx, seg) in segments[..split_at].iter().enumerate() {
                if idx > 0 {
                    candidate.push('.');
                }
                candidate.push_str(seg.as_str());
            }
            candidate.push('.');
        }

        for (idx, seg) in type_segments.iter().enumerate() {
            if idx > 0 {
                candidate.push('$');
            }
            candidate.push_str(seg.as_str());
        }

        let candidate = QualifiedName::from_dotted(&candidate);
        if let Some(ty) = index.resolve_type(&candidate) {
            return Some(ty);
        }
    }

    None
}
