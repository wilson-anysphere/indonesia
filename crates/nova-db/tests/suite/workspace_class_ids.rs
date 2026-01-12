use std::sync::Arc;

use nova_core::TypeName;
use nova_db::{
    FileId, NovaInputs, NovaResolve, ProjectId, SalsaDatabase, SalsaRootDatabase, SourceRootId,
};
use nova_memory::MemoryPressure;

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

fn set_file_threadsafe(
    db: &SalsaDatabase,
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
fn stable_ids_across_edits() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);

    let file_a = FileId::from_raw(1);
    let file_c = FileId::from_raw(2);
    set_file(&mut db, project, file_a, "src/A.java", "class A {}");
    set_file(&mut db, project, file_c, "src/C.java", "class C {}");
    db.set_project_files(project, Arc::new(vec![file_a, file_c]));
    db.set_all_file_ids(Arc::new(vec![file_a, file_c]));

    let a1 = db
        .class_id_for_workspace_type(project, TypeName::from("A"))
        .expect("A should be in the workspace");
    let c1 = db
        .class_id_for_workspace_type(project, TypeName::from("C"))
        .expect("C should be in the workspace");

    // Add a new workspace type `B` and assert existing IDs are unchanged.
    let file_b = FileId::from_raw(3);
    set_file(&mut db, project, file_b, "src/B.java", "class B {}");
    db.set_project_files(project, Arc::new(vec![file_a, file_b, file_c]));
    db.set_all_file_ids(Arc::new(vec![file_a, file_b, file_c]));

    let a2 = db
        .class_id_for_workspace_type(project, TypeName::from("A"))
        .expect("A should still be in the workspace");
    let c2 = db
        .class_id_for_workspace_type(project, TypeName::from("C"))
        .expect("C should still be in the workspace");
    let b2 = db
        .class_id_for_workspace_type(project, TypeName::from("B"))
        .expect("B should be in the workspace");

    assert_eq!(a1, a2, "existing type IDs must be stable across edits");
    assert_eq!(c1, c2, "existing type IDs must be stable across edits");
    assert_ne!(b2, a2, "new type must get a distinct ID");
    assert_ne!(b2, c2, "new type must get a distinct ID");
}

#[test]
fn order_independent_querying_yields_same_ids() {
    fn make_db() -> SalsaRootDatabase {
        let mut db = SalsaRootDatabase::default();
        let project = ProjectId::from_raw(0);
        let file_a = FileId::from_raw(1);
        let file_b = FileId::from_raw(2);
        let file_c = FileId::from_raw(3);
        set_file(&mut db, project, file_a, "src/A.java", "class A {}");
        set_file(&mut db, project, file_b, "src/B.java", "class B {}");
        set_file(&mut db, project, file_c, "src/C.java", "class C {}");
        db.set_project_files(project, Arc::new(vec![file_a, file_b, file_c]));
        db.set_all_file_ids(Arc::new(vec![file_a, file_b, file_c]));
        db
    }

    let project = ProjectId::from_raw(0);

    let db1 = make_db();
    let a1 = db1
        .class_id_for_workspace_type(project, TypeName::from("A"))
        .unwrap();
    let b1 = db1
        .class_id_for_workspace_type(project, TypeName::from("B"))
        .unwrap();
    let c1 = db1
        .class_id_for_workspace_type(project, TypeName::from("C"))
        .unwrap();

    let db2 = make_db();
    let c2 = db2
        .class_id_for_workspace_type(project, TypeName::from("C"))
        .unwrap();
    let a2 = db2
        .class_id_for_workspace_type(project, TypeName::from("A"))
        .unwrap();
    let b2 = db2
        .class_id_for_workspace_type(project, TypeName::from("B"))
        .unwrap();

    assert_eq!(a1, a2);
    assert_eq!(b1, b2);
    assert_eq!(c1, c2);
}

#[test]
fn eviction_preserves_interned_ids_across_salsa_memo_eviction() {
    let db = SalsaDatabase::default();
    let project = ProjectId::from_raw(0);

    // Start with `{A, C}` and force interning.
    let file_a = FileId::from_raw(1);
    let file_c = FileId::from_raw(2);
    set_file_threadsafe(&db, project, file_a, "src/A.java", "class A {}");
    set_file_threadsafe(&db, project, file_c, "src/C.java", "class C {}");
    db.set_project_files(project, Arc::new(vec![file_a, file_c]));

    let (a1, c1) = db.with_snapshot(|snap| {
        (
            snap.class_id_for_workspace_type(project, TypeName::from("A"))
                .unwrap(),
            snap.class_id_for_workspace_type(project, TypeName::from("C"))
                .unwrap(),
        )
    });

    // Add `B` (causing monotonic IDs to "append") and verify incremental stability.
    let file_b = FileId::from_raw(3);
    set_file_threadsafe(&db, project, file_b, "src/B.java", "class B {}");
    db.set_project_files(project, Arc::new(vec![file_a, file_b, file_c]));

    let (a2, b2, c2) = db.with_snapshot(|snap| {
        (
            snap.class_id_for_workspace_type(project, TypeName::from("A"))
                .unwrap(),
            snap.class_id_for_workspace_type(project, TypeName::from("B"))
                .unwrap(),
            snap.class_id_for_workspace_type(project, TypeName::from("C"))
                .unwrap(),
        )
    });

    assert_eq!(a1, a2);
    assert_eq!(c1, c2);
    assert_ne!(b2, a2);
    assert_ne!(b2, c2);

    // `evict_salsa_memos` rebuilds Salsa memo storage under memory pressure, but Nova snapshots and
    // restores the interned tables it relies on (see `InternedTablesSnapshot`) so
    // `#[ra_salsa::interned]` IDs remain stable across eviction within the lifetime of a single
    // `SalsaDatabase`.
    db.evict_salsa_memos(MemoryPressure::Critical);

    let (a3, b3, c3) = db.with_snapshot(|snap| {
        (
            snap.class_id_for_workspace_type(project, TypeName::from("A"))
                .unwrap(),
            snap.class_id_for_workspace_type(project, TypeName::from("B"))
                .unwrap(),
            snap.class_id_for_workspace_type(project, TypeName::from("C"))
                .unwrap(),
        )
    });

    assert_eq!(a2, a3);
    assert_eq!(b2, b3);
    assert_eq!(c2, c3);
}
