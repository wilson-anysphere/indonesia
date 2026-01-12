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
use nova_modules::{ModuleGraph, ModuleInfo, ModuleName};
use thiserror::Error;
use walkdir::WalkDir;

use crate::{Resolution, Resolver, ScopeGraph, ScopeId};

/// A JPMS-aware wrapper around [`crate::Resolver`].
///
/// When resolving a type that is known to come from a named module on the
/// classpath, the resolver enforces:
/// - module readability (`requires` / `requires transitive`)
/// - package exports (`exports` / qualified exports)
///
/// If either side is considered "unnamed" (missing module metadata), the type
/// is treated as belonging to the classpath "unnamed module".
pub struct JpmsResolver<'a> {
    jdk: &'a nova_jdk::JdkIndex,
    graph: &'a ModuleGraph,
    classpath: &'a ModuleAwareClasspathIndex,
    from: ModuleName,
}

struct JpmsTypeIndex<'a> {
    jdk: &'a nova_jdk::JdkIndex,
    graph: &'a ModuleGraph,
    classpath: &'a ModuleAwareClasspathIndex,
    from: &'a ModuleName,
}

impl<'a> JpmsTypeIndex<'a> {
    fn module_of_type(&self, ty: &TypeName) -> Option<ModuleName> {
        if let Some(to) = self.classpath.module_of(ty.as_str()) {
            return Some(to.clone());
        }

        // If the type exists in the classpath index but has no module metadata,
        // treat it as belonging to the classpath "unnamed module".
        if self.classpath.types.lookup_binary(ty.as_str()).is_some() {
            return Some(ModuleName::unnamed());
        }

        self.jdk.module_of_type(ty.as_str())
    }

    fn type_is_accessible(&self, ty: &TypeName) -> bool {
        let Some(to) = self.module_of_type(ty) else {
            return true;
        };

        if !self.graph.can_read(self.from, &to) {
            return false;
        }

        let package = ty
            .as_str()
            .rsplit_once('.')
            .map(|(pkg, _)| pkg)
            .unwrap_or("");

        let Some(info) = self.graph.get(&to) else {
            return true;
        };

        info.exports_package_to(package, self.from)
    }

    fn package_is_accessible(&self, package: &str, to: &ModuleName) -> bool {
        if !self.graph.can_read(self.from, to) {
            return false;
        }

        let Some(info) = self.graph.get(to) else {
            return true;
        };

        info.exports_package_to(package, self.from)
    }
}

impl TypeIndex for JpmsTypeIndex<'_> {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        if let Some(ty) = self.classpath.resolve_type(name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        let ty = self.jdk.resolve_type(name)?;
        self.type_is_accessible(&ty).then_some(ty)
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        if let Some(ty) = self.classpath.resolve_type_in_package(package, name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        let ty = self.jdk.resolve_type_in_package(package, name)?;
        self.type_is_accessible(&ty).then_some(ty)
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        let pkg = package.to_dotted();

        // --- Classpath/module-path packages ---------------------------------
        if self.classpath.package_exists(package) {
            let prefix = if pkg.is_empty() {
                String::new()
            } else {
                format!("{pkg}.")
            };

            let names = self.classpath.types.binary_names_sorted();
            let start = names.partition_point(|name| name.as_str() < prefix.as_str());
            for binary_name in &names[start..] {
                if !binary_name.starts_with(prefix.as_str()) {
                    break;
                }
                let Some((found_pkg, _)) = binary_name.rsplit_once('.') else {
                    continue;
                };
                if found_pkg != pkg {
                    continue;
                }

                let to = self
                    .classpath
                    .module_of(binary_name)
                    .cloned()
                    .unwrap_or_else(ModuleName::unnamed);
                if self.package_is_accessible(&pkg, &to) {
                    return true;
                }
            }
        }

        // --- JDK packages ---------------------------------------------------
        if self.jdk.package_exists(package) {
            let prefix = if pkg.is_empty() {
                String::new()
            } else {
                format!("{pkg}.")
            };

            let binary_names = match self.jdk.all_binary_class_names() {
                Ok(names) => names,
                // Best-effort fallback: if we cannot inspect the package contents
                // (e.g. due to an indexing error), preserve the old behavior.
                Err(_) => return true,
            };

            let start = binary_names.partition_point(|name| name.as_str() < prefix.as_str());
            for binary_name in &binary_names[start..] {
                if !binary_name.starts_with(prefix.as_str()) {
                    break;
                }
                let Some((found_pkg, _)) = binary_name.rsplit_once('.') else {
                    continue;
                };
                if found_pkg != pkg {
                    continue;
                }

                let Some(to) = self.jdk.module_of_type(binary_name) else {
                    // Without module metadata, we cannot enforce exports. Mirror
                    // `type_is_accessible` and treat the package as visible.
                    return true;
                };

                if self.package_is_accessible(&pkg, &to) {
                    return true;
                }
            }
        }

        false
    }

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        // Static member imports require the owning type to be accessible.
        if !self.type_is_accessible(owner) {
            return None;
        }

        self.classpath
            .resolve_static_member(owner, name)
            .or_else(|| self.jdk.resolve_static_member(owner, name))
    }
}

