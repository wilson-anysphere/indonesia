use std::fs;

use nova_project::{
    load_workspace_model_with_options, BuildSystem, ClasspathEntryKind, LoadOptions,
};

#[test]
fn gradle_workspace_model_puts_cached_jars_on_module_path_for_jpms_projects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Fake Gradle user home with a minimal `modules-2` cache layout.
    let gradle_home = root.join("gradle-home");
    let cache_dir =
        gradle_home.join("caches/modules-2/files-2.1/com.example/foo/1.2.3/abcdef1234567890");
    fs::create_dir_all(&cache_dir).expect("mkdir gradle cache dir");

    let jar_path = cache_dir.join("foo-1.2.3.jar");
    fs::write(&jar_path, b"").expect("write jar");

    // Gradle workspace with JPMS enabled via module-info.java.
    let workspace = root.join("workspace");
    fs::create_dir_all(workspace.join("src/main/java")).expect("mkdir src");
    fs::write(workspace.join("settings.gradle"), "").expect("write settings.gradle");
    fs::write(
        workspace.join("build.gradle"),
        "dependencies { implementation 'com.example:foo:1.2.3' }",
    )
    .expect("write build.gradle");
    fs::write(
        workspace.join("src/main/java/module-info.java"),
        "module mod.a { requires foo; }",
    )
    .expect("write module-info.java");

    let options = LoadOptions {
        gradle_user_home: Some(gradle_home),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&workspace, &options).expect("load gradle workspace");

    assert_eq!(model.build_system, BuildSystem::Gradle);
    assert_eq!(model.modules.len(), 1);
    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should appear on module_path for JPMS workspaces"
    );
    assert!(
        !module
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == jar_path),
        "resolved jar should not remain on classpath for JPMS workspaces"
    );

    // Output directories should remain on the classpath.
    assert!(module.classpath.iter().any(|e| {
        e.kind == ClasspathEntryKind::Directory && e.path.ends_with("build/classes/java/main")
    }));
    assert!(module.classpath.iter().any(|e| {
        e.kind == ClasspathEntryKind::Directory && e.path.ends_with("build/classes/java/test")
    }));

    // Ensure determinism.
    let model2 =
        load_workspace_model_with_options(&workspace, &options).expect("reload gradle workspace");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}
