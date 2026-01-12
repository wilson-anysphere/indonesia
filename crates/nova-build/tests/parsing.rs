use nova_build::{collect_gradle_build_files, collect_maven_build_files, BuildFileFingerprint};

#[test]
fn fingerprint_changes_on_gradle_version_catalog_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();

    let gradle_dir = root.join("gradle");
    std::fs::create_dir_all(&gradle_dir).unwrap();
    let catalog = gradle_dir.join("libs.versions.toml");
    std::fs::write(&catalog, "[versions]\nfoo = \"1.0\"\n").unwrap();

    let fp1 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();
    std::fs::write(&catalog, "[versions]\nfoo = \"1.1\"\n").unwrap();
    let fp2 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_applied_gradle_script_plugin_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("build.gradle"),
        "plugins { id 'java' }\napply from: 'dependencies.gradle'\n",
    )
    .unwrap();

    let script_plugin = root.join("dependencies.gradle");
    std::fs::write(&script_plugin, "ext.foo = 1\n").unwrap();

    let fp1 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();
    std::fs::write(&script_plugin, "ext.foo = 2\n").unwrap();
    let fp2 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_maven_jvm_config_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let mvn_dir = root.join(".mvn");
    std::fs::create_dir_all(&mvn_dir).unwrap();
    let config = mvn_dir.join("jvm.config");
    std::fs::write(&config, "-Xmx2g\n").unwrap();

    let fp1 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();
    std::fs::write(&config, "-Xmx4g\n").unwrap();
    let fp2 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}
