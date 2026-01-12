use std::sync::Arc;

use nova_core::{ClassId, TypeName};
use nova_db::{FileId, NovaResolve, ProjectId, SalsaDatabase};
use nova_memory::MemoryPressure;
use nova_project::{BuildSystem, JavaConfig, Module, ProjectConfig};
use tempfile::TempDir;

fn base_project_config(root: std::path::PathBuf) -> ProjectConfig {
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

fn setup_two_project_db() -> SalsaDatabase {
    let db = SalsaDatabase::new();
    let tmp = TempDir::new().unwrap();

    let project0 = ProjectId::from_raw(0);
    let project1 = ProjectId::from_raw(1);

    db.set_project_config(
        project0,
        Arc::new(base_project_config(tmp.path().join("project0"))),
    );
    db.set_project_config(
        project1,
        Arc::new(base_project_config(tmp.path().join("project1"))),
    );

    // Project 0 files/types.
    let p0_outer = FileId::from_raw(1);
    db.set_file_project(p0_outer, project0);
    db.set_file_text(
        p0_outer,
        r#"
package a;
class Foo {
    class Inner {}
}
"#,
    );

    let p0_bar = FileId::from_raw(2);
    db.set_file_project(p0_bar, project0);
    db.set_file_text(
        p0_bar,
        r#"
package a;
class Bar {}
"#,
    );

    // Project 1 files/types.
    let p1_foo = FileId::from_raw(3);
    db.set_file_project(p1_foo, project1);
    db.set_file_text(
        p1_foo,
        r#"
package a;
class Foo {}
"#,
    );

    let p1_baz = FileId::from_raw(4);
    db.set_file_project(p1_baz, project1);
    db.set_file_text(
        p1_baz,
        r#"
package b;
class Baz {}
"#,
    );

    db.set_project_files(project0, Arc::new(vec![p0_outer, p0_bar]));
    db.set_project_files(project1, Arc::new(vec![p1_foo, p1_baz]));

    db
}

#[test]
fn class_ids_are_deterministic_across_query_order() {
    let project0 = ProjectId::from_raw(0);
    let project1 = ProjectId::from_raw(1);

    let db1 = setup_two_project_db();
    let ids_1 = db1.with_snapshot(|snap| {
        let bar = snap.class_id_for_type(project0, TypeName::from("a.Bar"));
        let foo_inner = snap.class_id_for_type(project0, TypeName::from("a.Foo$Inner"));
        let foo_p1 = snap.class_id_for_type(project1, TypeName::from("a.Foo"));
        let baz = snap.class_id_for_type(project1, TypeName::from("b.Baz"));
        (bar, foo_inner, foo_p1, baz)
    });

    let db2 = setup_two_project_db();
    let ids_2 = db2.with_snapshot(|snap| {
        // Query in a different order to ensure IDs are not allocated based on call order.
        let baz = snap.class_id_for_type(project1, TypeName::from("b.Baz"));
        let foo_p1 = snap.class_id_for_type(project1, TypeName::from("a.Foo"));
        let foo_inner = snap.class_id_for_type(project0, TypeName::from("a.Foo$Inner"));
        let bar = snap.class_id_for_type(project0, TypeName::from("a.Bar"));
        (bar, foo_inner, foo_p1, baz)
    });

    assert_eq!(ids_1, ids_2);

    // Also validate the deterministic allocation scheme (project, then binary name).
    assert_eq!(ids_1.0, Some(ClassId::from_raw(0))); // (p0, a.Bar)
    assert_eq!(ids_1.1, Some(ClassId::from_raw(2))); // (p0, a.Foo$Inner)
    assert_eq!(ids_1.2, Some(ClassId::from_raw(3))); // (p1, a.Foo)
    assert_eq!(ids_1.3, Some(ClassId::from_raw(4))); // (p1, b.Baz)

    // Inverse mapping works.
    db1.with_snapshot(|snap| {
        let key = snap.class_key(ClassId::from_raw(3));
        assert_eq!(key, Some((project1, TypeName::from("a.Foo"))));
    });
}

#[test]
fn class_ids_survive_salsa_memo_eviction() {
    let project0 = ProjectId::from_raw(0);
    let project1 = ProjectId::from_raw(1);

    let db = setup_two_project_db();
    let before = db.with_snapshot(|snap| {
        (
            snap.class_id_for_type(project0, TypeName::from("a.Bar")),
            snap.class_id_for_type(project0, TypeName::from("a.Foo")),
            snap.class_id_for_type(project0, TypeName::from("a.Foo$Inner")),
            snap.class_id_for_type(project1, TypeName::from("a.Foo")),
            snap.class_id_for_type(project1, TypeName::from("b.Baz")),
        )
    });

    db.evict_salsa_memos(MemoryPressure::Critical);

    let after = db.with_snapshot(|snap| {
        (
            snap.class_id_for_type(project0, TypeName::from("a.Bar")),
            snap.class_id_for_type(project0, TypeName::from("a.Foo")),
            snap.class_id_for_type(project0, TypeName::from("a.Foo$Inner")),
            snap.class_id_for_type(project1, TypeName::from("a.Foo")),
            snap.class_id_for_type(project1, TypeName::from("b.Baz")),
        )
    });

    assert_eq!(before, after);
}
