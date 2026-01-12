use std::path::PathBuf;
use std::sync::Arc;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase};
use nova_hir::module_info::lower_module_info_source_strict;
use nova_jdk::JdkIndex;
use nova_modules::ModuleName;
use nova_project::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, JavaConfig, JpmsModuleRoot, Module,
    ProjectConfig,
};

use tempfile::TempDir;

fn base_module(root: PathBuf) -> Module {
    Module {
        name: "dummy".to_string(),
        root,
        annotation_processing: Default::default(),
    }
}

#[test]
fn typeck_reports_unresolved_type_for_unexported_workspace_module_package() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    let mod_a_root = root.join("mod-a");
    let mod_b_root = root.join("mod-b");
    std::fs::create_dir_all(mod_a_root.join("a")).unwrap();
    std::fs::create_dir_all(mod_b_root.join("b/internal")).unwrap();

    let src_mod_a = "module a { requires b; }";
    let src_mod_b = "module b { exports b.api; }";
    std::fs::write(mod_a_root.join("module-info.java"), src_mod_a).unwrap();
    std::fs::write(mod_b_root.join("module-info.java"), src_mod_b).unwrap();

    let info_a = lower_module_info_source_strict(src_mod_a).unwrap();
    let info_b = lower_module_info_source_strict(src_mod_b).unwrap();

    let cfg = ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![base_module(root.clone())],
        jpms_modules: vec![
            JpmsModuleRoot {
                name: ModuleName::new("a"),
                root: mod_a_root.clone(),
                module_info: mod_a_root.join("module-info.java"),
                info: info_a,
            },
            JpmsModuleRoot {
                name: ModuleName::new("b"),
                root: mod_b_root.clone(),
                module_info: mod_b_root.join("module-info.java"),
                info: info_b,
            },
        ],
        jpms_workspace: None,
        source_roots: Vec::new(),
        module_path: Vec::new(),
        classpath: Vec::new(),
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    };

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_project_config(project, Arc::new(cfg));
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let src_a = r#"
package a;
class App {
  void m() {
    b.internal.Hidden h;
  }
}
"#;
    let src_b = r#"
package b.internal;
public class Hidden {}
"#;

    db.set_file_project(file_a, project);
    db.set_file_exists(file_a, true);
    db.set_file_content(file_a, Arc::new(src_a.to_string()));
    db.set_file_rel_path(file_a, Arc::new("mod-a/a/App.java".to_string()));

    db.set_file_project(file_b, project);
    db.set_file_exists(file_b, true);
    db.set_file_content(file_b, Arc::new(src_b.to_string()));
    db.set_file_rel_path(file_b, Arc::new("mod-b/b/internal/Hidden.java".to_string()));

    let diags = db.type_diagnostics(file_a);
    assert!(
        diags.iter().any(|d| {
            d.code.as_ref() == "unresolved-type" && d.message.contains("b.internal.Hidden")
        }),
        "expected unresolved-type for unexported package, got {diags:?}"
    );
}

#[test]
fn typeck_allows_classpath_types_from_named_modules_via_all_unnamed_readability() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    let mod_a_root = root.join("mod-a");
    std::fs::create_dir_all(mod_a_root.join("a")).unwrap();

    let src_mod_a = "module a { }";
    std::fs::write(mod_a_root.join("module-info.java"), src_mod_a).unwrap();
    let info_a = lower_module_info_source_strict(src_mod_a).unwrap();

    let dep_jar =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar");

    let cfg = ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![base_module(root.clone())],
        jpms_modules: vec![JpmsModuleRoot {
            name: ModuleName::new("a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: info_a,
        }],
        jpms_workspace: None,
        source_roots: Vec::new(),
        module_path: Vec::new(),
        classpath: vec![ClasspathEntry {
            kind: ClasspathEntryKind::Jar,
            path: dep_jar,
        }],
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    };

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_project_config(project, Arc::new(cfg));
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    db.set_project_files(project, Arc::new(vec![file]));

    let src = r#"
package a;
class App {
  void m() {
    com.example.dep.Foo foo;
  }
}
"#;

    db.set_file_project(file, project);
    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new(src.to_string()));
    db.set_file_rel_path(file, Arc::new("mod-a/a/App.java".to_string()));

    let diags = db.type_diagnostics(file);
    assert!(
        !diags.iter().any(|d| {
            d.code.as_ref() == "unresolved-type" && d.message.contains("com.example.dep.Foo")
        }),
        "expected classpath type to be accessible via ALL-UNNAMED readability, got {diags:?}"
    );
}
