use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_db::salsa::{Database, NovaSyntax, WorkspaceLoader};
use nova_db::{FileId, NovaInputs};

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
fn loads_maven_multi_module_workspace_into_salsa_inputs() {
    let root = fixture_root("maven-multi");
    assert!(root.is_dir(), "fixture missing: {}", root.display());

    let db = Database::new();
    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    let app_project = loader
        .project_id_for_module("maven:com.example:app")
        .expect("app project");
    let lib_project = loader
        .project_id_for_module("maven:com.example:lib")
        .expect("lib project");

    db.with_snapshot(|snap| {
        let app_cfg = snap.project_config(app_project);
        assert!(app_cfg
            .source_roots
            .iter()
            .any(|r| r.kind == nova_project::SourceRootKind::Main
                && r.origin == nova_project::SourceRootOrigin::Source
                && r.path.ends_with(Path::new("app/src/main/java"))));
        assert_eq!(app_cfg.java.source.0, 17);

        let lib_cfg = snap.project_config(lib_project);
        assert!(lib_cfg
            .source_roots
            .iter()
            .any(|r| r.kind == nova_project::SourceRootKind::Main
                && r.origin == nova_project::SourceRootOrigin::Source
                && r.path.ends_with(Path::new("lib/src/main/java"))));
        assert_eq!(lib_cfg.java.source.0, 17);

        let app_files = snap.project_files(app_project);
        let app_rel_paths: Vec<String> = app_files
            .iter()
            .map(|&file| snap.file_rel_path(file).as_ref().clone())
            .collect();
        assert!(
            app_rel_paths.contains(&"app/src/main/java/com/example/app/App.java".to_string()),
            "unexpected app files: {app_rel_paths:?}"
        );

        let lib_files = snap.project_files(lib_project);
        let lib_rel_paths: Vec<String> = lib_files
            .iter()
            .map(|&file| snap.file_rel_path(file).as_ref().clone())
            .collect();
        assert!(
            lib_rel_paths.contains(&"lib/src/main/java/com/example/lib/Lib.java".to_string()),
            "unexpected lib files: {lib_rel_paths:?}"
        );

        // Language level is derived from the file's owning project.
        let app_file = *app_files
            .iter()
            .find(|&&file| snap.file_rel_path(file).as_ref().ends_with("App.java"))
            .expect("app file id");
        assert_eq!(snap.file_project(app_file), app_project);
        assert_eq!(snap.java_language_level(app_file).major, 17);

        // Classpath index is wired (may be empty if jars/directories contain no classes yet).
        assert!(snap.classpath_index(app_project).is_some());
    });
}

#[test]
fn loads_gradle_multi_module_workspace_into_salsa_inputs() {
    let root = fixture_root("gradle-multi");
    assert!(root.is_dir(), "fixture missing: {}", root.display());

    let db = Database::new();
    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    let app_project = loader
        .project_id_for_module("gradle::app")
        .expect("app project");

    db.with_snapshot(|snap| {
        let cfg = snap.project_config(app_project);
        assert!(cfg.source_roots.iter().any(|r| {
            r.kind == nova_project::SourceRootKind::Main
                && r.origin == nova_project::SourceRootOrigin::Source
                && r.path.ends_with(Path::new("app/src/main/java"))
        }));
        assert_eq!(cfg.java.source.0, 17);

        let files = snap.project_files(app_project);
        assert!(
            files.iter().any(|&file| {
                snap.file_rel_path(file)
                    .as_ref()
                    .ends_with("app/src/main/java/com/example/app/App.java")
            }),
            "unexpected project files: {:?}",
            files
                .iter()
                .map(|&f| snap.file_rel_path(f).as_ref().clone())
                .collect::<Vec<_>>()
        );
        assert!(snap.classpath_index(app_project).is_some());
    });
}

#[test]
fn jpms_project_config_includes_module_graph_entries_from_module_path() {
    let fixture = fixture_root("jpms-maven-transitive");
    assert!(fixture.is_dir(), "fixture missing: {}", fixture.display());

    // WorkspaceLoader uses `load_workspace_model_with_workspace_config` which does not let tests
    // override the Maven repo path directly. Copy the fixture into a temp directory and seed a
    // minimal local Maven repo via `.mvn/maven.config` so dependency jars exist and `module_path`
    // is populated deterministically.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("jpms-maven-transitive");
    copy_dir_all(&fixture, &root);

    let mvn_dir = root.join(".mvn");
    fs::create_dir_all(&mvn_dir).expect("mkdir .mvn");
    fs::write(mvn_dir.join("maven.config"), "-Dmaven.repo.local=repo").expect("write maven.config");

    let dep_jar = root.join("repo/com/example/dep/1.0/dep-1.0.jar");
    fs::create_dir_all(dep_jar.parent().expect("jar parent")).expect("mkdir repo dirs");
    fs::write(&dep_jar, b"").expect("write fake jar");

    let db = Database::new();
    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    let app_project = loader
        .project_id_for_module("maven:com.example:app")
        .expect("app project");

    db.with_snapshot(|snap| {
        let cfg = snap.project_config(app_project);
        assert!(
            !cfg.jpms_modules.is_empty(),
            "expected JPMS modules to be discovered"
        );
        assert!(
            !cfg.module_path.is_empty(),
            "expected module_path entries to be populated for JPMS workspaces"
        );

        let graph = cfg.module_graph().expect("module graph");
        assert!(
            graph.iter().count() > cfg.jpms_modules.len(),
            "expected module graph to include modules derived from module_path entries"
        );

        let roots = cfg.module_roots().expect("module roots");
        assert!(
            roots.len() > cfg.jpms_modules.len(),
            "expected module_roots to include module-path entry roots"
        );
    });
}

