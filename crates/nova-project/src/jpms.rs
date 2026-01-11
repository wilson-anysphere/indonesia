use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nova_archive::Archive;
use nova_classfile::parse_module_info_class;
use nova_hir::module_info::lower_module_info_source;
use nova_modules::{ModuleGraph, ModuleInfo, ModuleKind, ModuleName};

use crate::{ClasspathEntry, JpmsModuleRoot, JpmsWorkspace, Module};

pub(crate) fn discover_jpms_modules(modules: &[Module]) -> Vec<JpmsModuleRoot> {
    let mut out = Vec::new();
    for module in modules {
        if let Some(root) = discover_jpms_module_root(&module.root) {
            out.push(root);
        }
    }

    out.sort_by(|a, b| {
        a.root
            .cmp(&b.root)
            .then(a.name.as_str().cmp(b.name.as_str()))
    });
    out.dedup_by(|a, b| {
        a.root == b.root && a.name == b.name && a.module_info == b.module_info && a.info == b.info
    });
    out
}

pub(crate) fn workspace_uses_jpms(jpms_modules: &[JpmsModuleRoot]) -> bool {
    !jpms_modules.is_empty()
}

/// Classify dependency entries into module-path vs classpath.
///
/// For now we treat "workspace has any `module-info.java`" as the signal that
/// the workspace is JPMS-enabled.
pub(crate) fn classify_dependency_entries(
    jpms_modules: &[JpmsModuleRoot],
    entries: Vec<ClasspathEntry>,
) -> (Vec<ClasspathEntry>, Vec<ClasspathEntry>) {
    if workspace_uses_jpms(jpms_modules) {
        (entries, Vec::new())
    } else {
        (Vec::new(), entries)
    }
}

/// Build a workspace-level JPMS module graph.
///
/// The resulting graph contains:
/// - JPMS modules discovered in the workspace (from `module-info.java`)
/// - Named or automatic modules discovered from module-path entries
pub(crate) fn build_jpms_workspace(
    jpms_modules: &[JpmsModuleRoot],
    module_path: &[ClasspathEntry],
) -> Option<JpmsWorkspace> {
    if !workspace_uses_jpms(jpms_modules) {
        return None;
    }

    let mut candidates: BTreeMap<ModuleName, ModuleCandidate> = BTreeMap::new();

    for root in jpms_modules {
        insert_candidate(
            &mut candidates,
            ModuleCandidate {
                info: root.info.clone(),
                root: root.root.clone(),
                kind: ModuleCandidateKind::Workspace,
            },
        );
    }

    for entry in module_path {
        let Some(candidate) = module_candidate_from_module_path_entry(&entry.path) else {
            continue;
        };
        insert_candidate(&mut candidates, candidate);
    }

    let mut graph = ModuleGraph::new();
    let mut module_roots = BTreeMap::new();
    for (name, candidate) in candidates {
        module_roots.insert(name.clone(), candidate.root);
        graph.insert(candidate.info);
    }

    Some(JpmsWorkspace {
        graph,
        module_roots,
    })
}

