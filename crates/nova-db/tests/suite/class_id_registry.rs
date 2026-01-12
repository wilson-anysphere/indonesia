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

fn dep_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
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

    assert_eq!(
        app_id_before, app_id_after,
        "existing ids should remain stable"
    );
    assert_ne!(app_id_before, extra_id, "new types must get new ids");
}

#[test]
fn classpath_class_ids_are_registered_and_stable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("maven-multi");
    copy_dir_all(&fixture_root("maven-multi"), &root);

    // Install a tiny fixture jar into a workspace-local Maven repo and configure
    // `.mvn/maven.config` so the Maven loader resolves it deterministically without
    // touching any host/user global state.
    let maven_repo = tmp.path().join("m2/repository");
    let jar_dest = maven_repo.join("com/example/dep/1.0/dep-1.0.jar");
    fs::create_dir_all(jar_dest.parent().expect("jar parent")).expect("create maven repo dir");
    fs::copy(dep_jar(), &jar_dest).expect("copy dep.jar into maven repo");

    let mvn_dir = root.join(".mvn");
    fs::create_dir_all(&mvn_dir).expect("create .mvn");
    fs::write(
        mvn_dir.join("maven.config"),
        format!("-Dmaven.repo.local={}\n", maven_repo.display()),
    )
    .expect("write maven.config");

    // Add a dependency on the installed jar so it shows up in the project's classpath.
    let app_pom_path = root.join("app/pom.xml");
    let mut app_pom = fs::read_to_string(&app_pom_path).expect("read app/pom.xml");
    if !app_pom.contains("<artifactId>dep</artifactId>") {
        app_pom = app_pom.replace(
            "</dependencies>",
            r#"    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep</artifactId>
      <version>1.0</version>
    </dependency>
  </dependencies>"#,
        );
        fs::write(&app_pom_path, app_pom).expect("write app/pom.xml");
    }

    let db = Database::new();
    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    let app_project = loader
        .project_id_for_module("maven:com.example:app")
        .expect("app project");

    let foo_name: Arc<str> = Arc::from("com.example.dep.Foo");
    let foo_id_before = db.with_snapshot(|snap| {
        snap.class_id_for_name(app_project, foo_name.clone())
            .expect("expected Foo class id")
    });

    loader.reload(&db, &[], &mut alloc).expect("reload");

    let foo_id_after_reload = db.with_snapshot(|snap| {
        snap.class_id_for_name(app_project, foo_name.clone())
            .expect("expected Foo class id after reload")
    });
    assert_eq!(
        foo_id_before, foo_id_after_reload,
        "classpath type ids should remain stable across reload"
    );

    db.evict_salsa_memos(MemoryPressure::Critical);

    db.with_snapshot(|snap| {
        let foo_id_after_evict = snap
            .class_id_for_name(app_project, foo_name.clone())
            .expect("expected Foo class id after eviction");
        assert_eq!(
            foo_id_before, foo_id_after_evict,
            "classpath type ids should survive Salsa memo eviction"
        );
        assert_eq!(
            snap.class_name_for_id(app_project, foo_id_after_evict)
                .as_deref(),
            Some("com.example.dep.Foo")
        );
    });
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

#[test]
fn core_jdk_class_ids_are_seeded_and_stable() {
    let root = fixture_root("maven-multi");
    assert!(root.is_dir(), "fixture missing: {}", root.display());

    let db = Database::new();
    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    let app_project = loader
        .project_id_for_module("maven:com.example:app")
        .expect("app project");

    let names = ["java.lang.Object", "java.util.Map", "java.util.Map$Entry"];

    let ids_before: Vec<_> = db.with_snapshot(|snap| {
        names
            .iter()
            .map(|&name| {
                snap.class_id_for_name(app_project, Arc::from(name))
                    .unwrap_or_else(|| panic!("expected seeded ClassId for {name}"))
            })
            .collect()
    });

    loader.reload(&db, &[], &mut alloc).expect("reload");

    let ids_after_reload: Vec<_> = db.with_snapshot(|snap| {
        names
            .iter()
            .map(|&name| {
                snap.class_id_for_name(app_project, Arc::from(name))
                    .unwrap_or_else(|| panic!("expected ClassId for {name} after reload"))
            })
            .collect()
    });
    assert_eq!(
        ids_before, ids_after_reload,
        "seeded JDK ids should remain stable across reload"
    );

    db.evict_salsa_memos(MemoryPressure::Critical);

    db.with_snapshot(|snap| {
        for (&name, &id_before) in names.iter().zip(ids_before.iter()) {
            let id_after_evict = snap
                .class_id_for_name(app_project, Arc::from(name))
                .unwrap_or_else(|| panic!("expected ClassId for {name} after eviction"));
            assert_eq!(
                id_before, id_after_evict,
                "seeded JDK ids should survive Salsa memo eviction"
            );

            assert_eq!(
                snap.class_name_for_id(app_project, id_after_evict)
                    .as_deref(),
                Some(name)
            );
        }
    });
}
