//! JPMS-aware resolution helpers.
//!
//! Nova's main resolver is built around [`nova_core::TypeIndex`]. That index does
//! not yet encode module membership for individual packages/types, so the
//! JPMS-aware checks implemented here are intentionally best-effort and geared
//! toward validating module visibility semantics using fixture workspaces.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use nova_classpath::ModuleAwareClasspathIndex;
use nova_core::{Name, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_hir::module_info::{lower_module_info_source_strict, ModuleInfoLowerError};
use nova_hir::{CompilationUnit, ImportDecl};
use nova_modules::{ModuleGraph, ModuleInfo, ModuleName};
use thiserror::Error;
use walkdir::WalkDir;

use crate::{Resolution, ScopeGraph, ScopeId, ScopeKind};
use crate::scopes::{append_package, resolve_type_with_nesting};

/// A JPMS-aware wrapper around [`crate::Resolver`].
///
/// When resolving a type that is known to come from a named module on the
/// classpath, the resolver enforces:
/// - module readability (`requires` / `requires transitive`)
/// - package exports (`exports` / qualified exports)
///
/// If either side is considered "unnamed" (missing module metadata), the type
/// is treated as accessible, matching traditional classpath semantics.
pub struct JpmsResolver<'a> {
    jdk: &'a dyn TypeIndex,
    graph: &'a ModuleGraph,
    classpath: &'a ModuleAwareClasspathIndex,
    from: ModuleName,
}

impl<'a> JpmsResolver<'a> {
    pub fn new(
        jdk: &'a dyn TypeIndex,
        graph: &'a ModuleGraph,
        classpath: &'a ModuleAwareClasspathIndex,
        from: ModuleName,
    ) -> Self {
        Self {
            jdk,
            graph,
            classpath,
            from,
        }
    }

    fn type_is_accessible(&self, ty: &TypeName) -> bool {
        let Some(to) = self.classpath.module_of(ty.as_str()) else {
            return true;
        };

        if !self.graph.can_read(&self.from, to) {
            return false;
        }

        let package = ty
            .as_str()
            .rsplit_once('.')
            .map(|(pkg, _)| pkg)
            .unwrap_or("");

        let Some(info) = self.graph.get(to) else {
            return true;
        };

        info.exports_package_to(package, &self.from)
    }

    fn resolve_type_in_index(&self, name: &QualifiedName) -> Option<TypeName> {
        if let Some(ty) = resolve_type_with_nesting(self.classpath, name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        let ty = resolve_type_with_nesting(self.jdk, name)?;
        self.type_is_accessible(&ty).then_some(ty)
    }

    fn resolve_type_in_package_index(
        &self,
        package: &PackageName,
        name: &Name,
    ) -> Option<TypeName> {
        if let Some(ty) = self.classpath.resolve_type_in_package(package, name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        let ty = self.jdk.resolve_type_in_package(package, name)?;
        self.type_is_accessible(&ty).then_some(ty)
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.classpath.package_exists(package) || self.jdk.package_exists(package)
    }

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        self.classpath
            .resolve_static_member(owner, name)
            .or_else(|| self.jdk.resolve_static_member(owner, name))
    }

    pub fn resolve_qualified_name(&self, path: &QualifiedName) -> Option<TypeName> {
        self.resolve_type_in_index(path)
    }

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

    pub fn resolve_name(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> Option<Resolution> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = scopes.scope(id);

            if let Some(value) = data.values().get(name) {
                return Some(value.clone());
            }

            if let Some(ty) = data.types().get(name) {
                // Types declared in the current file/module are always accessible.
                return Some(Resolution::Type(ty.clone()));
            }

            match data.kind() {
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

                    if package
                        .as_ref()
                        .is_some_and(|pkg| self.package_exists(&append_package(pkg, name)))
                    {
                        let pkg = append_package(package.as_ref().unwrap(), name).to_dotted();
                        return Some(Resolution::Package(nova_core::PackageId::new(pkg)));
                    }
                }
                ScopeKind::Universe => {
                    if let Some(ty) = self
                        .resolve_type_in_package_index(&PackageName::from_dotted("java.lang"), name)
                    {
                        return Some(Resolution::Type(ty));
                    }
                }
                _ => {}
            }

            current = data.parent();
        }
        None
    }

    fn resolve_import_types(&self, imports: &[ImportDecl], name: &Name) -> Option<TypeName> {
        for import in imports {
            if let ImportDecl::TypeSingle { ty, alias } = import {
                let import_name = alias.as_ref().or_else(|| ty.last());
                if import_name == Some(name) {
                    if let Some(ty) = self.resolve_type_in_index(ty) {
                        return Some(ty);
                    }
                }
            }
        }

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDef {
    pub module: ModuleName,
    pub package: String,
    pub name: String,
}

impl TypeDef {
    pub fn fqcn(&self) -> String {
        if self.package.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.package, self.name)
        }
    }
}

