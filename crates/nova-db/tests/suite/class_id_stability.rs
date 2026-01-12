use std::path::PathBuf;
use std::sync::Arc;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_db::{
    ArcEq, FileId, NovaHir, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId,
};
use nova_hir::module_info::lower_module_info_source_strict;
use nova_jdk::JdkIndex;
use nova_modules::ModuleName;
use nova_project::{
    BuildSystem, ClasspathEntry as ProjectClasspathEntry, ClasspathEntryKind, JavaConfig,
    JpmsModuleRoot, Module, ProjectConfig,
};
use nova_resolve::ids::DefWithBodyId;
use nova_types::TypeEnv;
use tempfile::TempDir;

fn dep_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
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
    db.set_file_text(file, text);
}

fn find_method_named(tree: &nova_hir::item_tree::ItemTree, name: &str) -> nova_hir::ids::MethodId {
    fn visit_item(
        tree: &nova_hir::item_tree::ItemTree,
        item: nova_hir::item_tree::Item,
        name: &str,
    ) -> Option<nova_hir::ids::MethodId> {
        use nova_hir::item_tree::{Item, Member};

        let members = match item {
            Item::Class(id) => &tree.class(id).members,
            Item::Interface(id) => &tree.interface(id).members,
            Item::Enum(id) => &tree.enum_(id).members,
            Item::Record(id) => &tree.record(id).members,
            Item::Annotation(id) => &tree.annotation(id).members,
        };

        for member in members {
            match member {
                Member::Method(id) if tree.method(*id).name == name => return Some(*id),
                Member::Type(child) => {
                    if let Some(found) = visit_item(tree, *child, name) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }

    for item in &tree.items {
        if let Some(id) = visit_item(tree, *item, name) {
            return id;
        }
    }

    panic!("method {name:?} not found in test fixture")
}

#[test]
fn external_class_ids_are_stable_across_bodies() {
    // Avoid reading/writing any global caches in unit tests: build the index directly from the
    // fixture jar without the optional dependency store.
    let classpath =
        ClasspathIndex::build_with_deps_store(&[ClasspathEntry::Jar(dep_jar())], None, None, None)
            .expect("failed to build dep.jar classpath index");

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);
    db.set_file_project(file_a, project);
    db.set_file_project(file_b, project);
    db.set_file_rel_path(file_a, Arc::new("src/A.java".to_string()));
    db.set_file_rel_path(file_b, Arc::new("src/B.java".to_string()));
    db.set_all_file_ids(Arc::new(vec![file_a, file_b]));
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    // Ensure both Foo and Bar are referenced/loaded in file A, but only Foo is referenced in file
    // B. Without deterministic pre-interning, the different load orders will assign different
    // `ClassId`s to Foo across the two body-local type environments.
    let src_a = r#"
class A {
    com.example.dep.Foo m(com.example.dep.Bar b) { return null; }
}
"#;
    let src_b = r#"
class B {
    com.example.dep.Foo m() { return null; }
}
"#;

    db.set_file_text(file_a, src_a);
    db.set_file_text(file_b, src_b);

    let tree_a = db.hir_item_tree(file_a);
    let tree_b = db.hir_item_tree(file_b);
    let method_a = find_method_named(&tree_a, "m");
    let method_b = find_method_named(&tree_b, "m");

    let body_a = db.typeck_body(DefWithBodyId::Method(method_a));
    let body_b = db.typeck_body(DefWithBodyId::Method(method_b));

    let foo_a = body_a
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("Foo should be interned in body A env");
    let foo_b = body_b
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("Foo should be interned in body B env");

    assert_eq!(
        foo_a, foo_b,
        "expected Foo to have stable ClassId across bodies"
    );
}

#[test]
fn external_class_ids_are_stable_across_bodies_in_same_file() {
    let classpath =
        ClasspathIndex::build_with_deps_store(&[ClasspathEntry::Jar(dep_jar())], None, None, None)
            .expect("failed to build dep.jar classpath index");

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new("src/C.java".to_string()));
    db.set_all_file_ids(Arc::new(vec![file]));
    db.set_project_files(project, Arc::new(vec![file]));

    let src = r#"
class C {
    void a() {
        com.example.dep.Foo foo;
        com.example.dep.Bar bar;
    }

    void b() {
        com.example.dep.Bar bar;
        com.example.dep.Foo foo;
    }
}
"#;
    db.set_file_text(file, src);

    let tree = db.hir_item_tree(file);
    let method_a = find_method_named(&tree, "a");
    let method_b = find_method_named(&tree, "b");

    let body_a = db.typeck_body(DefWithBodyId::Method(method_a));
    let body_b = db.typeck_body(DefWithBodyId::Method(method_b));

    let foo_a = body_a
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("Foo should be interned in body a env");
    let bar_a = body_a
        .env
        .lookup_class("com.example.dep.Bar")
        .expect("Bar should be interned in body a env");

    let foo_b = body_b
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("Foo should be interned in body b env");
    let bar_b = body_b
        .env
        .lookup_class("com.example.dep.Bar")
        .expect("Bar should be interned in body b env");

    assert_eq!(
        foo_a, foo_b,
        "expected Foo to have stable ClassId across bodies"
    );
    assert_eq!(
        bar_a, bar_b,
        "expected Bar to have stable ClassId across bodies"
    );
}

#[test]
fn workspace_class_ids_are_stable_across_bodies() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);
    db.set_all_file_ids(Arc::new(vec![file_a, file_b]));
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let src_a = r#"
package p;
class A {
    A id() { return null; }
}
"#;
    let src_b = r#"
