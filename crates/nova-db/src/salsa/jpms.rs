use nova_core::{Name, PackageName, QualifiedName};
use nova_modules::{ModuleGraph, ModuleName};
use nova_project::ProjectConfig;
use nova_types::{TypeDefStub, TypeProvider};

/// Returns true when the project should use JPMS-aware name/type resolution.
///
/// This intentionally matches the heuristic used by `nova_resolve` queries so
/// name resolution and type checking stay in sync.
pub(crate) fn jpms_enabled(cfg: &ProjectConfig) -> bool {
    !cfg.jpms_modules.is_empty() || cfg.jpms_workspace.is_some() || !cfg.module_path.is_empty()
}

/// Determine the JPMS module that "owns" `rel_path`.
///
/// The algorithm chooses the deepest matching `jpms_modules` root (so nested
/// modules win over parent modules). When no workspace JPMS modules are
/// configured, this returns the sentinel unnamed module.
pub(crate) fn module_for_file(cfg: &ProjectConfig, rel_path: &str) -> ModuleName {
    if cfg.jpms_modules.is_empty() {
        return ModuleName::unnamed();
    }

    let file_path = cfg.workspace_root.join(rel_path);
    let mut best: Option<(usize, ModuleName)> = None;
    for root in &cfg.jpms_modules {
        if !file_path.starts_with(&root.root) {
            continue;
        }
        let depth = root.root.components().count();
        let replace = match &best {
            Some((best_depth, _)) => depth > *best_depth,
            None => true,
        };
        if replace {
            best = Some((depth, root.name.clone()));
        }
    }

    best.map(|(_, name)| name)
        .unwrap_or_else(ModuleName::unnamed)
}

/// JPMS-aware `TypeIndex` implementation used by name resolution and type
/// checking.
///
/// This wrapper enforces JPMS module readability + package exports when
/// resolving types and static members.
pub(crate) struct JpmsProjectIndex<'a> {
    pub(crate) workspace: &'a nova_resolve::WorkspaceDefMap,
    pub(crate) graph: &'a ModuleGraph,
    pub(crate) classpath: &'a nova_classpath::ModuleAwareClasspathIndex,
    pub(crate) jdk: &'a nova_jdk::JdkIndex,
    pub(crate) from: ModuleName,
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

fn is_java_binary_name(binary_name: &str) -> bool {
    binary_name
        .split('.')
        .next()
        .is_some_and(|seg| seg == "java")
}

impl<'a> JpmsProjectIndex<'a> {
    fn module_of_type(&self, ty: &nova_core::TypeName) -> Option<ModuleName> {
        // Mirror `nova_resolve::Resolver`'s `java.*` behavior: application class loaders cannot
        // define `java.*` packages, so `java.*` names should resolve exclusively through the JDK.
        if is_java_binary_name(ty.as_str()) {
            return self.jdk.module_of_type(ty.as_str());
        }

        if let Some(item) = self.workspace.item_by_type_name(ty) {
            if let Some(module) = self.workspace.module_for_item(item) {
                return Some(module.clone());
            }
            return Some(ModuleName::unnamed());
        }

        if let Some(to) = self.classpath.module_of(ty.as_str()) {
            return Some(to.clone());
        }

        if self.classpath.types.lookup_binary(ty.as_str()).is_some() {
            return Some(ModuleName::unnamed());
        }

        self.jdk.module_of_type(ty.as_str())
    }

    fn package_is_accessible(&self, package: &str, to: &ModuleName) -> bool {
        if !self.graph.can_read(&self.from, to) {
            return false;
        }

        let Some(info) = self.graph.get(to) else {
            // Unknown modules default to accessible so partial graphs don't
            // cascade into false-negative resolution failures.
            return true;
        };

        info.exports_package_to(package, &self.from)
    }

    fn type_is_accessible(&self, ty: &nova_core::TypeName) -> bool {
        let Some(to) = self.module_of_type(ty) else {
            // If we cannot determine module membership, fall back to "accessible"
            // (best-effort).
            return true;
        };

        let package = ty
            .as_str()
            .rsplit_once('.')
            .map(|(pkg, _)| pkg)
            .unwrap_or("");
        self.package_is_accessible(package, &to)
    }
}

