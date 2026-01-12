use std::collections::BTreeSet;
use std::path::PathBuf;

use nova_project::{load_project_with_options, BuildSystem, LoadOptions};
use tempfile::tempdir;

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

#[test]
fn extracts_gradle_version_catalog_libraries() {
    let root = testdata_path("gradle-version-catalog");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| {
            (
                d.group_id.as_str(),
                d.artifact_id.as_str(),
                d.version.as_deref(),
            )
        })
        .collect();

    assert!(deps.contains(&("com.google.guava", "guava", Some("33.0.0-jre"))));
    assert!(deps.contains(&("org.slf4j", "slf4j-api", Some("2.0.12"))));

    let config2 = load_project_with_options(&root, &options).expect("load gradle project again");
    assert_eq!(config, config2);
}

#[test]
fn extracts_gradle_version_catalog_bundles() {
    let root = testdata_path("gradle-version-catalog");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| {
            (
                d.group_id.as_str(),
                d.artifact_id.as_str(),
                d.version.as_deref(),
            )
        })
        .collect();

    // `junit-jupiter-api` is only referenced through a version catalog bundle.
    assert!(deps.contains(&("org.junit.jupiter", "junit-jupiter-api", Some("5.10.0"))));
}