impl<'a> JpmsResolver<'a> {
    pub fn new(
        jdk: &'a nova_jdk::JdkIndex,
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

    pub fn resolve_qualified_name(&self, path: &QualifiedName) -> Option<TypeName> {
        let index = JpmsTypeIndex {
            jdk: self.jdk,
            graph: self.graph,
            classpath: self.classpath,
            from: &self.from,
        };
        Resolver::new(&index).resolve_qualified_name(path)
    }

    pub fn resolve_qualified_type_in_scope(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        path: &QualifiedName,
    ) -> Option<TypeName> {
        let index = JpmsTypeIndex {
            jdk: self.jdk,
            graph: self.graph,
            classpath: self.classpath,
            from: &self.from,
        };
        Resolver::new(&index).resolve_qualified_type_in_scope(scopes, scope, path)
    }

    pub fn resolve_name(
        &self,
        scopes: &ScopeGraph,
        scope: ScopeId,
        name: &Name,
    ) -> Option<Resolution> {
        let index = JpmsTypeIndex {
            jdk: self.jdk,
            graph: self.graph,
            classpath: self.classpath,
            from: &self.from,
        };
        Resolver::new(&index).resolve_name(scopes, scope, name)
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
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use nova_classpath::ClasspathClassStub;
    use nova_core::FileId;
    use nova_hir::queries::HirDatabase;
    use nova_jdk::JdkIndex;
    use nova_modules::ModuleKind;

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

    fn main_unit_src() -> &'static str {
        r#"
package com.example.a;
import com.example.b.hidden.Hidden;
class C {}
"#
    }

    #[derive(Default)]
    struct TestDb {
        files: HashMap<FileId, Arc<str>>,
    }

    impl TestDb {
        fn set_file_text(&mut self, file: FileId, text: impl Into<Arc<str>>) {
            self.files.insert(file, text.into());
        }
    }

    impl HirDatabase for TestDb {
        fn file_text(&self, file: FileId) -> Arc<str> {
            self.files
                .get(&file)
                .cloned()
                .unwrap_or_else(|| Arc::from(""))
        }
    }

    #[test]
    fn exported_package_is_resolvable() {
        let ws = Workspace::load_from_dir(&workspace_path("exports")).unwrap();
        let from = ws.module("mod.a").unwrap().clone();
        let classpath = module_aware_index_from_workspace(&ws);
        let jdk = JdkIndex::new();

        let resolver = JpmsResolver::new(&jdk, &ws.graph, &classpath, from);

        let file = FileId::from_raw(0);
        let mut db = TestDb::default();
        db.set_file_text(file, main_unit_src());
        let scopes = build_scopes(&db, file);
        let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Hidden"));

        assert_eq!(
            res,
            Some(Resolution::Type(crate::TypeResolution::External(
                TypeName::from("com.example.b.hidden.Hidden")
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

        let file = FileId::from_raw(0);
        let mut db = TestDb::default();
        db.set_file_text(file, main_unit_src());
        let scopes = build_scopes(&db, file);
        let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Hidden"));

        assert_eq!(res, None);
    }

    #[test]
    fn classpath_types_are_only_accessible_from_unnamed_module() {
        let mut graph = ModuleGraph::new();
        graph.insert(ModuleInfo {
            name: ModuleName::new("mod.a"),
            kind: ModuleKind::Explicit,
            is_open: false,
            requires: Vec::new(),
            exports: Vec::new(),
            opens: Vec::new(),
            uses: Vec::new(),
            provides: Vec::new(),
        });

        let stub = ClasspathClassStub {
            binary_name: "com.example.Unnamed".to_string(),
            internal_name: "com/example/Unnamed".to_string(),
            access_flags: 0,
            super_binary_name: None,
            interfaces: Vec::new(),
            signature: None,
            annotations: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
        };
        let classpath = ModuleAwareClasspathIndex::from_stubs([(stub, None)]);
        let jdk = JdkIndex::new();
        let ty = QualifiedName::from_dotted("com.example.Unnamed");

        let named = JpmsResolver::new(&jdk, &graph, &classpath, ModuleName::new("mod.a"));
        assert_eq!(named.resolve_qualified_name(&ty), None);

        let unnamed = JpmsResolver::new(&jdk, &graph, &classpath, ModuleName::unnamed());
        assert_eq!(
            unnamed.resolve_qualified_name(&ty),
            Some(TypeName::from("com.example.Unnamed"))
        );
    }

    #[test]
    fn unreadable_module_is_not_resolvable() {
        let ws = Workspace::load_from_dir(&workspace_path("no_requires")).unwrap();
        let from = ws.module("mod.a").unwrap().clone();
        let classpath = module_aware_index_from_workspace(&ws);
        let jdk = JdkIndex::new();

        let resolver = JpmsResolver::new(&jdk, &ws.graph, &classpath, from);

        let file = FileId::from_raw(0);
        let mut db = TestDb::default();
        db.set_file_text(file, main_unit_src());
        let scopes = build_scopes(&db, file);
        let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Hidden"));

        assert_eq!(res, None);
    }
}
