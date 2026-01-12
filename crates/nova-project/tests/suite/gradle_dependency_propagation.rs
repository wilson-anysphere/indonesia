use std::collections::BTreeSet;
use std::path::PathBuf;

use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    LoadOptions,
};
use tempfile::tempdir;

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

#[test]
fn gradle_workspace_model_propagates_project_dependencies_transitively_into_classpath() {
    let root = testdata_path("gradle-project-deps-transitive");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    let app = model.module_by_id("gradle::app").expect("app module");
    let dirs: BTreeSet<_> = app
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Directory)
        .map(|cp| {
            cp.path
                .strip_prefix(&model.workspace_root)
                .unwrap()
                .to_path_buf()
        })
        .collect();

    assert!(
        dirs.contains(&PathBuf::from("lib/build/classes/java/main")),
        "expected app classpath to contain lib/build/classes/java/main"
    );
    assert!(
        dirs.contains(&PathBuf::from("core/build/classes/java/main")),
        "expected app classpath to contain core/build/classes/java/main"
    );
}

#[test]
fn gradle_root_subprojects_dependencies_propagate_into_modules() {
    let root = testdata_path("gradle-root-subprojects-deps");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert!(
        config.dependencies.iter().any(|d| {
            d.group_id == "com.google.guava"
                && d.artifact_id == "guava"
                && d.version.as_deref() == Some("33.0.0-jre")
        }),
        "expected project dependency list to include guava from root subprojects block"
    );

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    for id in ["gradle::app", "gradle::lib"] {
        let module = model.module_by_id(id).expect("module");
        assert!(
            module.dependencies.iter().any(|d| {
                d.group_id == "com.google.guava"
                    && d.artifact_id == "guava"
                    && d.version.as_deref() == Some("33.0.0-jre")
            }),
            "expected {id} to include guava from root subprojects block"
        );
    }
}

