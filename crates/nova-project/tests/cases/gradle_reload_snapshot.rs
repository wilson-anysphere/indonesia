use std::fs;

use nova_project::{JavaVersion, LoadOptions};

#[test]
fn reload_project_reloads_on_gradle_snapshot_handoff_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

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
    let config = nova_project::load_project_with_options(root, &options).expect("load project");
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
