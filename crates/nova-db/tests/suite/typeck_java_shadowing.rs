use std::path::PathBuf;
use std::sync::Arc;

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
    db.set_file_text(file, text);
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

    let module_aware_index =
        nova_classpath::ModuleAwareClasspathIndex::from_stubs(vec![(foo_stub, None)]);
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
    set_file(&mut db, project, file, "src/Test.java", src);
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

#[test]
fn typeck_does_not_load_java_types_from_workspace_stubs() {
    let project = ProjectId::from_raw(0);
    let mut db = SalsaRootDatabase::default();
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    // Define a `java.*` type in the workspace. Resolver semantics intentionally ignore these
    // definitions (mirroring JVM restrictions), so type checking should not be able to
    // "rescue" the unresolved name by loading it from workspace stubs.
    let foo_src = r#"
package java.fake;
class Foo {
  void bar() {}
}
"#;
    let foo_file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        foo_file,
        "src/java/fake/Foo.java",
        foo_src,
    );

    let test_src = r#"
class C {
  void m() {
    java.fake.Foo f = null;
    f.bar();
  }
}
"#;
    let test_file = FileId::from_raw(2);
    set_file(&mut db, project, test_file, "src/Test.java", test_src);

    db.set_project_files(project, Arc::new(vec![foo_file, test_file]));

    let diags = db.type_diagnostics(test_file);
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

#[test]
fn typeck_prefers_workspace_types_over_classpath_stubs() {
    let project = ProjectId::from_raw(0);
    let mut db = SalsaRootDatabase::default();
    let tmp = TempDir::new().unwrap();
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Classpath stub that conflicts with a workspace type of the same binary name. If typeck were
    // to lazily load it, it could overwrite the workspace `ClassDef` in the `TypeStore` and change
    // method resolution results.
    let a_stub = nova_classpath::ClasspathClassStub {
        binary_name: "p.A".to_string(),
        internal_name: "p/A".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: Vec::new(),
        signature: None,
        annotations: Vec::new(),
        fields: Vec::new(),
        methods: vec![nova_classpath::ClasspathMethodStub {
            name: "m".to_string(),
            // static String m()
            descriptor: "()Ljava/lang/String;".to_string(),
            signature: None,
            access_flags: 0x0008,
            annotations: Vec::new(),
        }],
    };
    let module_aware_index =
        nova_classpath::ModuleAwareClasspathIndex::from_stubs(vec![(a_stub, None)]);
    db.set_classpath_index(
        project,
        Some(ArcEq::new(Arc::new(module_aware_index.types.clone()))),
    );

    let src_a = r#"
package p;
class A {
  static int m() { return 1; }
}
"#;
    let src_b = r#"
package p;
class B {
  void test() {
    int x = A.m();
  }
}
"#;

    let a_file = FileId::from_raw(1);
    let b_file = FileId::from_raw(2);
    set_file(&mut db, project, a_file, "src/p/A.java", src_a);
    set_file(&mut db, project, b_file, "src/p/B.java", src_b);
    db.set_project_files(project, Arc::new(vec![a_file, b_file]));

    let offset = src_b.find("A.m()").expect("snippet should contain A.m()") + "A.m".len();
    let ty = db
        .type_at_offset_display(b_file, offset as u32)
        .expect("expected a type at offset for A.m()");
    assert_eq!(
        ty, "int",
        "expected workspace definition of p.A.m() to win over classpath stub; got type {ty:?}"
    );
}
