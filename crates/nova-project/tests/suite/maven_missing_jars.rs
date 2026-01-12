use std::fs;
use std::path::Path;

use nova_project::{
    load_project_with_options, load_workspace_model_with_options, ClasspathEntryKind, LoadOptions,
};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

#[test]
fn maven_project_omits_missing_dependency_jars_until_present_on_disk() {
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
              <dependencies>
                <dependency>
                  <groupId>com.example</groupId>
                  <artifactId>dep</artifactId>
                  <version>1.0</version>
                </dependency>
              </dependencies>
            </project>
        "#,
    );

    write(
        &root.join("src/main/java/com/example/Main.java"),
        "package com.example; class Main {}",
    );

    let tmp_repo = tempfile::tempdir().expect("tempdir maven repo");
    let expected_jar = tmp_repo.path().join("com/example/dep/1.0/dep-1.0.jar");
    assert!(
        !expected_jar.is_file(),
        "jar should not exist for this test"
    );

    let options = LoadOptions {
        maven_repo: Some(tmp_repo.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(root, &options).expect("load maven project");

    assert!(
        config.dependencies.iter().any(|d| {
            d.group_id == "com.example"
                && d.artifact_id == "dep"
                && d.version.as_deref() == Some("1.0")
        }),
        "expected dependency coordinates to be discovered even when jars are missing, got: {:#?}",
        config.dependencies
    );

    assert!(
        !config
            .classpath
            .iter()
            .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == expected_jar),
        "missing jar should not be added to classpath: {}",
        expected_jar.display()
    );
    assert!(
        !config
            .module_path
            .iter()
            .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == expected_jar),
        "missing jar should not be added to module-path: {}",
        expected_jar.display()
    );

    // Creating the jar should cause it to be added to the classpath on reload.
    fs::create_dir_all(expected_jar.parent().expect("jar parent")).expect("mkdir jar parent");
    fs::write(&expected_jar, b"").expect("write jar placeholder");

    let config2 = load_project_with_options(root, &options).expect("reload maven project");
    assert!(
        config2
            .classpath
            .iter()
            .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == expected_jar),
        "expected jar path to appear on classpath after creation: {}",
        expected_jar.display()
    );
    assert!(
        !config2
            .module_path
            .iter()
            .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == expected_jar),
        "jar should not be added to module-path for non-JPMS projects: {}",
        expected_jar.display()
    );
}

#[test]
fn maven_workspace_model_omits_missing_dependency_jars_until_present_on_disk() {
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
              <dependencies>
                <dependency>
                  <groupId>com.example</groupId>
                  <artifactId>dep</artifactId>
                  <version>1.0</version>
                </dependency>
              </dependencies>
            </project>
        "#,
    );

    write(
        &root.join("src/main/java/com/example/Main.java"),
        "package com.example; class Main {}",
    );

    let tmp_repo = tempfile::tempdir().expect("tempdir maven repo");
    let expected_jar = tmp_repo.path().join("com/example/dep/1.0/dep-1.0.jar");
    assert!(
        !expected_jar.is_file(),
        "jar should not exist for this test"
    );

    let options = LoadOptions {
        maven_repo: Some(tmp_repo.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(root, &options).expect("load maven workspace model");

    for module in &model.modules {
        assert!(
            !module
                .classpath
                .iter()
                .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == expected_jar),
            "missing jar should not be added to module classpath ({}): {}",
            module.id,
            expected_jar.display(),
        );
        assert!(
            !module
                .module_path
                .iter()
                .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == expected_jar),
            "missing jar should not be added to module module-path ({}): {}",
            module.id,
            expected_jar.display(),
        );
    }

    // Creating the jar should cause it to be added to module classpaths on reload.
    fs::create_dir_all(expected_jar.parent().expect("jar parent")).expect("mkdir jar parent");
    fs::write(&expected_jar, b"").expect("write jar placeholder");

    let model2 =
        load_workspace_model_with_options(root, &options).expect("reload maven workspace model");
    for module in &model2.modules {
        assert!(
            module
                .classpath
                .iter()
                .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == expected_jar),
            "expected jar path to appear on module classpath after creation ({}): {}",
            module.id,
            expected_jar.display(),
        );
        assert!(
            !module
                .module_path
                .iter()
                .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == expected_jar),
            "jar should not be placed on module-path for non-JPMS modules ({}): {}",
            module.id,
            expected_jar.display(),
        );
    }
}

#[test]
fn maven_project_accepts_exploded_dependency_directory_as_classpath_dir() {
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
              <dependencies>
                <dependency>
                  <groupId>com.example</groupId>
                  <artifactId>dep</artifactId>
                  <version>1.0</version>
                </dependency>
              </dependencies>
            </project>
        "#,
    );

    write(
        &root.join("src/main/java/com/example/Main.java"),
        "package com.example; class Main {}",
    );

    let tmp_repo = tempfile::tempdir().expect("tempdir maven repo");
    let expected_jar_dir = tmp_repo.path().join("com/example/dep/1.0/dep-1.0.jar");
    fs::create_dir_all(&expected_jar_dir).expect("mkdir exploded jar dir");
    assert!(
        expected_jar_dir.is_dir(),
        "expected jar path to be a directory"
    );

    let options = LoadOptions {
        maven_repo: Some(tmp_repo.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(root, &options).expect("load maven project");

    assert!(
        config
            .classpath
            .iter()
            .any(|e| e.kind == ClasspathEntryKind::Directory && e.path == expected_jar_dir),
        "expected exploded jar directory to be added to classpath as a Directory entry"
    );
    assert!(
        !config
            .module_path
            .iter()
            .any(|e| e.path == expected_jar_dir),
        "non-JPMS projects should not place exploded jars on module-path"
    );
}

#[test]
fn maven_workspace_model_accepts_exploded_dependency_directory_as_classpath_dir() {
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
              <dependencies>
                <dependency>
                  <groupId>com.example</groupId>
                  <artifactId>dep</artifactId>
                  <version>1.0</version>
                </dependency>
              </dependencies>
            </project>
        "#,
    );

    write(
        &root.join("src/main/java/com/example/Main.java"),
        "package com.example; class Main {}",
    );

    let tmp_repo = tempfile::tempdir().expect("tempdir maven repo");
    let expected_jar_dir = tmp_repo.path().join("com/example/dep/1.0/dep-1.0.jar");
    fs::create_dir_all(&expected_jar_dir).expect("mkdir exploded jar dir");
    assert!(
        expected_jar_dir.is_dir(),
        "expected jar path to be a directory"
    );

    let options = LoadOptions {
        maven_repo: Some(tmp_repo.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(root, &options).expect("load maven workspace model");

    for module in &model.modules {
        assert!(
            module
                .classpath
                .iter()
                .any(|e| e.kind == ClasspathEntryKind::Directory && e.path == expected_jar_dir),
            "expected exploded jar directory to be added to module classpath as a Directory entry ({})",
            module.id
        );
        assert!(
            !module
                .module_path
                .iter()
                .any(|e| e.path == expected_jar_dir),
            "non-JPMS modules should not place exploded jars on module-path ({})",
            module.id
        );
    }
}
