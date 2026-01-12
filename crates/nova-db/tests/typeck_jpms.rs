use std::path::PathBuf;
use std::sync::Arc;

use nova_classpath::{ClasspathEntry as CpEntry, ClasspathIndex};
use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_hir::module_info::lower_module_info_source_strict;
use nova_jdk::JdkIndex;
use nova_modules::ModuleName;
use nova_project::{
    BuildSystem, ClasspathEntry as ProjectClasspathEntry, ClasspathEntryKind, JavaConfig,
    JpmsModuleRoot, Module, ProjectConfig,
};
use tempfile::TempDir;

fn test_dep_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
}

fn test_named_module_hidden_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../nova-classpath/testdata/named-module-hidden.jar")
}

fn base_project_config(root: PathBuf) -> ProjectConfig {
    ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![Module {
            name: "dummy".to_string(),
            root,
            annotation_processing: Default::default(),
        }],
        jpms_modules: Vec::new(),
        jpms_workspace: None,
        source_roots: Vec::new(),
        module_path: Vec::new(),
        classpath: Vec::new(),
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    }
}

fn set_file(
    db: &mut SalsaRootDatabase,
    project: ProjectId,
    file: FileId,
    rel_path: &str,
    text: &str,
) {
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new(rel_path.to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new(text.to_string()));
}

#[test]
fn jpms_typeck_requires_is_enforced_for_module_path_automatic_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Simulate the current WorkspaceLoader behavior: merge module-path jars into the classpath
    // index input. Typeck should ignore this in JPMS mode and consult the JPMS compilation env.
    let classpath = ClasspathIndex::build(&[CpEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![JpmsModuleRoot {
        name: ModuleName::new("workspace.a"),
        root: mod_a_root.clone(),
        module_info: mod_a_root.join("module-info.java"),
        info: mod_a_info,
    }];
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_dep_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.dep.Foo f = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.dep.Foo")),
        "expected unresolved-type diagnostic for com.example.dep.Foo, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_requires_allows_resolution_for_module_path_automatic_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Same classpath-index setup as above to ensure we're not accidentally "passing" by ignoring
    // the module path entirely.
    let classpath = ClasspathIndex::build(&[CpEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { requires dep; }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![JpmsModuleRoot {
        name: ModuleName::new("workspace.a"),
        root: mod_a_root.clone(),
        module_info: mod_a_root.join("module-info.java"),
        info: mod_a_info,
    }];
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_dep_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.dep.Foo f = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.dep.Foo")),
        "expected com.example.dep.Foo to resolve without unresolved-type diagnostics, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_exports_are_enforced_for_explicit_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Same merged classpath-index setup to exercise the historical bug.
    let classpath =
        ClasspathIndex::build(&[CpEntry::Jar(test_named_module_hidden_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { requires example.mod; }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![JpmsModuleRoot {
        name: ModuleName::new("workspace.a"),
        root: mod_a_root.clone(),
        module_info: mod_a_root.join("module-info.java"),
        info: mod_a_info,
    }];
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_named_module_hidden_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.hidden.Hidden h = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.hidden.Hidden")),
        "expected unresolved-type diagnostic for com.example.hidden.Hidden, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_allows_exported_packages_from_explicit_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    let classpath =
        ClasspathIndex::build(&[CpEntry::Jar(test_named_module_hidden_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { requires example.mod; }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![JpmsModuleRoot {
        name: ModuleName::new("workspace.a"),
        root: mod_a_root.clone(),
        module_info: mod_a_root.join("module-info.java"),
        info: mod_a_info,
    }];
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_named_module_hidden_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.api.Api a = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.api.Api")),
        "expected com.example.api.Api to resolve without unresolved-type diagnostics, got {diags:?}"
    );
}

