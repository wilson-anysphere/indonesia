use std::path::PathBuf;

use nova_core::ProjectDatabase;
use nova_db::{FileId, InMemoryFileStore, SalsaDatabase, SalsaDbView};

#[test]
fn in_memory_file_store_implements_project_database() {
    let mut store = InMemoryFileStore::new();
    let file_id = store.file_id_for_path("src/Main.java");
    store.set_file_text(file_id, "class Main {}".to_string());

    let files = ProjectDatabase::project_files(&store);
    assert_eq!(files, vec![PathBuf::from("src/Main.java")]);

    let text = ProjectDatabase::file_text(&store, &files[0]).expect("file text");
    assert_eq!(text, "class Main {}");
}

#[test]
fn salsa_db_view_implements_project_database() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class A {}".to_string());
    db.set_file_path(file, "src/A.java");

    let view = SalsaDbView::new(db.snapshot());

    let files = ProjectDatabase::project_files(&view);
    assert_eq!(files, vec![PathBuf::from("src/A.java")]);

    let text = ProjectDatabase::file_text(&view, &files[0]).expect("file text");
    assert_eq!(text, "class A {}");
}

