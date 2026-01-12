use std::sync::Arc;

use std::path::PathBuf;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, Module, ProjectConfig};
use tempfile::TempDir;

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

#[test]
fn typeck_does_not_load_java_types_from_classpath_stubs() {
    let project = ProjectId::from_raw(0);
    let mut db = SalsaRootDatabase::default();
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Create a classpath index that (incorrectly) contains a `java.*` class. The resolver should
    // ignore these (mirroring JVM restrictions), and type checking should not be able to "rescue"
    // the type by lazily loading it from the classpath.
    let foo_stub = nova_classpath::ClasspathClassStub {
        binary_name: "java.fake.Foo".to_string(),
        internal_name: "java/fake/Foo".to_string(),
        access_flags: 0,
        super_binary_name: None,
        interfaces: Vec::new(),
        signature: None,
        annotations: Vec::new(),
        fields: Vec::new(),
        methods: vec![nova_classpath::ClasspathMethodStub {
            name: "bar".to_string(),
            descriptor: "()V".to_string(),
            signature: None,
            access_flags: 0,
            annotations: Vec::new(),
        }],
    };

    let module_aware_index = nova_classpath::ModuleAwareClasspathIndex::from_stubs(vec![
        (foo_stub, None),
    ]);
    let classpath_index = module_aware_index.types.clone();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath_index))));

    let src = r#"
class C {
  void m() {
    java.fake.Foo f = null;
    f.bar();
  }
}
"#;

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new("src/Test.java".to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new(src.to_string()));
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| {
            d.code.as_ref() == "unresolved-type" && d.message.contains("java.fake.Foo")
        }),
        "expected unresolved-type diagnostic for java.fake.Foo, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-method" && d.message.contains("bar")),
        "expected unresolved-method diagnostic for bar, got {diags:?}"
    );
}