fn discover_jpms_module_root(module_root: &Path) -> Option<JpmsModuleRoot> {
    let candidates = [
        module_root.join("src/main/java/module-info.java"),
        module_root.join("src/module-info.java"),
        module_root.join("module-info.java"),
    ];

    let module_info_path = candidates.into_iter().find(|p| p.is_file())?;
    let src = std::fs::read_to_string(&module_info_path).ok()?;
    let info = lower_module_info_source(&src).info?;

    Some(JpmsModuleRoot {
        name: info.name.clone(),
        root: module_root.to_path_buf(),
        module_info: module_info_path,
        info,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ModuleCandidateKind {
    Workspace,
    Explicit,
    Manifest,
    Filename,
}

impl ModuleCandidateKind {
    fn rank(self) -> u8 {
        match self {
            ModuleCandidateKind::Workspace => 3,
            ModuleCandidateKind::Explicit => 2,
            ModuleCandidateKind::Manifest => 1,
            ModuleCandidateKind::Filename => 0,
        }
    }
}

#[derive(Debug, Clone)]
struct ModuleCandidate {
    info: ModuleInfo,
    root: PathBuf,
    kind: ModuleCandidateKind,
}

fn insert_candidate(
    target: &mut BTreeMap<ModuleName, ModuleCandidate>,
    candidate: ModuleCandidate,
) {
    let name = candidate.info.name.clone();
    match target.get(&name) {
        None => {
            target.insert(name, candidate);
        }
        Some(existing) => {
            let existing_rank = existing.kind.rank();
            let new_rank = candidate.kind.rank();
            let replace = if new_rank != existing_rank {
                new_rank > existing_rank
            } else {
                // Deterministic tie-breaker: prefer lexicographically smaller paths.
                candidate.root < existing.root
            };
            if replace {
                target.insert(name, candidate);
            }
        }
    }
}

fn module_candidate_from_module_path_entry(path: &Path) -> Option<ModuleCandidate> {
    let archive = Archive::new(path.to_path_buf());

    if let Some(bytes) = archive
        .read("module-info.class")
        .ok()
        .flatten()
        .or_else(|| archive.read("classes/module-info.class").ok().flatten())
    {
        if let Ok(info) = parse_module_info_class(&bytes) {
            return Some(ModuleCandidate {
                info,
                root: path.to_path_buf(),
                kind: ModuleCandidateKind::Explicit,
            });
        }
    }

    if let Some(bytes) = archive.read("META-INF/MANIFEST.MF").ok().flatten() {
        if let Some(name) = automatic_module_name_from_manifest(&bytes) {
            return Some(ModuleCandidate {
                info: empty_module_info(name),
                root: path.to_path_buf(),
                kind: ModuleCandidateKind::Manifest,
            });
        }
    }

    let name = derive_automatic_module_name(path);
    Some(ModuleCandidate {
        info: empty_module_info(name),
        root: path.to_path_buf(),
        kind: ModuleCandidateKind::Filename,
    })
}

fn empty_module_info(name: ModuleName) -> ModuleInfo {
    ModuleInfo {
        kind: ModuleKind::Automatic,
        name,
        is_open: false,
        requires: Vec::new(),
        exports: Vec::new(),
        opens: Vec::new(),
        uses: Vec::new(),
        provides: Vec::new(),
    }
}

fn automatic_module_name_from_manifest(bytes: &[u8]) -> Option<ModuleName> {
    let text = String::from_utf8_lossy(bytes);
    let mut current_key: Option<String> = None;
    let mut current_value = String::new();

    let flush = |key: Option<String>, value: &str| -> Option<ModuleName> {
        let Some(key) = key else {
            return None;
        };
        if !key.eq_ignore_ascii_case("automatic-module-name") {
            return None;
        }
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        Some(ModuleName::new(value.to_string()))
    };

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(found) = flush(current_key.take(), &current_value) {
                return Some(found);
            }
            current_value.clear();
            continue;
        }

        if let Some(rest) = line.strip_prefix(' ') {
            // Manifest continuation line.
            current_value.push_str(rest);
            continue;
        }

        if let Some(found) = flush(current_key.take(), &current_value) {
            return Some(found);
        }
        current_value.clear();

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        current_key = Some(key.to_string());
        current_value = value.trim_start().to_string();
    }

    flush(current_key.take(), &current_value)
}

fn derive_automatic_module_name(path: &Path) -> ModuleName {
    let name = path
        .file_stem()
        .or_else(|| path.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed");

    let mut base = name.to_string();
    if let Some(pos) = base
        .as_bytes()
        .windows(2)
        .position(|w| w[0] == b'-' && w[1].is_ascii_digit())
    {
        base.truncate(pos);
    }

    let mut normalized = String::with_capacity(base.len());
    let mut last_dot = false;
    for ch in base.chars() {
        let keep = ch.is_ascii_alphanumeric() || ch == '_' || ch == '$';
        let out = if keep { ch } else { '.' };
        if out == '.' {
            if last_dot {
                continue;
            }
            last_dot = true;
            normalized.push('.');
        } else {
            last_dot = false;
            normalized.push(out);
        }
    }
    let normalized = normalized.trim_matches('.').to_string();

    let mut parts = Vec::new();
    for part in normalized.split('.') {
        if part.is_empty() {
            continue;
        }
        let mut part = part.to_string();
        if part.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
            part.insert(0, '_');
        }
        parts.push(part);
    }

    let module = if parts.is_empty() {
        "unnamed".to_string()
    } else {
        parts.join(".")
    };

    ModuleName::new(module)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        load_project_with_options, BuildSystem, ClasspathEntryKind, JavaConfig, LoadOptions,
        ProjectConfig,
    };
    use nova_hir::module_info::lower_module_info_source_strict;
    use std::collections::BTreeSet;
    use tempfile::tempdir;

    #[test]
    fn lowers_module_info_java_directives_to_nova_modules() {
        let src = r#"
            module mod.a {
                requires transitive mod.b;
                exports com.example.a.api;
                exports com.example.a.internal to mod.b;
                opens com.example.a.reflect;
                uses com.example.Service;
                provides com.example.Service with com.example.ServiceImpl;
            }
        "#;

        let info = lower_module_info_source_strict(src).expect("lower module-info.java");

        assert_eq!(info.name.as_str(), "mod.a");
        assert_eq!(info.requires.len(), 1);
        assert_eq!(info.requires[0].module.as_str(), "mod.b");
        assert!(info.requires[0].is_transitive);

        assert!(info.exports_package_to("com.example.a.api", &ModuleName::new("mod.b")));
        assert!(!info.exports_package_to("com.example.a.internal", &ModuleName::new("mod.c")));
        assert!(info.exports_package_to("com.example.a.internal", &ModuleName::new("mod.b")));
    }

    #[test]
    fn module_graph_readable_modules_respects_transitive_requires() {
        let a = lower_module_info_source_strict("module a { requires transitive b; }").unwrap();
        let b = lower_module_info_source_strict("module b { requires transitive c; }").unwrap();
        let c = lower_module_info_source_strict("module c { }").unwrap();

        let mut graph = ModuleGraph::new();
        graph.insert(a);
        graph.insert(b);
        graph.insert(c);

        let readable = graph.readable_modules(&ModuleName::new("a"));
        let names: BTreeSet<_> = readable.iter().map(|m| m.as_str()).collect();
        assert!(names.contains("a"));
        assert!(names.contains("b"));
        assert!(
            names.contains("c"),
            "transitive dependency should be readable"
        );
    }

    #[test]
    fn reads_automatic_module_name_from_manifest() {
        let manifest = b"Manifest-Version: 1.0\r\nAutomatic-Module-Name: com.example.foo\r\n\r\n";
        let name = automatic_module_name_from_manifest(manifest).expect("name");
        assert_eq!(name.as_str(), "com.example.foo");
    }

    #[test]
    fn derives_automatic_module_name_from_filename() {
        assert_eq!(
            derive_automatic_module_name(Path::new("foo-bar-1.2.3.jar")).as_str(),
            "foo.bar"
        );
        assert_eq!(
            derive_automatic_module_name(Path::new("guava-33.0.0-jre.jar")).as_str(),
            "guava"
        );
    }

    #[test]
    fn end_to_end_workspace_module_reads_dependency_module() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();

        std::fs::write(
            root.join("module-info.java"),
            "module mod.a { requires mod.b; }",
        )
        .expect("write module-info.java");

        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).expect("mkdir src");
        std::fs::write(src_dir.join("Main.java"), "class Main {}").expect("write dummy java");

        let dep_dir = root.join("deps/mod-b");
        std::fs::create_dir_all(&dep_dir).expect("mkdir");
        std::fs::write(dep_dir.join("module-info.class"), make_module_info_class())
            .expect("write module-info.class");

        let mut options = LoadOptions::default();
        options.classpath_overrides.push(dep_dir.clone());
        let cfg = load_project_with_options(root, &options).expect("load project");

        assert_eq!(cfg.build_system, BuildSystem::Simple);
        assert!(cfg
            .jpms_workspace
            .as_ref()
            .is_some_and(|jpms| jpms.graph.get(&ModuleName::new("mod.a")).is_some()));

        assert!(
            cfg.module_path
                .iter()
                .any(|e| e.path == dep_dir && e.kind == ClasspathEntryKind::Directory),
            "dependency directory should be classified onto module-path when JPMS is enabled"
        );

        let graph = cfg.module_graph().expect("module graph");
        assert!(graph.can_read(&ModuleName::new("mod.a"), &ModuleName::new("mod.b")));

        let b = graph.get(&ModuleName::new("mod.b")).expect("dep module");
        assert!(b.exports_package_to("com.example.b.api", &ModuleName::new("mod.a")));
        assert!(
            !b.exports_package_to("com.example.b.internal", &ModuleName::new("mod.a")),
            "non-exported packages should not be accessible"
        );

        // Ensure config stays deterministic (important for cache keys).
        let cfg2 = load_project_with_options(root, &options).expect("load project again");
        assert_eq!(cfg, cfg2);

        // Don't accidentally regress the public struct shape.
        let _ = ProjectConfig {
            workspace_root: cfg.workspace_root.clone(),
            build_system: cfg.build_system,
            java: JavaConfig::default(),
            modules: cfg.modules.clone(),
            jpms_modules: cfg.jpms_modules.clone(),
            jpms_workspace: cfg.jpms_workspace.clone(),
            source_roots: cfg.source_roots.clone(),
            module_path: cfg.module_path.clone(),
            classpath: cfg.classpath.clone(),
            output_dirs: cfg.output_dirs.clone(),
            dependencies: cfg.dependencies.clone(),
            workspace_model: cfg.workspace_model.clone(),
        };
    }

    fn make_module_info_class() -> Vec<u8> {
        fn push_u2(out: &mut Vec<u8>, v: u16) {
            out.extend_from_slice(&v.to_be_bytes());
        }
        fn push_u4(out: &mut Vec<u8>, v: u32) {
            out.extend_from_slice(&v.to_be_bytes());
        }

        #[derive(Clone)]
        enum CpEntry {
            Utf8(String),
            Module { name_index: u16 },
            Package { name_index: u16 },
        }

        struct Cp {
            entries: Vec<CpEntry>,
        }

        impl Cp {
            fn new() -> Self {
                Self {
                    entries: Vec::new(),
                }
            }

            fn push(&mut self, entry: CpEntry) -> u16 {
                self.entries.push(entry);
                self.entries.len() as u16
            }

            fn utf8(&mut self, s: &str) -> u16 {
                self.push(CpEntry::Utf8(s.to_string()))
            }

            fn module(&mut self, name: &str) -> u16 {
                let name_index = self.utf8(name);
                self.push(CpEntry::Module { name_index })
            }

            fn package(&mut self, name: &str) -> u16 {
                let name_index = self.utf8(name);
                self.push(CpEntry::Package { name_index })
            }

            fn write(&self, out: &mut Vec<u8>) {
                push_u2(out, (self.entries.len() as u16) + 1);
                for entry in &self.entries {
                    match entry {
                        CpEntry::Utf8(s) => {
                            out.push(1);
                            push_u2(out, s.len() as u16);
                            out.extend_from_slice(s.as_bytes());
                        }
                        CpEntry::Module { name_index } => {
                            out.push(19);
                            push_u2(out, *name_index);
                        }
                        CpEntry::Package { name_index } => {
                            out.push(20);
                            push_u2(out, *name_index);
                        }
                    }
                }
            }
        }

        let mut cp = Cp::new();
        let module_attr_name = cp.utf8("Module");
        let self_module = cp.module("mod.b");
        let api_pkg = cp.package("com/example/b/api");
        let _internal_pkg = cp.package("com/example/b/internal");
        let target_mod = cp.module("mod.a");

        let mut module_attr = Vec::new();
        push_u2(&mut module_attr, self_module); // module_name_index
        push_u2(&mut module_attr, 0); // module_flags
        push_u2(&mut module_attr, 0); // module_version_index
        push_u2(&mut module_attr, 0); // requires_count
        push_u2(&mut module_attr, 1); // exports_count
                                      // exports
        push_u2(&mut module_attr, api_pkg); // exports_index (Package)
        push_u2(&mut module_attr, 0); // exports_flags
        push_u2(&mut module_attr, 1); // exports_to_count
        push_u2(&mut module_attr, target_mod); // exports_to_index (Module)
        push_u2(&mut module_attr, 0); // opens_count
        push_u2(&mut module_attr, 0); // uses_count
        push_u2(&mut module_attr, 0); // provides_count

        let mut out = Vec::new();
        push_u4(&mut out, 0xCAFEBABE);
        push_u2(&mut out, 0); // minor
        push_u2(&mut out, 53); // major (Java 9)
        cp.write(&mut out);

        push_u2(&mut out, 0); // access_flags
        push_u2(&mut out, 0); // this_class
        push_u2(&mut out, 0); // super_class
        push_u2(&mut out, 0); // interfaces_count
        push_u2(&mut out, 0); // fields_count
        push_u2(&mut out, 0); // methods_count

        push_u2(&mut out, 1); // attributes_count
        push_u2(&mut out, module_attr_name);
        push_u4(&mut out, module_attr.len() as u32);
        out.extend_from_slice(&module_attr);

        // Sanity check: ensure the fixture parses.
        let info = parse_module_info_class(&out).expect("parse module-info.class");
        assert_eq!(info.name.as_str(), "mod.b");
        assert!(info.exports_package_to("com.example.b.api", &ModuleName::new("mod.a")));

        out
    }
}
