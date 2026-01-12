//! Java Platform Module System (JPMS) model.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;

pub const JAVA_BASE: &str = "java.base";
/// Sentinel module name used to model the classpath "unnamed module".
///
/// This string is intentionally not a valid Java module name.
pub const UNNAMED_MODULE: &str = "<unnamed>";

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleName(String);

impl ModuleName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn unnamed() -> Self {
        Self::new(UNNAMED_MODULE)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_java_base(&self) -> bool {
        self.0 == JAVA_BASE
    }

    pub fn is_unnamed(&self) -> bool {
        self.0 == UNNAMED_MODULE
    }
}

impl fmt::Display for ModuleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    /// A module with an explicit `module-info.{java,class}` descriptor.
    Explicit,
    /// A named module synthesized from a JAR on the module path without
    /// `module-info.class` (or with `Automatic-Module-Name`).
    Automatic,
    /// The classpath "unnamed module".
    Unnamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    pub kind: ModuleKind,
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
        match self.kind {
            // Automatic modules export (and open) all packages to everyone.
            ModuleKind::Automatic | ModuleKind::Unnamed => true,
            ModuleKind::Explicit => {
                if &self.name == to {
                    return true;
                }

                self.exports.iter().any(|exports| {
                    exports.package == package
                        && (exports.to.is_empty() || exports.to.iter().any(|m| m == to))
                })
            }
        }
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

/// Workspace-level representation of modules.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModuleGraph {
    modules: HashMap<ModuleName, ModuleInfo>,
}

