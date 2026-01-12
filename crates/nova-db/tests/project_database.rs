use std::path::PathBuf;

use nova_core::ProjectDatabase;
use nova_cache::CacheConfig;
use nova_db::{AnalysisDatabase, FileId, InMemoryFileStore, SalsaDatabase, SalsaDbView};
use tempfile::TempDir;

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

#[test]
fn salsa_database_implements_project_database() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class A {}".to_string());
    db.set_file_path(file, "src/A.java");

    let files = ProjectDatabase::project_files(&db);
    assert_eq!(files, vec![PathBuf::from("src/A.java")]);

    let text = ProjectDatabase::file_text(&db, &files[0]).expect("file text");
    assert_eq!(text, "class A {}");
}

#[test]
fn analysis_database_implements_project_database() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let cache_root = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_root).unwrap();
    let cfg = CacheConfig {
        cache_root_override: Some(cache_root),
    };

    let mut db = AnalysisDatabase::new_with_cache_config(&project_root, cfg).unwrap();
    db.set_file_content("src/A.java", "class A {}");
    db.set_file_content("src/B.java", "class B {}");

    let files = ProjectDatabase::project_files(&db);
    assert_eq!(files, vec![PathBuf::from("src/A.java"), PathBuf::from("src/B.java")]);

    let text = ProjectDatabase::file_text(&db, &files[0]).expect("file text");
    assert_eq!(text, "class A {}");
}
