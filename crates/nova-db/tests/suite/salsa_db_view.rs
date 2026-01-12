use nova_db::{Database as LegacyDatabase, FileId, SalsaDatabase, SalsaDbView, SourceDatabase};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn salsa_db_view_is_send_sync() {
    assert_send_sync::<SalsaDbView>();
}

#[test]
fn salsa_db_view_returns_stable_str_across_calls() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class A {}".to_string());

    let view = SalsaDbView::new(db.snapshot());
    let db: &dyn LegacyDatabase = &view;

    let first = db.file_content(file);
    let second = db.file_content(file);

    assert_eq!(first, "class A {}");
    assert_eq!(first, second);
    assert_eq!(first.as_ptr(), second.as_ptr());
    assert_eq!(first.len(), second.len());
}

#[test]
fn salsa_db_view_keeps_references_alive_for_view_lifetime() {
    let db = SalsaDatabase::new();
    let file_a = FileId::from_raw(0);
    let file_b = FileId::from_raw(1);
    db.set_file_text(file_a, "class A {}".to_string());
    db.set_file_text(file_b, "class B {}".to_string());

    let view = SalsaDbView::new(db.snapshot());
    let db: &dyn LegacyDatabase = &view;

    let a_text = db.file_content(file_a);
    let _b_text = db.file_content(file_b);

    // The `&str` from the first call remains valid after subsequent lookups.
    assert_eq!(a_text, "class A {}");

    let ids = db.all_file_ids();
    assert!(ids.contains(&file_a));
    assert!(ids.contains(&file_b));
}

#[test]
fn salsa_db_view_is_snapshot_isolated_from_main_db_mutations() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "old".to_string());

    let view_old = SalsaDbView::new(db.snapshot());
    db.set_file_text(file, "new".to_string());
    let view_new = SalsaDbView::new(db.snapshot());

    assert_eq!(LegacyDatabase::file_content(&view_old, file), "old");
    assert_eq!(LegacyDatabase::file_content(&view_new, file), "new");
}

#[test]
fn salsa_db_view_file_path_none_is_consistent() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class A {}".to_string());

    let view_without_path = SalsaDbView::new(db.snapshot());
    assert!(LegacyDatabase::file_path(&view_without_path, file).is_none());

    db.set_file_path(file, "src/A.java");
    assert!(
        LegacyDatabase::file_path(&view_without_path, file).is_none(),
        "existing view should not observe new paths"
    );

    let view_with_path = SalsaDbView::new(db.snapshot());
    assert_eq!(
        LegacyDatabase::file_path(&view_with_path, file)
            .unwrap()
            .to_str()
            .unwrap(),
        "src/A.java"
    );
    assert_eq!(
        LegacyDatabase::file_id(&view_with_path, std::path::Path::new("src/A.java")),
        Some(file)
    );
}

#[test]
fn salsa_all_file_ids_only_includes_files_with_content_set() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);

    // `file_exists` alone should not make the file enumeratable: `file_content`
    // is a Salsa input and would panic if no value was set.
    db.set_file_exists(file, true);
    {
        let snap = db.snapshot();
        assert!(
            SourceDatabase::all_file_ids(&snap).is_empty(),
            "files without file_content should not be enumerated"
        );
    }

    // Setting file_content adds the file to the enumerated set.
    //
    // NB: Salsa input writes may block while snapshots are alive, so ensure any
    // prior snapshots are dropped before setting new input values.
    db.set_file_content(file, std::sync::Arc::new("class A {}".to_string()));
    let snap = db.snapshot();
    assert_eq!(SourceDatabase::all_file_ids(&snap).as_ref(), &[file]);

    // The legacy view can still be built and read safely.
    let view = SalsaDbView::new(snap);
    assert_eq!(LegacyDatabase::file_content(&view, file), "class A {}");
}
