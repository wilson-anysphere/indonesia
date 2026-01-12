use std::fs;

use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    LoadOptions,
};

#[test]
fn resolves_gradle_dependency_jars_from_local_cache() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmp_root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Fake Gradle user home with a minimal `modules-2` cache layout.
    let gradle_home = tmp_root.join("gradle-home");
    let cache_dir =
        gradle_home.join("caches/modules-2/files-2.1/com.example/foo/1.2.3/abcdef1234567890");
    fs::create_dir_all(&cache_dir).expect("mkdir gradle cache dir");

    let jar_path = cache_dir.join("foo-1.2.3.jar");
    fs::write(&jar_path, b"").expect("write jar");

    // Ensure we don't accidentally include auxiliary jars.
    let sources_path = cache_dir.join("foo-1.2.3-sources.jar");
    fs::write(&sources_path, b"").expect("write sources jar");

    let workspace = tmp_root.join("workspace");
    fs::create_dir_all(workspace.join("src/main/java")).expect("mkdir src");
    fs::write(
        workspace.join("build.gradle"),
        "dependencies { implementation 'com.example:foo:1.2.3' }",
    )
    .expect("write build.gradle");

    let options = LoadOptions {
        gradle_user_home: Some(gradle_home),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&workspace, &options).expect("load gradle project");

    assert!(
        config
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should appear on the project classpath"
    );
    assert!(
        !config
            .classpath
            .iter()
            .any(|entry| entry.path == sources_path),
        "sources jar should be excluded"
    );
}

#[test]
fn resolves_gradle_dependency_jars_onto_module_path_for_jpms_workspace_model() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmp_root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Fake Gradle user home with a minimal `modules-2` cache layout.
    let gradle_home = tmp_root.join("gradle-home");
    let cache_dir =
        gradle_home.join("caches/modules-2/files-2.1/com.example/foo/1.2.3/abcdef1234567890");
    fs::create_dir_all(&cache_dir).expect("mkdir gradle cache dir");

    let jar_path = cache_dir.join("foo-1.2.3.jar");
    fs::write(&jar_path, b"").expect("write jar");

    let workspace = tmp_root.join("workspace");
    let src_dir = workspace.join("src/main/java");
    fs::create_dir_all(&src_dir).expect("mkdir src");
    fs::write(
        workspace.join("build.gradle"),
        "dependencies { implementation 'com.example:foo:1.2.3' }",
    )
    .expect("write build.gradle");
    fs::write(
        src_dir.join("module-info.java"),
        "module com.example.app { requires com.example.foo; }",
    )
    .expect("write module-info.java");

    let options = LoadOptions {
        gradle_user_home: Some(gradle_home),
        ..LoadOptions::default()
    };

    let model = load_workspace_model_with_options(&workspace, &options)
        .expect("load gradle workspace model");
    assert_eq!(model.build_system, BuildSystem::Gradle);
    assert_eq!(model.modules.len(), 1);
    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should appear on the module-path for JPMS workspaces"
    );
    assert!(
        !module.classpath.iter().any(|entry| entry.path == jar_path),
        "resolved jar should not remain on the classpath for JPMS workspaces"
    );

    // Output directories should remain on the classpath.
    assert!(module.classpath.iter().any(|entry| {
        entry.kind == ClasspathEntryKind::Directory
            && entry.path.ends_with("build/classes/java/main")
    }));
    assert!(module.classpath.iter().any(|entry| {
        entry.kind == ClasspathEntryKind::Directory
            && entry.path.ends_with("build/classes/java/test")
    }));
}

#[test]
fn resolves_gradle_dependency_jars_from_local_cache_with_map_notation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmp_root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Fake Gradle user home with a minimal `modules-2` cache layout.
    let gradle_home = tmp_root.join("gradle-home");
    let cache_dir =
        gradle_home.join("caches/modules-2/files-2.1/com.example/foo/1.2.3/abcdef1234567890");
    fs::create_dir_all(&cache_dir).expect("mkdir gradle cache dir");

    let jar_path = cache_dir.join("foo-1.2.3.jar");
    fs::write(&jar_path, b"").expect("write jar");

    let workspace = tmp_root.join("workspace");
    fs::create_dir_all(workspace.join("src/main/java")).expect("mkdir src");
    fs::write(
        workspace.join("build.gradle"),
        "dependencies { implementation group: 'com.example', name: 'foo', version: '1.2.3' }",
    )
    .expect("write build.gradle");

    let options = LoadOptions {
        gradle_user_home: Some(gradle_home),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&workspace, &options).expect("load gradle project");

    assert!(
        config
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should appear on the project classpath"
    );
}

