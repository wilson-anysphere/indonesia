use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nova_project::{load_project_with_options, reload_project, LoadOptions, ProjectConfig};

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(path, contents).expect("write file");
}

fn create_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("create dir");
}

fn module_roots(config: &ProjectConfig) -> BTreeSet<PathBuf> {
    config
        .modules
        .iter()
        .map(|module| {
            module
                .root
                .strip_prefix(&config.workspace_root)
                .unwrap_or(&module.root)
                .to_path_buf()
        })
        .collect()
}

#[test]
fn reloads_gradle_project_when_script_plugin_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_file(
        &root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app")
"#,
    );
    write_file(&root.join("build.gradle"), "");
    create_dir(&root.join("app/src/main/java"));
    write_file(&root.join("dependencies.gradle"), "// initial\n");

    let mut options = LoadOptions::default();
    let config = load_project_with_options(root, &options).expect("load gradle project");

    assert_eq!(
        module_roots(&config),
        BTreeSet::from([PathBuf::from("app")])
    );

    // Simulate a change that should be picked up on reload.
    let workspace_root = &config.workspace_root;
    write_file(
        &workspace_root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app", ":lib")
"#,
    );
    create_dir(&workspace_root.join("lib/src/main/java"));
    write_file(&workspace_root.join("dependencies.gradle"), "// changed\n");

    let reloaded = reload_project(
        &config,
        &mut options,
        &[workspace_root.join("dependencies.gradle")],
    )
    .expect("reload project");

    assert_eq!(
        module_roots(&reloaded),
        BTreeSet::from([PathBuf::from("app"), PathBuf::from("lib")])
    );
}

#[test]
fn reloads_gradle_project_when_version_catalog_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_file(
        &root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app")
"#,
    );
    write_file(&root.join("build.gradle"), "");
    create_dir(&root.join("app/src/main/java"));
    write_file(&root.join("gradle/libs.versions.toml"), "# initial\n");

    let mut options = LoadOptions::default();
    let config = load_project_with_options(root, &options).expect("load gradle project");

    assert_eq!(
        module_roots(&config),
        BTreeSet::from([PathBuf::from("app")])
    );

    // Simulate a change that should be picked up on reload.
    let workspace_root = &config.workspace_root;
    write_file(
        &workspace_root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app", ":lib")
"#,
    );
    create_dir(&workspace_root.join("lib/src/main/java"));
    write_file(
        &workspace_root.join("gradle/libs.versions.toml"),
        "# changed\n",
    );

    let reloaded = reload_project(
        &config,
        &mut options,
        &[workspace_root.join("gradle/libs.versions.toml")],
    )
    .expect("reload project");

    assert_eq!(
        module_roots(&reloaded),
        BTreeSet::from([PathBuf::from("app"), PathBuf::from("lib")])
    );
}

#[test]
fn reloads_gradle_project_when_custom_version_catalog_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_file(
        &root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app")
"#,
    );
    write_file(&root.join("build.gradle"), "");
    create_dir(&root.join("app/src/main/java"));
    write_file(&root.join("gradle/deps.versions.toml"), "# initial\n");

    let mut options = LoadOptions::default();
    let config = load_project_with_options(root, &options).expect("load gradle project");

    assert_eq!(
        module_roots(&config),
        BTreeSet::from([PathBuf::from("app")])
    );

    // Simulate a change that should be picked up on reload.
    let workspace_root = &config.workspace_root;
    write_file(
        &workspace_root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app", ":lib")
"#,
    );
    create_dir(&workspace_root.join("lib/src/main/java"));
    write_file(
        &workspace_root.join("gradle/deps.versions.toml"),
        "# changed\n",
    );

    let reloaded = reload_project(
        &config,
        &mut options,
        &[workspace_root.join("gradle/deps.versions.toml")],
    )
    .expect("reload project");

    assert_eq!(
        module_roots(&reloaded),
        BTreeSet::from([PathBuf::from("app"), PathBuf::from("lib")])
    );
}

#[test]
fn reloads_gradle_project_when_wrapper_properties_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_file(
        &root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app")
"#,
    );
    write_file(&root.join("build.gradle"), "");
    create_dir(&root.join("app/src/main/java"));
    write_file(
        &root.join("gradle/wrapper/gradle-wrapper.properties"),
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.5-bin.zip\n",
    );

    let mut options = LoadOptions::default();
    let config = load_project_with_options(root, &options).expect("load gradle project");

    assert_eq!(
        module_roots(&config),
        BTreeSet::from([PathBuf::from("app")])
    );

    // Simulate a change that should be picked up on reload.
    let workspace_root = &config.workspace_root;
    write_file(
        &workspace_root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app", ":lib")
"#,
    );
    create_dir(&workspace_root.join("lib/src/main/java"));
    write_file(
        &workspace_root.join("gradle/wrapper/gradle-wrapper.properties"),
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.6-bin.zip\n",
    );

    let reloaded = reload_project(
        &config,
        &mut options,
        &[workspace_root.join("gradle/wrapper/gradle-wrapper.properties")],
    )
    .expect("reload project");

    assert_eq!(
        module_roots(&reloaded),
        BTreeSet::from([PathBuf::from("app"), PathBuf::from("lib")])
    );
}

#[test]
fn reloads_gradle_project_when_gradlew_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_file(
        &root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app")
"#,
    );
    write_file(&root.join("build.gradle"), "");
    write_file(&root.join("gradlew"), "#!/bin/sh\n");
    create_dir(&root.join("app/src/main/java"));

    let mut options = LoadOptions::default();
    let config = load_project_with_options(root, &options).expect("load gradle project");

    assert_eq!(
        module_roots(&config),
        BTreeSet::from([PathBuf::from("app")])
    );

    // Simulate a change that should be picked up on reload.
    let workspace_root = &config.workspace_root;
    write_file(
        &workspace_root.join("settings.gradle"),
        r#"
rootProject.name = "demo"
include(":app", ":lib")
"#,
    );
    create_dir(&workspace_root.join("lib/src/main/java"));
    write_file(&workspace_root.join("gradlew"), "#!/bin/sh\n# changed\n");

    let reloaded = reload_project(&config, &mut options, &[workspace_root.join("gradlew")])
        .expect("reload project");

    assert_eq!(
        module_roots(&reloaded),
        BTreeSet::from([PathBuf::from("app"), PathBuf::from("lib")])
    );
}
