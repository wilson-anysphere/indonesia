use std::fs;
use std::path::Path;

use nova_project::{load_project, load_project_with_options, BuildSystem, LoadOptions};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

#[test]
fn load_project_finds_maven_workspace_root_from_nested_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write(
        &root.join("pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>demo</artifactId>
              <version>0.0.1</version>
              <packaging>pom</packaging>
              <modules>
                <module>app</module>
                <module>lib</module>
              </modules>
            </project>
        "#,
    );

    write(
        &root.join("app/pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>app</artifactId>
              <version>0.0.1</version>
            </project>
        "#,
    );
    write(
        &root.join("lib/pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>lib</artifactId>
              <version>0.0.1</version>
            </project>
        "#,
    );

    write(
        &root.join("app/src/main/java/com/example/App.java"),
        "package com.example; class App {}",
    );
    write(
        &root.join("lib/src/main/java/com/example/Lib.java"),
        "package com.example; class Lib {}",
    );

    let expected_root = fs::canonicalize(root).expect("canonicalize root");
    let nested = root.join("app/src/main/java/com/example/App.java");

    let repo_dir = tempfile::tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config =
        load_project_with_options(&nested, &options).expect("load project from nested file");
    assert_eq!(config.build_system, BuildSystem::Maven);
    assert_eq!(config.workspace_root, expected_root);
    // The root pom here is an aggregator (`<packaging>pom</packaging>`), so we
    // only expect the child modules.
    assert_eq!(config.modules.len(), 2);
}

#[test]
fn load_project_finds_gradle_workspace_root_from_nested_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write(&root.join("settings.gradle"), r#"include("app", "lib")"#);
    write(&root.join("build.gradle"), "// root build");

    write(&root.join("app/build.gradle"), "// app build");
    write(&root.join("lib/build.gradle"), "// lib build");

    write(
        &root.join("app/src/main/java/com/example/App.java"),
        "package com.example; class App {}",
    );
    write(
        &root.join("lib/src/main/java/com/example/Lib.java"),
        "package com.example; class Lib {}",
    );

    let expected_root = fs::canonicalize(root).expect("canonicalize root");
    let nested = root.join("lib/src/main/java/com/example/Lib.java");

    let gradle_home = tempfile::tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config =
        load_project_with_options(&nested, &options).expect("load project from nested file");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.workspace_root, expected_root);
    assert_eq!(config.modules.len(), 2);
}

#[test]
fn load_project_finds_gradle_workspace_root_from_buildsrc_nested_file_without_settings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // No `settings.gradle`: Gradle workspace root detection falls back to the nearest `build.gradle`.
    // Ensure `buildSrc/build.gradle` does not "steal" the workspace root when loading a file under
    // `buildSrc/**`.
    write(&root.join("build.gradle"), "// root build");
    write(&root.join("buildSrc/build.gradle"), "// buildSrc build");

    write(
        &root.join("buildSrc/src/main/java/com/example/BuildLogic.java"),
        "package com.example; class BuildLogic {}",
    );

    let expected_root = fs::canonicalize(root).expect("canonicalize root");
    let nested = root.join("buildSrc/src/main/java/com/example/BuildLogic.java");

    let config = load_project(&nested).expect("load project from buildSrc nested file");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.workspace_root, expected_root);
    assert!(
        config.modules.iter().any(|m| m.root.ends_with("buildSrc")),
        "expected buildSrc to be loaded as a module"
    );
}

#[test]
fn load_project_finds_bazel_workspace_root_from_nested_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write(&root.join("WORKSPACE"), "# workspace");
    write(&root.join("java/com/example/BUILD"), "# build");
    write(
        &root.join("java/com/example/Example.java"),
        "package com.example; class Example {}",
    );

    let expected_root = fs::canonicalize(root).expect("canonicalize root");
    let nested = root.join("java/com/example/Example.java");

    let options = LoadOptions::default();
    let config =
        load_project_with_options(&nested, &options).expect("load project from nested file");
    assert_eq!(config.build_system, BuildSystem::Bazel);
    assert_eq!(config.workspace_root, expected_root);
}