#[test]
fn reload_preserves_stable_ids_and_reuses_indexes_when_unchanged() {
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

    let (before_jdk, before_cp, before_app_file, before_root) = db.with_snapshot(|snap| {
        let before_jdk = snap.jdk_index(app_project);
        let before_cp = snap
            .classpath_index(app_project)
            .expect("classpath index should be set");
        let app_file = snap
            .project_files(app_project)
            .iter()
            .copied()
            .find(|&file| snap.file_rel_path(file).as_ref().ends_with("App.java"))
            .expect("app file id");
        (before_jdk, before_cp, app_file, snap.source_root(app_file))
    });

    // Add a new file under the existing source root.
    let extra = root.join("app/src/main/java/com/example/app/Extra.java");
    fs::write(
        &extra,
        "package com.example.app; public class Extra { int x = 1; }",
    )
    .expect("write");

    loader
        .reload(&db, &[extra.clone()], &mut alloc)
        .expect("reload");

    db.with_snapshot(|snap| {
        let after_jdk = snap.jdk_index(app_project);
        assert!(
            std::sync::Arc::ptr_eq(&before_jdk.0, &after_jdk.0),
            "expected jdk index to be reused when config is unchanged"
        );

        let after_cp = snap
            .classpath_index(app_project)
            .expect("classpath index should be set");
        assert!(
            std::sync::Arc::ptr_eq(&before_cp.0, &after_cp.0),
            "expected classpath index to be reused when classpath is unchanged"
        );

        let files = snap.project_files(app_project);
        assert!(
            files
                .iter()
                .any(|&file| snap.file_rel_path(file).as_ref().ends_with("Extra.java")),
            "expected Extra.java to be added"
        );

        // Existing file ids and source-root assignments remain stable across reload.
        let app_file = files
            .iter()
            .copied()
            .find(|&file| snap.file_rel_path(file).as_ref().ends_with("App.java"))
            .expect("App.java id");
        assert_eq!(app_file, before_app_file);
        assert_eq!(snap.source_root(app_file), before_root);
    });
}

#[test]
fn reload_rebuilds_classpath_index_when_target_release_changes() {
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

    let before_cp = db.with_snapshot(|snap| {
        snap.classpath_index(app_project)
            .expect("classpath index should be set")
    });

    // Change the effective Java target release for the workspace and reload. This should
    // invalidate the cached classpath index even if the classpath entries themselves are
    // unchanged (multi-release selection depends on `--release`).
    let pom_path = root.join("pom.xml");
    let pom = fs::read_to_string(&pom_path).expect("read pom.xml");
    let pom = pom.replace(
        "<maven.compiler.target>17</maven.compiler.target>",
        "<maven.compiler.target>8</maven.compiler.target>",
    );
    fs::write(&pom_path, pom).expect("write pom.xml");

    loader.reload(&db, &[], &mut alloc).expect("reload");

    db.with_snapshot(|snap| {
        assert_eq!(snap.project_config(app_project).java.target.0, 8);

        let after_cp = snap
            .classpath_index(app_project)
            .expect("classpath index should be set");
        assert!(
            !std::sync::Arc::ptr_eq(&before_cp.0, &after_cp.0),
            "expected classpath index to be rebuilt when target release changes"
        );
    });
}

#[test]
fn java_language_level_is_project_scoped() {
    let root = fixture_root("maven-multi");
    let db = Database::new();

    let mut loader = WorkspaceLoader::new();
    let mut alloc = file_id_allocator();
    loader.load(&db, &root, &mut alloc).expect("load workspace");

    // Both module projects use Java 17 in the fixture.
    let app_project = loader
        .project_id_for_module("maven:com.example:app")
        .expect("app project");

    let file = db.with_snapshot(|snap| {
        snap.project_files(app_project)
            .iter()
            .copied()
            .find(|&file| snap.file_rel_path(file).as_ref().ends_with("App.java"))
            .expect("App.java")
    });

    // Override the module's language level to Java 8 and ensure feature diagnostics change.
    db.with_write(|db| {
        let mut cfg = (*db.project_config(app_project)).clone();
        cfg.java.source = nova_project::JavaVersion::JAVA_8;
        cfg.java.target = nova_project::JavaVersion::JAVA_8;
        db.set_project_config(app_project, Arc::new(cfg));
    });

    db.with_snapshot(|snap| {
        assert_eq!(snap.java_language_level(file).major, 8);
    });
}