#[derive(Debug)]
pub struct Workspace {
    graph: ModuleGraph,
    module_roots: HashMap<ModuleName, PathBuf>,
    types: HashMap<String, TypeDef>,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("failed to read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Lower(#[from] ModuleInfoLowerError),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ResolveError {
    #[error("type `{fqcn}` not found")]
    NotFound { fqcn: String },
    #[error("module `{from}` does not read `{to}`")]
    NotReadable { from: ModuleName, to: ModuleName },
    #[error("module `{exporter}` does not export package `{package}` to `{to}`")]
    NotExported {
        exporter: ModuleName,
        package: String,
        to: ModuleName,
    },
}

impl Workspace {
    pub fn load_from_dir(root: &Path) -> Result<Self, WorkspaceError> {
        let mut graph = ModuleGraph::new();
        let mut module_roots = HashMap::new();

        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.file_name() != "module-info.java" {
                continue;
            }

            let path = entry.path().to_path_buf();
            let src = fs::read_to_string(&path).map_err(|source| WorkspaceError::ReadFile {
                path: path.clone(),
                source,
            })?;

            let info = lower_module_info_source_strict(&src)?;
            let module_root = path
                .parent()
                .ok_or_else(|| WorkspaceError::ReadFile {
                    path: path.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "module-info.java has no parent directory",
                    ),
                })?
                .to_path_buf();

            module_roots.insert(info.name.clone(), module_root);
            graph.insert(info);
        }

        let mut types = HashMap::new();
        for (module, module_root) in &module_roots {
            index_java_sources(module, module_root, &mut types)?;
        }

        Ok(Self {
            graph,
            module_roots,
            types,
        })
    }

    pub fn module(&self, name: &str) -> Option<&ModuleName> {
        self.graph
            .iter()
            .find(|(module, _)| module.as_str() == name)
            .map(|(module, _)| module)
    }

    pub fn module_info(&self, module: &ModuleName) -> Option<&ModuleInfo> {
        self.graph.get(module)
    }

    pub fn module_root(&self, module: &ModuleName) -> Option<&Path> {
        self.module_roots.get(module).map(PathBuf::as_path)
    }

    pub fn resolve_fqcn(&self, from: &ModuleName, fqcn: &str) -> Result<&TypeDef, ResolveError> {
        let Some(def) = self.types.get(fqcn) else {
            return Err(ResolveError::NotFound {
                fqcn: fqcn.to_string(),
            });
        };

        if &def.module == from {
            return Ok(def);
        }

        if !self.graph.can_read(from, &def.module) {
            return Err(ResolveError::NotReadable {
                from: from.clone(),
                to: def.module.clone(),
            });
        }

        let Some(exporter_info) = self.graph.get(&def.module) else {
            return Ok(def);
        };

        if !exporter_info.exports_package_to(&def.package, from) {
            return Err(ResolveError::NotExported {
                exporter: def.module.clone(),
                package: def.package.clone(),
                to: from.clone(),
            });
        }

        Ok(def)
    }
}