package p;
class B {
    A m() { return null; }
}
"#;

    set_file(&mut db, project, file_a, "src/p/A.java", src_a);
    set_file(&mut db, project, file_b, "src/p/B.java", src_b);

    let tree_a = db.hir_item_tree(file_a);
    let tree_b = db.hir_item_tree(file_b);
    let method_a = find_method_named(&tree_a, "id");
    let method_b = find_method_named(&tree_b, "m");

    let body_a = db.typeck_body(DefWithBodyId::Method(method_a));
    let body_b = db.typeck_body(DefWithBodyId::Method(method_b));

    let a_in_a = body_a
        .env
        .lookup_class("p.A")
        .expect("A should be interned in body A env");
    let a_in_b = body_b
        .env
        .lookup_class("p.A")
        .expect("A should be interned in body B env");

    assert_eq!(
        a_in_a, a_in_b,
        "expected workspace type to have stable ClassId across bodies"
    );
}

#[test]
fn workspace_class_ids_are_stable_across_bodies_in_same_file() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let types_file = FileId::from_raw(1);
    let user_file = FileId::from_raw(2);
    db.set_all_file_ids(Arc::new(vec![types_file, user_file]));
    // Stable order by `file_rel_path`.
    db.set_project_files(project, Arc::new(vec![user_file, types_file]));

    set_file(
        &mut db,
        project,
        types_file,
        "src/Types.java",
        r#"
class A {}
class B {}
"#,
    );

    // Reference A/B in different orders across two bodies.
    set_file(
        &mut db,
        project,
        user_file,
        "src/C.java",
        r#"
class C {
    void m1() {
        A a = new A();
        B b = new B();
    }

    void m2() {
        B b = new B();
        A a = new A();
    }
}
"#,
    );

    let tree = db.hir_item_tree(user_file);
    let m1 = find_method_named(&tree, "m1");
    let m2 = find_method_named(&tree, "m2");

    let body1 = db.typeck_body(DefWithBodyId::Method(m1));
    let body2 = db.typeck_body(DefWithBodyId::Method(m2));

    let a1 = body1.env.lookup_class("A").expect("A should be interned");
    let a2 = body2.env.lookup_class("A").expect("A should be interned");
    assert_eq!(a1, a2, "expected A to have stable ClassId across bodies");

    let b1 = body1.env.lookup_class("B").expect("B should be interned");
    let b2 = body2.env.lookup_class("B").expect("B should be interned");
    assert_eq!(b1, b2, "expected B to have stable ClassId across bodies");
}

