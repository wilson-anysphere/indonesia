//! Java Platform Module System (JPMS) model.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;

pub const JAVA_BASE: &str = "java.base";

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleName(String);

impl ModuleName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_java_base(&self) -> bool {
        self.0 == JAVA_BASE
    }
}

impl fmt::Display for ModuleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    pub name: ModuleName,
    pub is_open: bool,
    pub requires: Vec<Requires>,
    pub exports: Vec<Exports>,
    pub opens: Vec<Opens>,
    pub uses: Vec<Uses>,
    pub provides: Vec<Provides>,
}

impl ModuleInfo {
    pub fn exports_package_to(&self, package: &str, to: &ModuleName) -> bool {
        if &self.name == to {
            return true;
        }

        self.exports.iter().any(|exports| {
            exports.package == package
                && (exports.to.is_empty() || exports.to.iter().any(|m| m == to))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requires {
    pub module: ModuleName,
    pub is_transitive: bool,
    pub is_static: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exports {
    pub package: String,
    pub to: Vec<ModuleName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opens {
    pub package: String,
    pub to: Vec<ModuleName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uses {
    pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provides {
    pub service: String,
    pub implementations: Vec<String>,
}

/// Workspace-level representation of named modules.
#[derive(Debug, Default, Clone)]
pub struct ModuleGraph {
    modules: HashMap<ModuleName, ModuleInfo>,
}

impl ModuleGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, info: ModuleInfo) {
        self.modules.insert(info.name.clone(), info);
    }

    pub fn get(&self, name: &ModuleName) -> Option<&ModuleInfo> {
        self.modules.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ModuleName, &ModuleInfo)> {
        self.modules.iter()
    }

    /// Compute the set of modules readable by `from`.
    ///
    /// This is a best-effort approximation of JPMS readability:
    /// * A module can always read itself
    /// * `java.base` is implicitly readable
    /// * Direct `requires` edges are readable
    /// * Only `requires transitive` edges of dependencies are propagated
    pub fn readable_modules(&self, from: &ModuleName) -> BTreeSet<ModuleName> {
        let mut out = BTreeSet::new();
        out.insert(from.clone());
        out.insert(ModuleName::new(JAVA_BASE));

        let mut queue = VecDeque::new();
        queue.push_back(from.clone());

        while let Some(current) = queue.pop_front() {
            let Some(info) = self.get(&current) else {
                continue;
            };

            let follow_all = current == *from;
            for req in &info.requires {
                if !follow_all && !req.is_transitive {
                    continue;
                }
                let dep = req.module.clone();
                if out.insert(dep.clone()) {
                    queue.push_back(dep);
                }
            }
        }

        out
    }

    pub fn can_read(&self, from: &ModuleName, to: &ModuleName) -> bool {
        if from == to || to.is_java_base() {
            return true;
        }
        self.readable_modules(from).contains(to)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nova_resolve::jpms::{ResolveError, Workspace};

    fn workspace_path(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name)
    }

    #[test]
    fn resolution_fails_for_non_exported_package() {
        let ws = Workspace::load_from_dir(&workspace_path("no_exports")).unwrap();
        let from = ws.module("mod.a").unwrap();

        let err = ws
            .resolve_fqcn(from, "com.example.b.hidden.Hidden")
            .unwrap_err();

        assert!(matches!(err, ResolveError::NotExported { .. }), "{err:?}");
    }

    #[test]
    fn resolution_succeeds_for_exported_package() {
        let ws = Workspace::load_from_dir(&workspace_path("exports")).unwrap();
        let from = ws.module("mod.a").unwrap();

        ws.resolve_fqcn(from, "com.example.b.hidden.Hidden")
            .expect("type should be accessible when package is exported");
    }
}
