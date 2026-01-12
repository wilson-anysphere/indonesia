use std::path::Path;

use nova_project::{load_project_with_options, reload_project, BuildSystem, LoadOptions};

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, contents).expect("write file");
}

#[test]
fn reload_project_rescans_on_maven_build_markers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_file(
        &root.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1</version>
</project>
"#,
    );

    let repo_dir = tempfile::tempdir().expect("tempdir");
    let mut options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(root, &options).expect("load project");
    assert_eq!(config.build_system, BuildSystem::Maven);

    // Start without any standard Maven source roots so we can detect a rescan.
    let main_src = config.workspace_root.join("src/main/java");
    assert!(
        !config.source_roots.iter().any(|sr| sr.path == main_src),
        "unexpected src/main/java root in initial config"
    );

    // Create a new source root; it will only be picked up if `reload_project` takes the rescan
    // branch.
    std::fs::create_dir_all(&main_src).expect("create src/main/java");

    // Simulate a watcher reporting a Maven wrapper/config change (not just `pom.xml`).
    let mvn_cfg = config.workspace_root.join(".mvn/maven.config");
    write_file(&mvn_cfg, "-DskipTests\n");

    let next = reload_project(&config, &mut options, &[mvn_cfg]).expect("reload project");
    assert!(
        next.source_roots.iter().any(|sr| sr.path == main_src),
        "expected rescan to pick up src/main/java"
    );
}

#[test]
fn reload_project_rescans_on_gradle_build_markers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_file(&root.join("settings.gradle"), "include ':app'\n");
    write_file(&root.join("build.gradle"), "\n");

    let gradle_home = tempfile::tempdir().expect("tempdir");
    let mut options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(root, &options).expect("load project");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert!(
        !config
            .modules
            .iter()
            .any(|m| m.root == config.workspace_root.join("lib")),
        "unexpected lib module in initial config"
    );

    // Update settings.gradle to include a new module.
    write_file(
        &config.workspace_root.join("settings.gradle"),
        "include ':app', ':lib'\n",
    );

    // Simulate a wrapper change being the only "build file" event we received.
    let wrapper_props = config
        .workspace_root
        .join("gradle/wrapper/gradle-wrapper.properties");
    write_file(
        &wrapper_props,
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.0-bin.zip\n",
    );

    let next = reload_project(&config, &mut options, &[wrapper_props]).expect("reload project");
    assert!(
        next.modules
            .iter()
            .any(|m| m.root == next.workspace_root.join("lib")),
        "expected rescan to pick up the new :lib module"
    );
}

#[test]
fn reload_project_rescans_on_bazel_build_markers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_file(&root.join("WORKSPACE"), "\n");

    let mut options = LoadOptions::default();
    let config = load_project_with_options(root, &options).expect("load project");
    assert_eq!(config.build_system, BuildSystem::Bazel);

    let new_pkg = config.workspace_root.join("foo");
    assert!(
        !config.source_roots.iter().any(|sr| sr.path == new_pkg),
        "unexpected foo source root in initial config"
    );

    // Add a new Bazel package (BUILD file). We will ensure the reload only happens when the
    // changed file is a Bazel config marker (e.g. `.bazelrc`).
    write_file(&new_pkg.join("BUILD"), "\n");

    let bazelrc = config.workspace_root.join(".bazelrc");
    write_file(&bazelrc, "build --announce_rc\n");

    let next = reload_project(&config, &mut options, &[bazelrc]).expect("reload project");
    assert!(
        next.source_roots.iter().any(|sr| sr.path == new_pkg),
        "expected rescan to pick up the new Bazel package root"
    );
}

#[test]
fn gradle_wrapper_properties_is_path_aware() {
    // Ensure we don't treat arbitrary `gradle-wrapper.properties` files as build markers unless
    // they are under `gradle/wrapper/`.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let bogus = root.join("some/other/gradle-wrapper.properties");
    write_file(
        &bogus,
        "distributionUrl=https://example.invalid/gradle.zip\n",
    );

    // Intentionally set up a "workspace root" that does not contain any Gradle markers. If the
    // wrapper file is incorrectly treated as build-affecting (despite being in the wrong
    // location), `reload_project` would attempt a rescan and fail `detect_build_system`.
    let config = nova_project::ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Gradle,
        java: Default::default(),
        modules: Vec::new(),
        jpms_modules: Vec::new(),
        jpms_workspace: None,
        source_roots: Vec::new(),
        module_path: Vec::new(),
        classpath: Vec::new(),
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    };

    let mut options = LoadOptions::default();
    let next = reload_project(&config, &mut options, &[bogus]).expect("reload project");
    assert_eq!(next, config);
}
