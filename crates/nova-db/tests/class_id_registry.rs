use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_db::salsa::{Database, WorkspaceLoader};
use nova_db::{FileId, NovaInputs};
use nova_memory::MemoryPressure;

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../nova-project/testdata")
        .join(name)
}

fn copy_dir_all(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create_dir_all");
    for entry in fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("read_dir entry");
        let ty = entry.file_type().expect("file_type");
        let dst = to.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst);
        } else {
            fs::copy(entry.path(), dst).expect("copy");
        }
    }
}

fn file_id_allocator() -> impl FnMut(&Path) -> FileId {
    let mut next: u32 = 0;
    let mut map: HashMap<PathBuf, FileId> = HashMap::new();
    move |path: &Path| {
        if let Some(&id) = map.get(path) {
            return id;
        }
        let id = FileId::from_raw(next);
        next = next.saturating_add(1);
        map.insert(path.to_path_buf(), id);
        id
    }
}

#[test]
fn class_ids_are_stable_across_repeated_lookups() {
    let root = fixture_root("maven-multi");
    assert!(root.is_dir(), "fixture missing: {}", root.display());

    let db = Database::new();
    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    let app_project = loader
        .project_id_for_module("maven:com.example:app")
        .expect("app project");

    let name: Arc<str> = Arc::from("com.example.app.App");

    let id1 = db.with_snapshot(|snap| {
        snap.class_id_for_name(app_project, name.clone())
            .expect("expected App class id")
    });
    let id2 = db.with_snapshot(|snap| {
        snap.class_id_for_name(app_project, name.clone())
            .expect("expected App class id")
    });

    assert_eq!(id1, id2);
    db.with_snapshot(|snap| {
        assert_eq!(
            snap.class_name_for_id(app_project, id1).as_deref(),
            Some("com.example.app.App")
        );
    });
}

#[test]
fn reload_allocates_new_ids_without_changing_existing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("maven-multi");
    copy_dir_all(&fixture_root("maven-multi"), &root);

    let db = Database::new();
    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    let app_project = loader
        .project_id_for_module("maven:com.example:app")
        .expect("app project");

    let app_name: Arc<str> = Arc::from("com.example.app.App");
    let app_id_before = db.with_snapshot(|snap| {
        snap.class_id_for_name(app_project, app_name.clone())
            .expect("expected App class id")
    });

    // Add a new type under the existing source root.
    let extra = root.join("app/src/main/java/com/example/app/Extra.java");
    fs::write(
        &extra,
        "package com.example.app; public class Extra { int x = 1; }",
    )
    .expect("write");

    loader
        .reload(&db, &[extra.clone()], &mut alloc)
        .expect("reload");

    let (app_id_after, extra_id) = db.with_snapshot(|snap| {
        let app_id = snap
            .class_id_for_name(app_project, app_name.clone())
            .expect("expected App class id");
        let extra_id = snap
            .class_id_for_name(app_project, Arc::from("com.example.app.Extra"))
            .expect("expected Extra class id");
        (app_id, extra_id)
    });

    assert_eq!(app_id_before, app_id_after, "existing ids should remain stable");
    assert_ne!(app_id_before, extra_id, "new types must get new ids");
}

#[test]
fn class_ids_survive_salsa_memo_eviction() {
    let root = fixture_root("maven-multi");
    assert!(root.is_dir(), "fixture missing: {}", root.display());

    let db = Database::new();
    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    let app_project = loader
        .project_id_for_module("maven:com.example:app")
        .expect("app project");

    let app_id_before = db.with_snapshot(|snap| {
        snap.class_id_for_name(app_project, Arc::from("com.example.app.App"))
            .expect("expected App class id")
    });

    db.evict_salsa_memos(MemoryPressure::Critical);

    db.with_snapshot(|snap| {
        let app_id_after = snap
            .class_id_for_name(app_project, Arc::from("com.example.app.App"))
            .expect("expected App class id after eviction");
        assert_eq!(app_id_before, app_id_after);
        assert_eq!(
            snap.class_name_for_id(app_project, app_id_after).as_deref(),
            Some("com.example.app.App")
        );
    });
}