#[test]
fn workspace_class_ids_are_stable_across_files() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let foo_file = FileId::from_raw(1);
    let bar_file = FileId::from_raw(2);
    db.set_all_file_ids(Arc::new(vec![foo_file, bar_file]));
    // Stable order by `file_rel_path`.
    db.set_project_files(project, Arc::new(vec![bar_file, foo_file]));

    set_file(
        &mut db,
        project,
        foo_file,
        "src/Foo.java",
        r#"
class Foo {
    static Foo make() {
        return new Foo();
    }
}
"#,
    );

    set_file(
        &mut db,
        project,
        bar_file,
        "src/Bar.java",
        r#"
class Bar {
    Foo makeFoo() { return new Foo(); }
}
"#,
    );

    let foo_tree = db.hir_item_tree(foo_file);
    let bar_tree = db.hir_item_tree(bar_file);
    let foo_method = find_method_named(&foo_tree, "make");
    let bar_method = find_method_named(&bar_tree, "makeFoo");

    let foo_body = db.typeck_body(DefWithBodyId::Method(foo_method));
    let bar_body = db.typeck_body(DefWithBodyId::Method(bar_method));

    let foo_from_foo = foo_body
        .env
        .lookup_class("Foo")
        .expect("Foo should be interned in Foo env");
    let foo_from_bar = bar_body
        .env
        .lookup_class("Foo")
        .expect("Foo should be interned in Bar env");
    assert_eq!(
        foo_from_foo, foo_from_bar,
        "expected Foo to have stable ClassId across files"
    );
}

#[test]
fn jpms_external_class_ids_are_stable_across_bodies_in_same_file() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    // JPMS projects still read the `classpath_index` input even if typeck ignores it.
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_info = lower_module_info_source_strict("module workspace.a { requires dep; }")
        .expect("module-info should parse");

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
            kind: ClasspathEntryKind::Jar,
            path: dep_jar(),
        }],
        classpath: Vec::new(),
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    };
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(
        file,
        Arc::new("mod-a/src/main/java/com/example/a/C.java".to_string()),
    );
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_all_file_ids(Arc::new(vec![file]));
    db.set_project_files(project, Arc::new(vec![file]));

    let src = r#"
package com.example.a;

class C {
    void a() {
        com.example.dep.Foo foo;
        com.example.dep.Bar bar;
    }

    void b() {
        com.example.dep.Bar bar;
        com.example.dep.Foo foo;
    }
}
"#;
    db.set_file_text(file, src);

    let tree = db.hir_item_tree(file);
    let method_a = find_method_named(&tree, "a");
    let method_b = find_method_named(&tree, "b");

    let body_a = db.typeck_body(DefWithBodyId::Method(method_a));
    let body_b = db.typeck_body(DefWithBodyId::Method(method_b));

    let foo_a = body_a
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("Foo should be interned in body a env");
    let bar_a = body_a
        .env
        .lookup_class("com.example.dep.Bar")
        .expect("Bar should be interned in body a env");

    let foo_b = body_b
        .env
        .lookup_class("com.example.dep.Foo")
        .expect("Foo should be interned in body b env");
    let bar_b = body_b
        .env
        .lookup_class("com.example.dep.Bar")
        .expect("Bar should be interned in body b env");

    assert_eq!(
        foo_a, foo_b,
        "expected Foo to have stable ClassId across bodies"
    );
    assert_eq!(
        bar_a, bar_b,
        "expected Bar to have stable ClassId across bodies"
    );
}
