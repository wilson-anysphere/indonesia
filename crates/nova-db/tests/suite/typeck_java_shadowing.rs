use std::path::PathBuf;
use std::sync::Arc;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_hir::module_info::lower_module_info_source_strict;
use nova_jdk::JdkIndex;
use nova_modules::ModuleName;
use nova_project::{
    BuildSystem, ClasspathEntry as ProjectClasspathEntry, ClasspathEntryKind, JavaConfig,
    JpmsModuleRoot, Module, ProjectConfig,
};
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

fn minimal_class_file_bytes(internal_name: &str) -> Vec<u8> {
    fn u16_be(v: u16) -> [u8; 2] {
        v.to_be_bytes()
    }
    fn u32_be(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    let mut out = Vec::new();
    out.extend_from_slice(&u32_be(0xCAFEBABE));
    out.extend_from_slice(&u16_be(0)); // minor
    out.extend_from_slice(&u16_be(52)); // major (Java 8)

    // constant_pool_count = entries + 1
    out.extend_from_slice(&u16_be(7));

    // 1: Utf8 this_class internal name
    out.push(1);
    out.extend_from_slice(&u16_be(internal_name.len() as u16));
    out.extend_from_slice(internal_name.as_bytes());

    // 2: Class #1
    out.push(7);
    out.extend_from_slice(&u16_be(1));

    // 3: Utf8 java/lang/Object
    let super_name = "java/lang/Object";
    out.push(1);
    out.extend_from_slice(&u16_be(super_name.len() as u16));
    out.extend_from_slice(super_name.as_bytes());

    // 4: Class #3
    out.push(7);
    out.extend_from_slice(&u16_be(3));

    // 5: Utf8 method name `bar`
    let bar_name = "bar";
    out.push(1);
    out.extend_from_slice(&u16_be(bar_name.len() as u16));
    out.extend_from_slice(bar_name.as_bytes());

    // 6: Utf8 method descriptor `()V`
    let bar_desc = "()V";
    out.push(1);
    out.extend_from_slice(&u16_be(bar_desc.len() as u16));
    out.extend_from_slice(bar_desc.as_bytes());

    // access_flags (public | interface | abstract)
    out.extend_from_slice(&u16_be(0x0601));
    // this_class
    out.extend_from_slice(&u16_be(2));
    // super_class
    out.extend_from_slice(&u16_be(4));
    // interfaces_count
    out.extend_from_slice(&u16_be(0));
    // fields_count
    out.extend_from_slice(&u16_be(0));

    // methods_count
    out.extend_from_slice(&u16_be(1));
    // method[0]: `public abstract void bar()`
    out.extend_from_slice(&u16_be(0x0401)); // public | abstract
    out.extend_from_slice(&u16_be(5)); // name_index
    out.extend_from_slice(&u16_be(6)); // descriptor_index
    out.extend_from_slice(&u16_be(0)); // attributes_count

    // attributes_count
    out.extend_from_slice(&u16_be(0));

    out
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
fn typeck_jpms_does_not_load_java_types_from_module_path_stubs() {
    let project = ProjectId::from_raw(0);
    let mut db = SalsaRootDatabase::default();
    let tmp = TempDir::new().unwrap();
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    // JPMS projects still read the `classpath_index` input even if typeck ignores it.
    db.set_classpath_index(project, None);

    // Create an automatic module-path entry that (incorrectly) defines a `java.*` class. The
    // resolver should ignore these (mirroring JVM restrictions), and JPMS-aware type checking
    // should not be able to "rescue" the type by lazily loading it from module-path stubs.
    let dep_root = tmp.path().join("evil");
    let class_path = dep_root.join("java/fake/Foo.class");
    std::fs::create_dir_all(class_path.parent().unwrap()).unwrap();
    std::fs::write(&class_path, minimal_class_file_bytes("java/fake/Foo")).unwrap();

    let dep_module = nova_classpath::derive_automatic_module_name_from_path(&dep_root)
        .expect("expected automatic module name to be derived for module-path directory");

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_info_src = format!("module workspace.a {{ requires {}; }}", dep_module.as_str());
    let mod_a_info =
        lower_module_info_source_strict(&mod_a_info_src).expect("module-info should parse");

    let cfg = ProjectConfig {
        workspace_root: tmp.path().to_path_buf(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![Module {
            name: "dummy".to_string(),
            root: tmp.path().to_path_buf(),
            annotation_processing: Default::default(),
        }],
        jpms_modules: vec![JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: mod_a_info,
        }],
        jpms_workspace: None,
        source_roots: Vec::new(),
        module_path: vec![ProjectClasspathEntry {
            kind: ClasspathEntryKind::Directory,
            path: dep_root,
        }],
        classpath: Vec::new(),
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    };
    db.set_project_config(project, Arc::new(cfg));

    let src = r#"
package com.example.a;

class C {
  void m() {
    java.fake.Foo f = null;
    f.bar();
  }
}
"#;

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(
        file,
        Arc::new("mod-a/src/main/java/com/example/a/C.java".to_string()),
    );
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_text(file, src);
    db.set_all_file_ids(Arc::new(vec![file]));
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
