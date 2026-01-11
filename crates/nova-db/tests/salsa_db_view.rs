use nova_db::{Database as LegacyDatabase, FileId, SalsaDatabase, SalsaDbView};

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

    assert_eq!(view_old.file_content(file), "old");
    assert_eq!(view_new.file_content(file), "new");
}

#[test]
fn salsa_db_view_file_path_none_is_consistent() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class A {}".to_string());

    let view_without_path = SalsaDbView::new(db.snapshot());
    assert!(view_without_path.file_path(file).is_none());

    db.set_file_path(file, "src/A.java");
    assert!(
        view_without_path.file_path(file).is_none(),
        "existing view should not observe new paths"
    );

    let view_with_path = SalsaDbView::new(db.snapshot());
    assert_eq!(
        view_with_path.file_path(file).unwrap().to_str().unwrap(),
        "src/A.java"
    );
    assert_eq!(view_with_path.file_id(std::path::Path::new("src/A.java")), Some(file));
}