impl nova_core::TypeIndex for JpmsProjectIndex<'_> {
    fn resolve_type(&self, name: &QualifiedName) -> Option<nova_core::TypeName> {
        // Match `nova_resolve::Resolver` semantics for `java.*`: ignore workspace and external
        // indices so "fake" `java.*` types on the module-path/classpath cannot affect resolution.
        if is_java_qualified_name(name) {
            let ty = self.jdk.resolve_type(name)?;
            return self.type_is_accessible(&ty).then_some(ty);
        }

        if let Some(ty) = self.workspace.resolve_type(name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        if let Some(ty) = self.classpath.resolve_type(name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

        let ty = self.jdk.resolve_type(name)?;
        self.type_is_accessible(&ty).then_some(ty)
    }

    fn resolve_type_in_package(
        &self,
        package: &PackageName,
        name: &Name,
    ) -> Option<nova_core::TypeName> {
        if is_java_package(package) {
            let ty = self.jdk.resolve_type_in_package(package, name)?;
            return self.type_is_accessible(&ty).then_some(ty);
        }

        if let Some(ty) = self.workspace.resolve_type_in_package(package, name) {
            if self.type_is_accessible(&ty) {
                return Some(ty);
            }
        }

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

        if !is_java_package(package) {
            // --- Workspace packages ---------------------------------------------
            for to in self.workspace.modules_defining_package(package) {
                if self.package_is_accessible(&pkg, &to) {
                    return true;
                }
            }

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

    fn resolve_static_member(
        &self,
        owner: &nova_core::TypeName,
        name: &Name,
    ) -> Option<nova_core::StaticMemberId> {
        if !self.type_is_accessible(owner) {
            return None;
        }

        if is_java_binary_name(owner.as_str()) {
            return self.jdk.resolve_static_member(owner, name);
        }

        self.workspace
            .resolve_static_member(owner, name)
            .or_else(|| self.classpath.resolve_static_member(owner, name))
            .or_else(|| self.jdk.resolve_static_member(owner, name))
    }
}

/// JPMS-aware `TypeProvider` wrapper used for external stub loading during type
/// checking.
///
/// This enforces the same readability/exports rules as [`JpmsProjectIndex`],
/// but for `TypeProvider::lookup_type` (class stub loading) instead of name
/// resolution.
pub(crate) struct JpmsTypeProvider<'a> {
    pub(crate) graph: &'a ModuleGraph,
    pub(crate) classpath: &'a nova_classpath::ModuleAwareClasspathIndex,
    pub(crate) jdk: &'a nova_jdk::JdkIndex,
    pub(crate) from: ModuleName,
}

impl JpmsTypeProvider<'_> {
    fn is_accessible_in_module(&self, binary_name: &str, to: Option<ModuleName>) -> bool {
        let Some(to) = to else {
            // If the module membership is unknown (builtin JDK stubs, partial
            // graphs), treat as accessible.
            return true;
        };

        if !self.graph.can_read(&self.from, &to) {
            return false;
        }

        let package = binary_name
            .rsplit_once('.')
            .map(|(pkg, _)| pkg)
            .unwrap_or("");

        let Some(info) = self.graph.get(&to) else {
            return true;
        };

        info.exports_package_to(package, &self.from)
    }

    fn module_of_classpath_type(&self, binary_name: &str) -> Option<ModuleName> {
        if let Some(module) = self.classpath.module_of(binary_name) {
            return Some(module.clone());
        }
        self.classpath
            .types
            .lookup_binary(binary_name)
            .is_some()
            .then(ModuleName::unnamed)
    }
}

impl TypeProvider for JpmsTypeProvider<'_> {
    fn lookup_type(&self, binary_name: &str) -> Option<TypeDefStub> {
        // Mirror `nova_resolve::Resolver`'s special-casing for `java.*`: even if a classpath /
        // module-path dependency contains a `java.*` class, the JDK should win (user class loaders
        // cannot define `java.*`).
        //
        // Note: we still apply JPMS readability/exports checks for the JDK type (e.g. `java.sql.*`
        // should not be accessible from a module that doesn't read `java.sql`).
        if binary_name.starts_with("java.") {
            if let Some(stub) =
                <nova_jdk::JdkIndex as TypeProvider>::lookup_type(self.jdk, binary_name)
            {
                let to = self.jdk.module_of_type(binary_name);
                if self.is_accessible_in_module(binary_name, to) {
                    return Some(stub);
                }
            }
            return None;
        }

        // 1) Module-path + classpath dependencies.
        if let Some(stub) = self.classpath.lookup_type(binary_name) {
            let to = self.module_of_classpath_type(binary_name);
            if self.is_accessible_in_module(binary_name, to) {
                return Some(stub);
            }
        }

        // 2) JDK stubs.
        if let Some(stub) = <nova_jdk::JdkIndex as TypeProvider>::lookup_type(self.jdk, binary_name)
        {
            let to = self.jdk.module_of_type(binary_name);
            if self.is_accessible_in_module(binary_name, to) {
                return Some(stub);
            }
        }

        None
    }
}
