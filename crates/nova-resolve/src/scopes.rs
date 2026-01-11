//! Scope graph building and basic name resolution.

use std::collections::HashMap;

use nova_core::{Name, PackageId, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_hir::{Block, CompilationUnit, ImportDecl, MethodDecl, Stmt, TypeDecl};

pub type ScopeId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Local,
    Parameter,
    Field,
    Method,
    Type(TypeName),
    Package(PackageId),
    StaticMember(StaticMemberId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeGraph {
    scopes: Vec<ScopeData>,
}

impl ScopeGraph {
    pub fn scope(&self, id: ScopeId) -> &ScopeData {
        &self.scopes[id]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeData {
    parent: Option<ScopeId>,
    kind: ScopeKind,
    values: HashMap<Name, Resolution>,
    types: HashMap<Name, TypeName>,
}

impl ScopeData {
    pub fn parent(&self) -> Option<ScopeId> {
        self.parent
    }

    pub fn kind(&self) -> &ScopeKind {
        &self.kind
    }

    pub(crate) fn values(&self) -> &HashMap<Name, Resolution> {
        &self.values
    }

    pub(crate) fn types(&self) -> &HashMap<Name, TypeName> {
        &self.types
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeKind {
    Universe,
    Package {
        package: Option<PackageName>,
    },
    Import {
        imports: Vec<ImportDecl>,
        package: Option<PackageName>,
    },
    File,
    Class {
        type_id: TypeName,
    },
    Method,
    Block,
}

/// A name resolver that consults the JDK index and an optional project/classpath index.
pub struct Resolver<'a> {
    jdk: &'a dyn TypeIndex,
    classpath: Option<&'a dyn TypeIndex>,
}

impl<'a> Resolver<'a> {
    pub fn new(jdk: &'a dyn TypeIndex) -> Self {
        Self {
            jdk,
            classpath: None,
        }
    }

    pub fn with_classpath(mut self, classpath: &'a dyn TypeIndex) -> Self {
        self.classpath = Some(classpath);
        self
    }

    fn resolve_type_in_index(&self, name: &QualifiedName) -> Option<TypeName> {
        if let Some(classpath) = self.classpath {
            if let Some(ty) = resolve_type_with_nesting(classpath, name) {
                return Some(ty);
            }
        }

        resolve_type_with_nesting(self.jdk, name)
    }

    fn resolve_type_in_package_index(
        &self,
        package: &PackageName,
        name: &Name,
    ) -> Option<TypeName> {
        self.classpath
            .and_then(|cp| cp.resolve_type_in_package(package, name))
            .or_else(|| self.jdk.resolve_type_in_package(package, name))
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.classpath.is_some_and(|cp| cp.package_exists(package))
            || self.jdk.package_exists(package)
    }

    pub(crate) fn resolve_static_member(
        &self,
        owner: &TypeName,
        name: &Name,
    ) -> Option<StaticMemberId> {
        self.classpath
            .and_then(|cp| cp.resolve_static_member(owner, name))
            .or_else(|| self.jdk.resolve_static_member(owner, name))
    }

    /// Resolve a qualified name as a type.
    pub fn resolve_qualified_name(&self, path: &QualifiedName) -> Option<TypeName> {
        self.resolve_type_in_index(path)
    }

    /// Resolve a qualified type name in the context of a scope.
    ///
    /// This is a small (but important) extension over [`Resolver::resolve_qualified_name`]:
    /// Java code frequently refers to nested types through an imported/enclosing
    /// outer type, e.g. `Map.Entry`, where `Map` is a simple name resolved via
    /// imports/same-package/java.lang.
    ///
    /// The algorithm is:
    /// 1. Try resolving the path as a fully-qualified name (fast path, supports
    ///    `java.util.Map.Entry`).
    /// 2. Otherwise resolve the first segment as a simple type name in `scope`,
    ///    then append remaining segments as binary nested type separators (`$`)
    ///    and resolve via the type index (`java.util.Map$Entry`).
    pub fn resolve_qualified_type_in_scope(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        path: &QualifiedName,
    ) -> Option<TypeName> {
        if let Some(ty) = self.resolve_qualified_name(path) {
            return Some(ty);
        }

        let segments = path.segments();
        let (first, rest) = segments.split_first()?;

        if rest.is_empty() {
            return match self.resolve_name(scopes, scope, first)? {
                Resolution::Type(ty) => Some(ty),
                _ => None,
            };
        }

        let owner = match self.resolve_name(scopes, scope, first)? {
            Resolution::Type(ty) => ty,
            _ => return None,
        };

        let mut candidate = owner.as_str().to_string();
        for seg in rest {
            candidate.push('$');
            candidate.push_str(seg.as_str());
        }

        self.resolve_type_in_index(&QualifiedName::from_dotted(&candidate))
    }

    /// Resolve a simple name against a given scope.
    pub fn resolve_name(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> Option<Resolution> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = scopes.scope(id);

            if let Some(value) = data.values.get(name) {
                return Some(value.clone());
            }

            if let Some(ty) = data.types.get(name) {
                return Some(Resolution::Type(ty.clone()));
            }

            match &data.kind {
                ScopeKind::Import { imports, .. } => {
                    if let Some(res) = self.resolve_static_imports(imports, name) {
                        return Some(res);
                    }
                    if let Some(ty) = self.resolve_import_types(imports, name) {
                        return Some(Resolution::Type(ty));
                    }
                }
                ScopeKind::Package { package } => {
                    if let Some(pkg) = package {
                        if let Some(ty) = self.resolve_type_in_package_index(pkg, name) {
                            return Some(Resolution::Type(ty));
                        }
                    }

                    // Allow resolving subpackages in a qualified name context.
                    if package
                        .as_ref()
                        .is_some_and(|pkg| self.package_exists(&append_package(pkg, name)))
                    {
                        let pkg = append_package(package.as_ref().unwrap(), name).to_dotted();
                        return Some(Resolution::Package(PackageId::new(pkg)));
                    }
                }
                ScopeKind::Universe => {
                    // `java.lang.*` is always implicitly available (after package + imports).
                    if let Some(ty) = self
                        .resolve_type_in_package_index(&PackageName::from_dotted("java.lang"), name)
                    {
                        return Some(Resolution::Type(ty));
                    }
                }
                _ => {}
            }

            current = data.parent;
        }
        None
    }

    /// Resolve a type name via the file's imports and package.
    pub fn resolve_import(&self, file: &CompilationUnit, name: &Name) -> Option<TypeName> {
        self.resolve_import_types(&file.imports, name)
            .or_else(|| {
                file.package
                    .as_ref()
                    .and_then(|pkg| self.resolve_type_in_package_index(pkg, name))
            })
            .or_else(|| {
                self.resolve_type_in_package_index(&PackageName::from_dotted("java.lang"), name)
            })
    }

    fn resolve_import_types(&self, imports: &[ImportDecl], name: &Name) -> Option<TypeName> {
        // 1) Explicit single-type imports (shadow star imports).
        for import in imports {
            if let ImportDecl::TypeSingle { ty, alias } = import {
                let import_name = alias.as_ref().or_else(|| ty.last());
                if import_name == Some(name) {
                    return self.resolve_type_in_index(ty);
                }
            }
        }

        // 2) Star imports.
        for import in imports {
            if let ImportDecl::TypeStar { package } = import {
                if let Some(ty) = self.resolve_type_in_package_index(package, name) {
                    return Some(ty);
                }
            }
        }

        None
    }

    fn resolve_static_imports(&self, imports: &[ImportDecl], name: &Name) -> Option<Resolution> {
        // 1) Explicit single static member imports.
        for import in imports {
            if let ImportDecl::StaticSingle { ty, member, alias } = import {
                let import_name = alias.as_ref().unwrap_or(member);
                if import_name == name {
                    let owner = self.resolve_type_in_index(ty)?;
                    let static_member = self.resolve_static_member(&owner, member)?;
                    return Some(Resolution::StaticMember(static_member));
                }
            }
        }

        // 2) Static star imports.
        for import in imports {
            if let ImportDecl::StaticStar { ty } = import {
                let owner = self.resolve_type_in_index(ty)?;
                if let Some(static_member) = self.resolve_static_member(&owner, name) {
                    return Some(Resolution::StaticMember(static_member));
                }
            }
        }

        None
    }
}

pub(crate) fn append_package(base: &PackageName, name: &Name) -> PackageName {
    let mut next = PackageName::from_dotted(&base.to_dotted());
    next.push(name.clone());
    next
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeBuildResult {
    pub scopes: ScopeGraph,
    pub file_scope: ScopeId,
    pub class_scopes: HashMap<String, ScopeId>,
    pub method_scopes: HashMap<String, ScopeId>,
    pub block_scopes: Vec<ScopeId>,
}

/// Build a scope graph for a compilation unit.
pub fn build_scopes(jdk: &dyn TypeIndex, file: &CompilationUnit) -> ScopeBuildResult {
    let resolver = Resolver::new(jdk);
    build_scopes_with_resolver(&resolver, file)
}

/// Build a scope graph using an already-configured [`Resolver`].
pub fn build_scopes_with_resolver(
    resolver: &Resolver<'_>,
    file: &CompilationUnit,
) -> ScopeBuildResult {
    build_scopes_with_resolver_and_cancel(resolver, file, || {})
}

/// Like [`build_scopes_with_resolver`], but cooperatively checks for cancellation.
///
/// Callers should provide a cheap closure that panics/unwinds when cancellation is requested.
pub fn build_scopes_with_resolver_and_cancel(
    resolver: &Resolver<'_>,
    file: &CompilationUnit,
    check_cancelled: impl FnMut(),
) -> ScopeBuildResult {
    ScopeBuilder::new(resolver, check_cancelled).build(file)
}

struct ScopeBuilder<'r, 'idx, F> {
    resolver: &'r Resolver<'idx>,
    check_cancelled: F,
    scopes: Vec<ScopeData>,
    class_scopes: HashMap<String, ScopeId>,
    method_scopes: HashMap<String, ScopeId>,
    block_scopes: Vec<ScopeId>,
}

impl<'r, 'idx, F> ScopeBuilder<'r, 'idx, F>
where
    F: FnMut(),
{
    fn new(resolver: &'r Resolver<'idx>, check_cancelled: F) -> Self {
        Self {
            resolver,
            check_cancelled,
            scopes: Vec::new(),
            class_scopes: HashMap::new(),
            method_scopes: HashMap::new(),
            block_scopes: Vec::new(),
        }
    }

    #[inline]
    fn check_cancelled(&mut self) {
        (self.check_cancelled)();
    }

    fn build(mut self, file: &CompilationUnit) -> ScopeBuildResult {
        self.check_cancelled();
        let universe = self.alloc_scope(None, ScopeKind::Universe);
        self.populate_universe(universe);

        let package = self.alloc_scope(
            Some(universe),
            ScopeKind::Package {
                package: file.package.clone(),
            },
        );
        let import = self.alloc_scope(
            Some(package),
            ScopeKind::Import {
                imports: file.imports.clone(),
                package: file.package.clone(),
            },
        );
        let file_scope = self.alloc_scope(Some(import), ScopeKind::File);

        for (idx, ty) in file.types.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            self.declare_top_level_type(file_scope, file.package.as_ref(), ty);
        }

        for (idx, ty) in file.types.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            self.build_type_scopes(file_scope, file.package.as_ref(), ty);
        }

        self.check_cancelled();
        ScopeBuildResult {
            scopes: ScopeGraph {
                scopes: self.scopes,
            },
            file_scope,
            class_scopes: self.class_scopes,
            method_scopes: self.method_scopes,
            block_scopes: self.block_scopes,
        }
    }

    fn populate_universe(&mut self, universe: ScopeId) {
        let primitives = [
            "boolean", "byte", "short", "int", "long", "char", "float", "double", "void",
        ];
        for (idx, prim) in primitives.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            self.scopes[universe]
                .types
                .insert(Name::from(*prim), TypeName::from(*prim));
        }

        // Populate common java.lang types from the JDK index.
        // We don't have a way to enumerate, so we hardcode the usual suspects used in tests.
        for (idx, ty) in ["Object", "String", "Integer", "System", "Math"]
            .iter()
            .enumerate()
        {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            let name = Name::from(*ty);
            if let Some(id) = self
                .resolver
                .resolve_type_in_package_index(&PackageName::from_dotted("java.lang"), &name)
            {
                self.scopes[universe].types.insert(name, id);
            }
        }
    }

    fn declare_top_level_type(
        &mut self,
        file_scope: ScopeId,
        package: Option<&PackageName>,
        ty: &TypeDecl,
    ) -> TypeName {
        self.check_cancelled();
        let fq = match package {
            Some(pkg) if !pkg.segments().is_empty() => {
                format!("{}.{}", pkg.to_dotted(), ty.name.as_str())
            }
            _ => ty.name.as_str().to_string(),
        };
        let id = TypeName::new(fq);
        self.scopes[file_scope]
            .types
            .insert(ty.name.clone(), id.clone());
        id
    }

    fn build_type_scopes(
        &mut self,
        parent: ScopeId,
        package: Option<&PackageName>,
        ty: &TypeDecl,
    ) -> ScopeId {
        self.check_cancelled();
        // Ensure the type is declared in the file scope before building the class scope.
        // This avoids order-dependence when multiple top-level types reference each other.
        let type_id = self.scopes[parent]
            .types
            .get(&ty.name)
            .cloned()
            .unwrap_or_else(|| self.declare_top_level_type(parent, package, ty));
        let class_scope = self.alloc_scope(
            Some(parent),
            ScopeKind::Class {
                type_id: type_id.clone(),
            },
        );
        self.class_scopes
            .insert(type_id.as_str().to_string(), class_scope);

        for (idx, field) in ty.fields.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            self.scopes[class_scope]
                .values
                .insert(field.name.clone(), Resolution::Field);
        }

        for (idx, method) in ty.methods.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            self.scopes[class_scope]
                .values
                .insert(method.name.clone(), Resolution::Method);
        }

        for (idx, nested) in ty.nested_types.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            // Nested types are in the class' type namespace.
            let nested_fq = format!("{}${}", type_id.as_str(), nested.name.as_str());
            self.scopes[class_scope]
                .types
                .insert(nested.name.clone(), TypeName::new(nested_fq));
        }

        for (idx, method) in ty.methods.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            self.build_method_scopes(class_scope, &type_id, method);
        }

        class_scope
    }

    fn build_method_scopes(
        &mut self,
        parent: ScopeId,
        owner: &TypeName,
        method: &MethodDecl,
    ) -> ScopeId {
        self.check_cancelled();
        let method_scope = self.alloc_scope(Some(parent), ScopeKind::Method);
        let key = format!("{}#{}", owner.as_str(), method.name.as_str());
        self.method_scopes.insert(key, method_scope);

        for (idx, param) in method.params.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            self.scopes[method_scope]
                .values
                .insert(param.name.clone(), Resolution::Parameter);
        }

        self.build_block_scopes(method_scope, &method.body);
        method_scope
    }

    fn build_block_scopes(&mut self, parent: ScopeId, block: &Block) -> ScopeId {
        self.check_cancelled();
        let block_scope = self.alloc_scope(Some(parent), ScopeKind::Block);
        self.block_scopes.push(block_scope);

        for (idx, stmt) in block.stmts.iter().enumerate() {
            if idx % 64 == 0 {
                self.check_cancelled();
            }
            match stmt {
                Stmt::Local(local) => {
                    self.scopes[block_scope]
                        .values
                        .insert(local.name.clone(), Resolution::Local);
                }
                Stmt::Block(inner) => {
                    self.build_block_scopes(block_scope, inner);
                }
            }
        }

        block_scope
    }

    fn alloc_scope(&mut self, parent: Option<ScopeId>, kind: ScopeKind) -> ScopeId {
        let id = self.scopes.len();
        self.scopes.push(ScopeData {
            parent,
            kind,
            values: HashMap::new(),
            types: HashMap::new(),
        });
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    use nova_classpath::{ClasspathEntry, ClasspathIndex};
    use nova_core::{QualifiedName, TypeIndex};
    use nova_hir::{FieldDecl, ImportDecl, LocalVarDecl, MethodDecl, ParamDecl, Stmt, TypeDecl};
    use nova_jdk::JdkIndex;

    fn test_dep_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
    }

    #[derive(Default)]
    struct TestIndex {
        types: HashMap<String, TypeName>,
        package_to_types: HashMap<String, HashMap<String, TypeName>>,
        packages: HashSet<String>,
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
    }

    #[test]
    fn local_shadows_field() {
        let jdk = JdkIndex::new();

        let mut ty = TypeDecl::new("C");
        ty.fields.push(FieldDecl::new("x"));
        let mut method = MethodDecl::new("m");
        method.body.stmts.push(Stmt::Local(LocalVarDecl::new("x")));
        ty.methods.push(method);

        let unit = CompilationUnit {
            package: None,
            imports: Vec::new(),
            types: vec![ty],
        };

        let result = build_scopes(&jdk, &unit);
        let block_scope = *result.block_scopes.first().expect("block scope");

        let resolver = Resolver::new(&jdk);
        let res = resolver.resolve_name(&result.scopes, block_scope, &Name::from("x"));
        assert_eq!(res, Some(Resolution::Local));
    }

    #[test]
    fn import_beats_same_package() {
        let jdk = JdkIndex::new();
        let mut index = TestIndex::default();
        let imported = index.add_type("q", "Foo");
        let _same = index.add_type("p", "Foo");

        let mut unit = CompilationUnit::new(Some(PackageName::from_dotted("p")));
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("q.Foo"),
            alias: None,
        });

        let result = build_scopes(&jdk, &unit);
        let resolver = Resolver::new(&jdk).with_classpath(&index);

        // Resolve from file scope (no locals/classes etc).
        let res = resolver.resolve_name(&result.scopes, result.file_scope, &Name::from("Foo"));
        assert_eq!(res, Some(Resolution::Type(imported)));
    }

    #[test]
    fn star_import_resolves_type() {
        let jdk = JdkIndex::new();
        let mut unit = CompilationUnit::new(None);
        unit.imports.push(ImportDecl::TypeStar {
            package: PackageName::from_dotted("java.util"),
        });

        let result = build_scopes(&jdk, &unit);
        let resolver = Resolver::new(&jdk);
        let res = resolver.resolve_name(&result.scopes, result.file_scope, &Name::from("List"));
        assert_eq!(
            res,
            Some(Resolution::Type(TypeName::from("java.util.List")))
        );
    }

    #[test]
    fn java_lang_is_implicit() {
        let jdk = JdkIndex::new();
        let unit = CompilationUnit::new(None);
        let result = build_scopes(&jdk, &unit);

        let resolver = Resolver::new(&jdk);
        let res = resolver.resolve_name(&result.scopes, result.file_scope, &Name::from("String"));
        assert_eq!(
            res,
            Some(Resolution::Type(TypeName::from("java.lang.String")))
        );
    }

    #[test]
    fn static_import_resolves_member() {
        let jdk = JdkIndex::new();
        let mut unit = CompilationUnit::new(None);
        unit.imports.push(ImportDecl::StaticSingle {
            ty: QualifiedName::from_dotted("java.lang.Math"),
            member: Name::from("max"),
            alias: None,
        });

        let result = build_scopes(&jdk, &unit);
        let resolver = Resolver::new(&jdk);
        let res = resolver.resolve_name(&result.scopes, result.file_scope, &Name::from("max"));
        assert_eq!(
            res,
            Some(Resolution::StaticMember(StaticMemberId::new(
                "java.lang.Math::max"
            )))
        );
    }

    #[test]
    fn static_star_import_resolves_field() {
        let jdk = JdkIndex::new();
        let mut unit = CompilationUnit::new(None);
        unit.imports.push(ImportDecl::StaticStar {
            ty: QualifiedName::from_dotted("java.lang.Math"),
        });

        let result = build_scopes(&jdk, &unit);
        let resolver = Resolver::new(&jdk);
        let res = resolver.resolve_name(&result.scopes, result.file_scope, &Name::from("PI"));
        assert_eq!(
            res,
            Some(Resolution::StaticMember(StaticMemberId::new(
                "java.lang.Math::PI"
            )))
        );
    }

    #[test]
    fn resolve_import_api_includes_java_lang_fallback() {
        let jdk = JdkIndex::new();
        let resolver = Resolver::new(&jdk);
        let unit = CompilationUnit::new(None);
        assert_eq!(
            resolver.resolve_import(&unit, &Name::from("String")),
            Some(TypeName::from("java.lang.String"))
        );
    }

    #[test]
    fn method_param_shadows_field() {
        let jdk = JdkIndex::new();

        let mut ty = TypeDecl::new("C");
        ty.fields.push(FieldDecl::new("x"));
        let mut method = MethodDecl::new("m");
        method.params.push(ParamDecl::new("x"));
        ty.methods.push(method);

        let unit = CompilationUnit {
            package: None,
            imports: Vec::new(),
            types: vec![ty],
        };
        let result = build_scopes(&jdk, &unit);

        let resolver = Resolver::new(&jdk);
        let method_scope = *result.method_scopes.values().next().expect("method scope");
        let res = resolver.resolve_name(&result.scopes, method_scope, &Name::from("x"));
        assert_eq!(res, Some(Resolution::Parameter));
    }

    #[test]
    fn resolves_imported_type_from_dependency_jar() {
        let jdk = JdkIndex::new();
        let classpath =
            ClasspathIndex::build(&[ClasspathEntry::Jar(test_dep_jar())], None).unwrap();
        let resolver = Resolver::new(&jdk).with_classpath(&classpath);

        let mut unit = CompilationUnit::new(None);
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("com.example.dep.Foo"),
            alias: None,
        });

        assert_eq!(
            resolver.resolve_import(&unit, &Name::from("Foo")),
            Some(TypeName::from("com.example.dep.Foo"))
        );
    }

    #[test]
    fn nested_type_resolves_in_class_scope() {
        let jdk = JdkIndex::new();

        let mut outer = TypeDecl::new("C");
        outer.nested_types.push(TypeDecl::new("Inner"));
        outer.methods.push(MethodDecl::new("m"));

        let unit = CompilationUnit {
            package: None,
            imports: Vec::new(),
            types: vec![outer],
        };
        let result = build_scopes(&jdk, &unit);

        let resolver = Resolver::new(&jdk);
        let method_scope = *result.method_scopes.get("C#m").expect("method scope");
        let res = resolver.resolve_name(&result.scopes, method_scope, &Name::from("Inner"));
        assert_eq!(res, Some(Resolution::Type(TypeName::from("C$Inner"))));
    }

    #[test]
    fn java_lang_lookup_is_not_limited_to_hardcoded_universe_types() {
        let mut index = TestIndex::default();
        let foo = index.add_type("java.lang", "Foo");

        let unit = CompilationUnit::new(None);
        let result = build_scopes(&index, &unit);

        let resolver = Resolver::new(&index);
        let res = resolver.resolve_name(&result.scopes, result.file_scope, &Name::from("Foo"));
        assert_eq!(res, Some(Resolution::Type(foo)));
    }

    #[test]
    fn qualified_name_resolves_nested_types() {
        let jdk = JdkIndex::new();
        let mut index = TestIndex::default();
        let entry = index.add_type("java.util", "Map$Entry");

        let resolver = Resolver::new(&jdk).with_classpath(&index);
        let resolved =
            resolver.resolve_qualified_name(&QualifiedName::from_dotted("java.util.Map.Entry"));
        assert_eq!(resolved, Some(entry));

        let mut unit = CompilationUnit::new(None);
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("java.util.Map.Entry"),
            alias: None,
        });
        assert_eq!(
            resolver.resolve_import(&unit, &Name::from("Entry")),
            Some(TypeName::from("java.util.Map$Entry"))
        );
    }

    #[test]
    fn qualified_type_resolves_via_imported_outer() {
        let jdk = JdkIndex::new();
        let mut index = TestIndex::default();
        index.add_type("java.util", "Map");
        let entry = index.add_type("java.util", "Map$Entry");

        let mut unit = CompilationUnit::new(None);
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("java.util.Map"),
            alias: None,
        });

        let result = build_scopes(&jdk, &unit);
        let resolver = Resolver::new(&jdk).with_classpath(&index);

        let resolved = resolver.resolve_qualified_type_in_scope(
            &result.scopes,
            result.file_scope,
            &QualifiedName::from_dotted("Map.Entry"),
        );
        assert_eq!(resolved, Some(entry));
    }
}