#[test]
fn resolves_gradle_dependency_jars_from_local_cache_with_kotlin_named_args() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmp_root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Fake Gradle user home with a minimal `modules-2` cache layout.
    let gradle_home = tmp_root.join("gradle-home");
    let cache_dir =
        gradle_home.join("caches/modules-2/files-2.1/com.example/foo/1.2.3/abcdef1234567890");
    fs::create_dir_all(&cache_dir).expect("mkdir gradle cache dir");

    let jar_path = cache_dir.join("foo-1.2.3.jar");
    fs::write(&jar_path, b"").expect("write jar");

    let workspace = tmp_root.join("workspace");
    fs::create_dir_all(workspace.join("src/main/java")).expect("mkdir src");
    fs::write(
        workspace.join("build.gradle.kts"),
        r#"dependencies { implementation(group = "com.example", name = "foo", version = "1.2.3") }"#,
    )
    .expect("write build.gradle.kts");

    let options = LoadOptions {
        gradle_user_home: Some(gradle_home),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&workspace, &options).expect("load gradle project");

    assert!(
        config
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should appear on the project classpath"
    );
}

#[test]
fn resolves_gradle_dependency_jars_from_local_cache_with_version_catalog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmp_root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Fake Gradle user home with a minimal `modules-2` cache layout.
    let gradle_home = tmp_root.join("gradle-home");
    let cache_dir =
        gradle_home.join("caches/modules-2/files-2.1/com.example/foo/1.2.3/abcdef1234567890");
    fs::create_dir_all(&cache_dir).expect("mkdir gradle cache dir");

    let jar_path = cache_dir.join("foo-1.2.3.jar");
    fs::write(&jar_path, b"").expect("write jar");

    let workspace = tmp_root.join("workspace");
    fs::create_dir_all(workspace.join("src/main/java")).expect("mkdir src");
    fs::create_dir_all(workspace.join("gradle")).expect("mkdir gradle dir");
    fs::write(
        workspace.join("gradle/libs.versions.toml"),
        r#"
[versions]
foo = "1.2.3"

[libraries]
foo = { module = "com.example:foo", version.ref = "foo" }
"#,
    )
    .expect("write libs.versions.toml");
    fs::write(
        workspace.join("build.gradle.kts"),
        r#"dependencies { implementation(libs.foo) }"#,
    )
    .expect("write build.gradle.kts");

    let options = LoadOptions {
        gradle_user_home: Some(gradle_home),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&workspace, &options).expect("load gradle project");

    assert!(
        config
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should appear on the project classpath"
    );
}

#[test]
fn resolves_gradle_dependency_jars_from_local_cache_with_version_catalog_get() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmp_root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Fake Gradle user home with a minimal `modules-2` cache layout.
    let gradle_home = tmp_root.join("gradle-home");
    let cache_dir =
        gradle_home.join("caches/modules-2/files-2.1/org.slf4j/slf4j-api/2.0.12/abcdef1234567890");
    fs::create_dir_all(&cache_dir).expect("mkdir gradle cache dir");

    let jar_path = cache_dir.join("slf4j-api-2.0.12.jar");
    fs::write(&jar_path, b"").expect("write jar");

    let workspace = tmp_root.join("workspace");
    fs::create_dir_all(workspace.join("src/main/java")).expect("mkdir src");
    fs::create_dir_all(workspace.join("gradle")).expect("mkdir gradle dir");
    fs::write(
        workspace.join("gradle/libs.versions.toml"),
        r#"
[versions]
slf4j = "2.0.12"

[libraries]
slf4j-api = { group = "org.slf4j", name = "slf4j-api", version.ref = "slf4j" }
"#,
    )
    .expect("write libs.versions.toml");
    fs::write(
        workspace.join("build.gradle.kts"),
        r#"dependencies { implementation(libs.slf4j.api.get()) }"#,
    )
    .expect("write build.gradle.kts");

    let options = LoadOptions {
        gradle_user_home: Some(gradle_home),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&workspace, &options).expect("load gradle project");

    assert!(
        config
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should appear on the project classpath"
    );
}

#[test]
fn resolves_gradle_dependency_jars_from_local_cache_with_version_catalog_bundle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmp_root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Fake Gradle user home with a minimal `modules-2` cache layout.
    let gradle_home = tmp_root.join("gradle-home");
    let cache_dir = gradle_home.join(
        "caches/modules-2/files-2.1/org.junit.jupiter/junit-jupiter-api/5.10.0/abcdef1234567890",
    );
    fs::create_dir_all(&cache_dir).expect("mkdir gradle cache dir");

    let jar_path = cache_dir.join("junit-jupiter-api-5.10.0.jar");
    fs::write(&jar_path, b"").expect("write jar");

    let workspace = tmp_root.join("workspace");
    fs::create_dir_all(workspace.join("src/main/java")).expect("mkdir src");
    fs::create_dir_all(workspace.join("gradle")).expect("mkdir gradle dir");
    fs::write(
        workspace.join("gradle/libs.versions.toml"),
        r#"
[libraries]
junit-jupiter-api = { module = "org.junit.jupiter:junit-jupiter-api", version = "5.10.0" }

[bundles]
test-libs = ["junit-jupiter-api"]
"#,
    )
    .expect("write libs.versions.toml");
    fs::write(
        workspace.join("build.gradle.kts"),
        r#"dependencies { testImplementation(libs.bundles.test.libs) }"#,
    )
    .expect("write build.gradle.kts");

    let options = LoadOptions {
        gradle_user_home: Some(gradle_home),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&workspace, &options).expect("load gradle project");

    assert!(
        config
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should appear on the project classpath"
    );
}
