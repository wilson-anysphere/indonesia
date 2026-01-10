use std::path::Path;

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, JavaConfig, Module, ProjectConfig, SourceRoot,
    SourceRootKind,
};

pub(crate) fn load_simple_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let mut source_roots = Vec::new();

    // Simple heuristic: `src/` is the main source root, and `src/test/java` is a test root.
    let src_dir = root.join("src");
    if src_dir.is_dir() {
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Main,
            path: src_dir,
        });
    }

    let src_test_java = root.join("src/test/java");
    if src_test_java.is_dir() {
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Test,
            path: src_test_java,
        });
    }

    let mut classpath = Vec::new();
    for entry in &options.classpath_overrides {
        classpath.push(ClasspathEntry {
            kind: if entry.extension().is_some_and(|ext| ext == "jar") {
                ClasspathEntryKind::Jar
            } else {
                ClasspathEntryKind::Directory
            },
            path: entry.clone(),
        });
    }

    source_roots.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    source_roots.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
    classpath.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    classpath.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);

    Ok(ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![Module {
            name: root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("root")
                .to_string(),
            root: root.to_path_buf(),
        }],
        source_roots,
        classpath,
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
    })
}
