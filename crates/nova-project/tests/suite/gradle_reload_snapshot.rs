use std::fs;

use nova_project::{JavaVersion, LoadOptions};

#[test]
fn reload_project_reloads_on_gradle_snapshot_handoff_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Match the canonicalization behavior of `load_project_with_options` to avoid path mismatch
    // issues on macOS (`/var` vs `/private/var`) when we later pass `snapshot_path` to
    // `reload_project`.
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");

    // Ensure `detect_build_system`/`workspace_root` classify this as a Gradle workspace.
    fs::write(root.join("settings.gradle"), "").expect("settings.gradle");

    let build_gradle_path = root.join("build.gradle");
    fs::write(
        &build_gradle_path,
        "sourceCompatibility = JavaVersion.VERSION_11\n\
         targetCompatibility = JavaVersion.VERSION_11\n",
    )
    .expect("build.gradle");

    let snapshot_path = root.join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH);
    fs::create_dir_all(snapshot_path.parent().expect("snapshot parent")).expect("create .nova dir");
    fs::write(&snapshot_path, "{}").expect("write gradle snapshot");

    let mut options = LoadOptions::default();
    let config = nova_project::load_project_with_options(&root, &options).expect("load project");
    assert_eq!(config.java.source, JavaVersion(11));
    assert_eq!(config.java.target, JavaVersion(11));

    // Mutate `build.gradle` but report only the Gradle handoff file as changed.
    // If `.nova/queries/gradle.json` is treated as a build file, this should still reload and
    // pick up the updated Java config.
    fs::write(
        &build_gradle_path,
        "sourceCompatibility = JavaVersion.VERSION_21\n\
         targetCompatibility = JavaVersion.VERSION_21\n",
    )
    .expect("update build.gradle");

    let reloaded = nova_project::reload_project(&config, &mut options, &[snapshot_path])
        .expect("reload project");
    assert_eq!(reloaded.java.source, JavaVersion(21));
    assert_eq!(reloaded.java.target, JavaVersion(21));
}

#[cfg(unix)]
#[test]
fn reload_project_reloads_on_gradle_snapshot_handoff_change_when_changed_path_uses_symlink_root() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let real_root = tmp.path().join("real");
    fs::create_dir_all(&real_root).expect("mkdir real root");
    let link_root = tmp.path().join("link");
    symlink(&real_root, &link_root).expect("symlink root");

    // Ensure `detect_build_system`/`workspace_root` classify this as a Gradle workspace.
    fs::write(link_root.join("settings.gradle"), "").expect("settings.gradle");

    let build_gradle_path = link_root.join("build.gradle");
    fs::write(
        &build_gradle_path,
        "sourceCompatibility = JavaVersion.VERSION_11\n\
         targetCompatibility = JavaVersion.VERSION_11\n",
    )
    .expect("build.gradle");

    let snapshot_path = link_root.join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH);
    fs::create_dir_all(snapshot_path.parent().expect("snapshot parent")).expect("create .nova dir");
    fs::write(&snapshot_path, "{}").expect("write gradle snapshot");

    let mut options = LoadOptions::default();
    let config = nova_project::load_project_with_options(&link_root, &options).expect("load project");
    assert_eq!(config.java.source, JavaVersion(11));
    assert_eq!(config.java.target, JavaVersion(11));

    // Mutate `build.gradle` but report only the Gradle handoff file as changed.
    fs::write(
        config.workspace_root.join("build.gradle"),
        "sourceCompatibility = JavaVersion.VERSION_21\n\
         targetCompatibility = JavaVersion.VERSION_21\n",
    )
    .expect("update build.gradle");

    // The changed path uses the symlink root, which should still be recognized as workspace-local.
    let reloaded = nova_project::reload_project(&config, &mut options, &[snapshot_path])
        .expect("reload project");
    assert_eq!(reloaded.java.source, JavaVersion(21));
    assert_eq!(reloaded.java.target, JavaVersion(21));
}
