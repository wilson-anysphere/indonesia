use std::path::Path;

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, JavaConfig, Module, ProjectConfig, SourceRoot,
    SourceRootKind, SourceRootOrigin,
};
use nova_modules::ModuleName;

pub(crate) fn load_bazel_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let mut source_roots = Vec::new();

    // Naive Bazel heuristic:
    // - treat each package directory that contains a BUILD/BUILD.bazel file as a source root
    // - classify "test-ish" directories as test roots
    //
    // `nova-build-bazel` is responsible for the more accurate (per-target) view. `nova-project`
    // keeps a coarse-grained workspace-level config that is good enough for features like test
    // discovery.
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy();
        if name != "BUILD" && name != "BUILD.bazel" {
            continue;
        }

        let Some(dir) = entry.path().parent() else {
            continue;
        };

        source_roots.push(SourceRoot {
            kind: classify_bazel_source_root(dir),
            origin: SourceRootOrigin::Source,
            path: dir.to_path_buf(),
        });
    }

    if source_roots.is_empty() {
        // Fallback: treat the workspace root as a source root so the rest of Nova can still
        // operate (e.g. file watchers, simple indexing).
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: root.to_path_buf(),
        });
    }

    crate::generated::append_generated_source_roots(&mut source_roots, root, &options.nova_config);

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

    source_roots.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.kind.cmp(&b.kind))
            .then(a.origin.cmp(&b.origin))
    });
    source_roots.dedup_by(|a, b| a.kind == b.kind && a.origin == b.origin && a.path == b.path);
    classpath.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    classpath.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);

    Ok(ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Bazel,
        java: JavaConfig::default(),
        modules: vec![Module {
            name: ModuleName::new(
                root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("root")
                    .to_string(),
            ),
            root: root.to_path_buf(),
        }],
        source_roots,
        module_path: Vec::new(),
        classpath,
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
    })
}

fn classify_bazel_source_root(path: &Path) -> SourceRootKind {
    let mut parts = Vec::new();
    for component in path.components() {
        let part = component.as_os_str().to_string_lossy();
        parts.push(part.to_lowercase());
    }

    if parts
        .iter()
        .any(|part| part == "test" || part == "tests" || part == "javatests")
    {
        SourceRootKind::Test
    } else {
        SourceRootKind::Main
    }
}
