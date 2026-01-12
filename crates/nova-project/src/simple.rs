use std::path::Path;

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, JavaConfig, JavaLanguageLevel,
    LanguageLevelProvenance, Module, ModuleLanguageLevel, ProjectConfig, SourceRoot,
    SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId, WorkspaceModuleConfig,
    WorkspaceProjectModel,
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
            origin: SourceRootOrigin::Source,
            path: src_dir,
        });
    }

    let src_test_java = root.join("src/test/java");
    if src_test_java.is_dir() {
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Test,
            origin: SourceRootOrigin::Source,
            path: src_test_java,
        });
    }

    crate::generated::append_generated_source_roots(
        &mut source_roots,
        root,
        root,
        BuildSystem::Simple,
        &options.nova_config,
    );

    let mut dependency_entries = Vec::new();
    for entry in &options.classpath_overrides {
        dependency_entries.push(ClasspathEntry {
            kind: if entry
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                }) {
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
    dependency_entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    dependency_entries.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);

    let modules = vec![Module {
        name: root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("root")
            .to_string(),
        root: root.to_path_buf(),
        annotation_processing: Default::default(),
    }];
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules);
    let (mut module_path, mut classpath) =
        crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);
    classpath.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    classpath.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
    module_path.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    module_path.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
    let jpms_workspace = crate::jpms::build_jpms_workspace(&jpms_modules, &module_path);

    Ok(ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules,
        jpms_modules,
        jpms_workspace,
        source_roots,
        module_path,
        classpath,
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    })
}

pub(crate) fn load_simple_workspace_model(
    root: &Path,
    options: &LoadOptions,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let mut source_roots = Vec::new();

    // Simple heuristic: `src/` is the main source root, and `src/test/java` is a test root.
    let src_dir = root.join("src");
    if src_dir.is_dir() {
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: src_dir,
        });
    }

    let src_test_java = root.join("src/test/java");
    if src_test_java.is_dir() {
        source_roots.push(SourceRoot {
            kind: SourceRootKind::Test,
            origin: SourceRootOrigin::Source,
            path: src_test_java,
        });
    }

    crate::generated::append_generated_source_roots(
        &mut source_roots,
        root,
        root,
        BuildSystem::Simple,
        &options.nova_config,
    );

    let mut dependency_entries = Vec::new();
    for entry in &options.classpath_overrides {
        dependency_entries.push(ClasspathEntry {
            kind: if entry
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod")
                }) {
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
    dependency_entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    dependency_entries.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);

    let module_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("root")
        .to_string();

    let jpms_modules = crate::jpms::discover_jpms_modules(&[Module {
        name: module_name.clone(),
        root: root.to_path_buf(),
        annotation_processing: Default::default(),
    }]);
    let (mut module_path, mut classpath) =
        crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);
    classpath.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    classpath.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
    module_path.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    module_path.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);

    let module_config = WorkspaceModuleConfig {
        id: format!("simple:{module_name}"),
        name: module_name.clone(),
        root: root.to_path_buf(),
        build_id: WorkspaceModuleBuildId::Simple,
        language_level: ModuleLanguageLevel {
            level: JavaLanguageLevel::from_java_config(JavaConfig::default()),
            provenance: LanguageLevelProvenance::Default,
        },
        source_roots,
        output_dirs: Vec::new(),
        module_path,
        classpath,
        dependencies: Vec::new(),
    };

    Ok(WorkspaceProjectModel::new(
        root.to_path_buf(),
        BuildSystem::Simple,
        JavaConfig::default(),
        vec![module_config],
        jpms_modules,
    ))
}