impl ModuleGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Approximate heap memory usage of this graph in bytes.
    ///
    /// This is intended for best-effort integration with `nova-memory`.
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        fn add_string(bytes: &mut u64, s: &String) {
            *bytes = bytes.saturating_add(s.capacity() as u64);
        }

        fn add_module_name(bytes: &mut u64, m: &ModuleName) {
            *bytes = bytes.saturating_add(m.0.capacity() as u64);
        }

        fn add_module_name_vec(bytes: &mut u64, v: &Vec<ModuleName>) {
            *bytes = bytes.saturating_add((v.capacity() * size_of::<ModuleName>()) as u64);
            for m in v {
                add_module_name(bytes, m);
            }
        }

        let mut bytes = 0u64;

        bytes = bytes.saturating_add(
            (self.modules.capacity() * size_of::<(ModuleName, ModuleInfo)>()) as u64,
        );

        for (name, info) in &self.modules {
            // Key module name.
            add_module_name(&mut bytes, name);

            // Value module info.
            add_module_name(&mut bytes, &info.name);

            bytes = bytes.saturating_add((info.requires.capacity() * size_of::<Requires>()) as u64);
            for req in &info.requires {
                add_module_name(&mut bytes, &req.module);
            }

            bytes = bytes.saturating_add((info.exports.capacity() * size_of::<Exports>()) as u64);
            for exports in &info.exports {
                add_string(&mut bytes, &exports.package);
                add_module_name_vec(&mut bytes, &exports.to);
            }

            bytes = bytes.saturating_add((info.opens.capacity() * size_of::<Opens>()) as u64);
            for opens in &info.opens {
                add_string(&mut bytes, &opens.package);
                add_module_name_vec(&mut bytes, &opens.to);
            }

            bytes = bytes.saturating_add((info.uses.capacity() * size_of::<Uses>()) as u64);
            for uses in &info.uses {
                add_string(&mut bytes, &uses.service);
            }

            bytes = bytes.saturating_add((info.provides.capacity() * size_of::<Provides>()) as u64);
            for provides in &info.provides {
                add_string(&mut bytes, &provides.service);

                bytes = bytes.saturating_add(
                    (provides.implementations.capacity() * size_of::<String>()) as u64,
                );
                for implementation in &provides.implementations {
                    add_string(&mut bytes, implementation);
                }
            }
        }

        bytes
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

        // The unnamed module (classpath) and automatic modules read every *named*
        // module.
        //
        // We also treat automatic modules as a best-effort stand-in for
        // `requires transitive *`, so any module that can read an automatic
        // module ends up reading every named module as well.
        //
        // Note: unlike automatic modules, readability does *not* propagate
        // through the unnamed module. This matches `--add-reads <module>=ALL-UNNAMED`
        // semantics used by build tools: named modules may read the classpath, but
        // do not automatically gain readability to all named modules.
        if from.is_unnamed() {
            self.add_all_named_modules(&mut out);
            return out;
        }
        if let Some(info) = self.get(from) {
            if matches!(info.kind, ModuleKind::Unnamed | ModuleKind::Automatic) {
                self.add_all_named_modules(&mut out);
                return out;
            }
        }

        let mut queue = VecDeque::new();
        queue.push_back(from.clone());

        while let Some(current) = queue.pop_front() {
            let Some(info) = self.get(&current) else {
                continue;
            };

            if matches!(info.kind, ModuleKind::Automatic) {
                self.add_all_named_modules(&mut out);
                break;
            }

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

    fn add_all_named_modules(&self, out: &mut BTreeSet<ModuleName>) {
        for (name, info) in &self.modules {
            if info.kind != ModuleKind::Unnamed {
                out.insert(name.clone());
            }
        }
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

    fn module(
        kind: super::ModuleKind,
        name: &str,
        requires: Vec<super::Requires>,
    ) -> super::ModuleInfo {
        super::ModuleInfo {
            kind,
            name: super::ModuleName::new(name),
            is_open: false,
            requires,
            exports: Vec::new(),
            opens: Vec::new(),
            uses: Vec::new(),
            provides: Vec::new(),
        }
    }

    #[test]
    fn unnamed_reads_all_named_modules() {
        let mut graph = super::ModuleGraph::new();
        graph.insert(module(super::ModuleKind::Explicit, "a", Vec::new()));
        graph.insert(module(super::ModuleKind::Explicit, "b", Vec::new()));
        graph.insert(module(super::ModuleKind::Automatic, "auto", Vec::new()));
        graph.insert(module(
            super::ModuleKind::Unnamed,
            super::UNNAMED_MODULE,
            Vec::new(),
        ));

        let unnamed = super::ModuleName::unnamed();
        for named in ["a", "b", "auto"] {
            assert!(
                graph.can_read(&unnamed, &super::ModuleName::new(named)),
                "unnamed should read {named}"
            );
        }
    }

    #[test]
    fn automatic_reads_all_named_modules() {
        let mut graph = super::ModuleGraph::new();
        graph.insert(module(super::ModuleKind::Explicit, "a", Vec::new()));
        graph.insert(module(super::ModuleKind::Explicit, "b", Vec::new()));
        graph.insert(module(super::ModuleKind::Automatic, "auto", Vec::new()));
        graph.insert(module(
            super::ModuleKind::Unnamed,
            super::UNNAMED_MODULE,
            Vec::new(),
        ));

        let auto = super::ModuleName::new("auto");
        for named in ["a", "b", "auto"] {
            assert!(
                graph.can_read(&auto, &super::ModuleName::new(named)),
                "automatic should read {named}"
            );
        }

        assert!(
            !graph.can_read(&auto, &super::ModuleName::unnamed()),
            "named modules should not read the unnamed module"
        );
    }

    #[test]
    fn requires_unnamed_does_not_make_all_named_modules_readable() {
        let mut graph = super::ModuleGraph::new();
        graph.insert(module(
            super::ModuleKind::Explicit,
            "a",
            vec![super::Requires {
                module: super::ModuleName::unnamed(),
                is_transitive: false,
                is_static: false,
            }],
        ));
        graph.insert(module(super::ModuleKind::Explicit, "b", Vec::new()));
        graph.insert(module(super::ModuleKind::Automatic, "auto", Vec::new()));
        graph.insert(module(
            super::ModuleKind::Unnamed,
            super::UNNAMED_MODULE,
            Vec::new(),
        ));

        let a = super::ModuleName::new("a");
        let unnamed = super::ModuleName::unnamed();
        assert!(
            graph.can_read(&a, &unnamed),
            "module `a` should read unnamed when it explicitly requires it"
        );

        // Even though the unnamed module reads all named modules, that readability does not
        // propagate through to modules that require it (matches `--add-reads <module>=ALL-UNNAMED`).
        assert!(
            !graph.can_read(&a, &super::ModuleName::new("b")),
            "requiring unnamed should not implicitly make other named modules readable"
        );
        assert!(
            !graph.can_read(&a, &super::ModuleName::new("auto")),
            "requiring unnamed should not implicitly make automatic modules readable"
        );
    }

    #[test]
    fn explicit_requires_controls_readability() {
        let mut graph = super::ModuleGraph::new();
        graph.insert(module(
            super::ModuleKind::Explicit,
            "a",
            vec![super::Requires {
                module: super::ModuleName::new("b"),
                is_transitive: false,
                is_static: false,
            }],
        ));
        graph.insert(module(
            super::ModuleKind::Explicit,
            "b",
            vec![super::Requires {
                module: super::ModuleName::new("c"),
                is_transitive: false,
                is_static: false,
            }],
        ));
        graph.insert(module(super::ModuleKind::Explicit, "c", Vec::new()));

        let a = super::ModuleName::new("a");
        assert!(graph.can_read(&a, &super::ModuleName::new("b")));
        assert!(
            !graph.can_read(&a, &super::ModuleName::new("c")),
            "non-transitive requires should not propagate"
        );

        // Mark b -> c as transitive and ensure it becomes readable.
        graph.insert(module(
            super::ModuleKind::Explicit,
            "b",
            vec![super::Requires {
                module: super::ModuleName::new("c"),
                is_transitive: true,
                is_static: false,
            }],
        ));
        assert!(
            graph.can_read(&a, &super::ModuleName::new("c")),
            "transitive requires should propagate"
        );
    }

    #[test]
    fn automatic_exports_are_unrestricted() {
        let auto = module(super::ModuleKind::Automatic, "auto", Vec::new());
        assert!(auto.exports_package_to("com.example", &super::ModuleName::new("someone")));
        assert!(auto.exports_package_to("com.example.internal", &super::ModuleName::new("someone")));
    }
}