fn index_java_sources(
    module: &ModuleName,
    module_root: &Path,
    types: &mut HashMap<String, TypeDef>,
) -> Result<(), WorkspaceError> {
    for entry in WalkDir::new(module_root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name() == "module-info.java" {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }

        let path = entry.path().to_path_buf();
        let src = fs::read_to_string(&path).map_err(|source| WorkspaceError::ReadFile {
            path: path.clone(),
            source,
        })?;

        let package = parse_package(&src).unwrap_or_default();
        let Some(name) = parse_first_type_name(&src) else {
            continue;
        };

        let def = TypeDef {
            module: module.clone(),
            package,
            name,
        };
        types.insert(def.fqcn(), def);
    }

    Ok(())
}

fn parse_package(src: &str) -> Option<String> {
    for line in src.lines() {
        let line = line.trim();
        if line.starts_with("//") || line.starts_with("/*") || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("package ") {
            let name = rest.trim_end_matches(';').trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn parse_first_type_name(src: &str) -> Option<String> {
    for line in src.lines() {
        let line = line.trim();
        if line.starts_with("//") || line.starts_with("/*") || line.is_empty() {
            continue;
        }

        let tokens: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == '{' || c == '(')
            .filter(|t| !t.is_empty())
            .collect();

        for win in tokens.windows(2) {
            if let [kw, name] = win {
                if matches!(*kw, "class" | "interface" | "enum" | "record") {
                    return Some(name.trim_end_matches('{').to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nova_classpath::ClasspathClassStub;
    use nova_jdk::JdkIndex;

    use super::*;
    use crate::build_scopes;

    fn workspace_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-modules/testdata")
            .join(name)
    }

    fn module_aware_index_from_workspace(ws: &Workspace) -> ModuleAwareClasspathIndex {
        let stubs = ws.types.values().map(|def| {
            let binary_name = def.fqcn();
            let internal_name = binary_name.replace('.', "/");
            let stub = ClasspathClassStub {
                binary_name: binary_name.clone(),
                internal_name,
                access_flags: 0,
                super_binary_name: None,
                interfaces: Vec::new(),
                signature: None,
                annotations: Vec::new(),
                fields: Vec::new(),
                methods: Vec::new(),
            };
            (stub, Some(def.module.clone()))
        });

        ModuleAwareClasspathIndex::from_stubs(stubs)
    }

    fn main_unit() -> CompilationUnit {
        let mut unit = CompilationUnit::new(Some(PackageName::from_dotted("com.example.a")));
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("com.example.b.hidden.Hidden"),
            alias: None,
        });
        unit
    }

    #[test]
    fn exported_package_is_resolvable() {
        let ws = Workspace::load_from_dir(&workspace_path("exports")).unwrap();
        let from = ws.module("mod.a").unwrap().clone();
        let classpath = module_aware_index_from_workspace(&ws);
        let jdk = JdkIndex::new();

        let resolver = JpmsResolver::new(&jdk, &ws.graph, &classpath, from);

        let unit = main_unit();
        let scopes = build_scopes(&jdk, &unit);
        let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Hidden"));

        assert_eq!(
            res,
            Some(Resolution::Type(TypeName::from(
                "com.example.b.hidden.Hidden"
            )))
        );
    }

    #[test]
    fn unexported_package_is_not_resolvable() {
        let ws = Workspace::load_from_dir(&workspace_path("no_exports")).unwrap();
        let from = ws.module("mod.a").unwrap().clone();
        let classpath = module_aware_index_from_workspace(&ws);
        let jdk = JdkIndex::new();

        let resolver = JpmsResolver::new(&jdk, &ws.graph, &classpath, from);

        let unit = main_unit();
        let scopes = build_scopes(&jdk, &unit);
        let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Hidden"));

        assert_eq!(res, None);
    }

    #[test]
    fn unreadable_module_is_not_resolvable() {
        let ws = Workspace::load_from_dir(&workspace_path("no_requires")).unwrap();
        let from = ws.module("mod.a").unwrap().clone();
        let classpath = module_aware_index_from_workspace(&ws);
        let jdk = JdkIndex::new();

        let resolver = JpmsResolver::new(&jdk, &ws.graph, &classpath, from);

        let unit = main_unit();
        let scopes = build_scopes(&jdk, &unit);
        let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Hidden"));

        assert_eq!(res, None);
    }
}
