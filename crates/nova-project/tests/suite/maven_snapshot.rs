use nova_project::{
    load_project_with_options, load_workspace_model_with_options, ClasspathEntryKind, LoadOptions,
};
use tempfile::tempdir;

#[test]
fn resolves_timestamped_snapshot_jars_from_metadata() {
    let repo = tempdir().expect("temp repo");
    let repo_root = repo.path();

    let version_dir = repo_root.join("com/example/dep/1.0-SNAPSHOT");
    std::fs::create_dir_all(&version_dir).expect("mkdir dep version dir");

    // Typical timestamped SNAPSHOT jar filename.
    let jar_name = "dep-1.0-20260112.123456-1.jar";
    let jar_path = version_dir.join(jar_name);
    std::fs::write(&jar_path, b"").expect("write jar placeholder");

    // Minimal Maven metadata that maps `1.0-SNAPSHOT` -> `1.0-20260112.123456-1`.
    let metadata = r#"
        <metadata>
          <groupId>com.example</groupId>
          <artifactId>dep</artifactId>
          <version>1.0-SNAPSHOT</version>
          <versioning>
            <snapshotVersions>
              <snapshotVersion>
                <extension>jar</extension>
                <value>1.0-20260112.123456-1</value>
                <updated>20260112123456</updated>
              </snapshotVersion>
            </snapshotVersions>
          </versioning>
        </metadata>
    "#;
    std::fs::write(version_dir.join("maven-metadata-local.xml"), metadata).expect("write metadata");

    let workspace = tempdir().expect("temp workspace");
    let root = workspace.path();

    let pom = r#"
        <project>
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId>
          <artifactId>app</artifactId>
          <version>1.0</version>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId>
              <artifactId>dep</artifactId>
              <version>1.0-SNAPSHOT</version>
            </dependency>
          </dependencies>
        </project>
    "#;
    std::fs::write(root.join("pom.xml"), pom).expect("write pom.xml");

    let options = LoadOptions {
        maven_repo: Some(repo_root.to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(root, &options).expect("load project");
    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();

    let resolved = jar_entries
        .iter()
        .find(|p| {
            p.to_string_lossy()
                .replace('\\', "/")
                .ends_with(&format!("com/example/dep/1.0-SNAPSHOT/{jar_name}"))
        })
        .expect("timestamped snapshot jar should be resolved from metadata");

    assert!(resolved.is_file(), "resolved jar should exist on disk");
}

#[test]
fn omits_timestamped_snapshot_jars_when_artifact_is_missing() {
    let repo = tempdir().expect("temp repo");
    let repo_root = repo.path();

    let version_dir = repo_root.join("com/example/dep/1.0-SNAPSHOT");
    std::fs::create_dir_all(&version_dir).expect("mkdir dep version dir");

    // Typical timestamped SNAPSHOT jar filename, but do not create it on disk.
    let jar_name = "dep-1.0-20260112.123456-1.jar";
    assert!(
        !version_dir.join(jar_name).exists(),
        "jar should not exist for this test"
    );

    // Minimal Maven metadata that maps `1.0-SNAPSHOT` -> `1.0-20260112.123456-1`.
    let metadata = r#"
        <metadata>
          <groupId>com.example</groupId>
          <artifactId>dep</artifactId>
          <version>1.0-SNAPSHOT</version>
          <versioning>
            <snapshotVersions>
              <snapshotVersion>
                <extension>jar</extension>
                <value>1.0-20260112.123456-1</value>
                <updated>20260112123456</updated>
              </snapshotVersion>
            </snapshotVersions>
          </versioning>
        </metadata>
    "#;
    std::fs::write(version_dir.join("maven-metadata-local.xml"), metadata).expect("write metadata");

    let workspace = tempdir().expect("temp workspace");
    let root = workspace.path();

    let pom = r#"
        <project>
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId>
          <artifactId>app</artifactId>
          <version>1.0</version>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId>
              <artifactId>dep</artifactId>
              <version>1.0-SNAPSHOT</version>
            </dependency>
          </dependencies>
        </project>
    "#;
    std::fs::write(root.join("pom.xml"), pom).expect("write pom.xml");

    let options = LoadOptions {
        maven_repo: Some(repo_root.to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(root, &options).expect("load project");
    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();

    assert!(
        jar_entries.is_empty(),
        "expected snapshot jar to be omitted when missing on disk, got: {jar_entries:?}"
    );
}

#[test]
fn prefers_newest_existing_timestamped_snapshot_jar_from_metadata() {
    let repo = tempdir().expect("temp repo");
    let repo_root = repo.path();

    let version_dir = repo_root.join("com/example/dep/1.0-SNAPSHOT");
    std::fs::create_dir_all(&version_dir).expect("mkdir dep version dir");

    let old_value = "1.0-20260112.123456-1";
    let old_jar_name = format!("dep-{old_value}.jar");
    let old_jar_path = version_dir.join(&old_jar_name);
    std::fs::write(&old_jar_path, b"").expect("write old jar placeholder");
    assert!(old_jar_path.is_file(), "old jar should exist on disk");

    let new_value = "1.0-20260113.123456-2";
    let new_jar_name = format!("dep-{new_value}.jar");
    assert!(
        !version_dir.join(&new_jar_name).exists(),
        "newest jar should not exist for this test"
    );

    // Metadata declares two timestamped snapshot versions; only one exists on disk. We should
    // resolve the newest *existing* jar, not the newest entry in metadata.
    let metadata = format!(
        r#"
        <metadata>
          <groupId>com.example</groupId>
          <artifactId>dep</artifactId>
          <version>1.0-SNAPSHOT</version>
          <versioning>
            <snapshotVersions>
              <snapshotVersion>
                <extension>jar</extension>
                <value>{old_value}</value>
                <updated>20260112123456</updated>
              </snapshotVersion>
              <snapshotVersion>
                <extension>jar</extension>
                <value>{new_value}</value>
                <updated>20260113123456</updated>
              </snapshotVersion>
            </snapshotVersions>
          </versioning>
        </metadata>
    "#
    );
    std::fs::write(version_dir.join("maven-metadata-local.xml"), metadata).expect("write metadata");

    let workspace = tempdir().expect("temp workspace");
    let root = workspace.path();

    let pom = r#"
        <project>
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId>
          <artifactId>app</artifactId>
          <version>1.0</version>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId>
              <artifactId>dep</artifactId>
              <version>1.0-SNAPSHOT</version>
            </dependency>
          </dependencies>
        </project>
    "#;
    std::fs::write(root.join("pom.xml"), pom).expect("write pom.xml");

    let options = LoadOptions {
        maven_repo: Some(repo_root.to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(root, &options).expect("load project");
    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();

    let expected_suffix = format!("com/example/dep/1.0-SNAPSHOT/{old_jar_name}");
    jar_entries
        .iter()
        .find(|p| {
            p.to_string_lossy()
                .replace('\\', "/")
                .ends_with(&expected_suffix)
        })
        .expect("expected newest existing snapshot jar to be resolved from metadata");

    let missing_suffix = format!("com/example/dep/1.0-SNAPSHOT/{new_jar_name}");
    assert!(
        !jar_entries.iter().any(|p| p
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with(&missing_suffix)),
        "did not expect missing jar to be added to classpath, got: {jar_entries:?}"
    );
}

#[test]
fn falls_back_to_conventional_snapshot_jar_when_timestamped_artifact_is_missing() {
    let repo = tempdir().expect("temp repo");
    let repo_root = repo.path();

    let version_dir = repo_root.join("com/example/dep/1.0-SNAPSHOT");
    std::fs::create_dir_all(&version_dir).expect("mkdir dep version dir");

    // Typical timestamped SNAPSHOT jar filename, but do not create it on disk.
    let timestamped_jar_name = "dep-1.0-20260112.123456-1.jar";
    assert!(
        !version_dir.join(timestamped_jar_name).exists(),
        "timestamped jar should not exist for this test"
    );

    // Create the conventional `*-SNAPSHOT.jar` artifact that some local repos use.
    let fallback_jar_name = "dep-1.0-SNAPSHOT.jar";
    let fallback_jar_path = version_dir.join(fallback_jar_name);
    std::fs::write(&fallback_jar_path, b"").expect("write fallback jar placeholder");

    // Maven metadata that resolves to the missing timestamped jar.
    let metadata = r#"
        <metadata>
          <groupId>com.example</groupId>
          <artifactId>dep</artifactId>
          <version>1.0-SNAPSHOT</version>
          <versioning>
            <snapshotVersions>
              <snapshotVersion>
                <extension>jar</extension>
                <value>1.0-20260112.123456-1</value>
                <updated>20260112123456</updated>
              </snapshotVersion>
            </snapshotVersions>
          </versioning>
        </metadata>
    "#;
    std::fs::write(version_dir.join("maven-metadata-local.xml"), metadata).expect("write metadata");

    let workspace = tempdir().expect("temp workspace");
    let root = workspace.path();

    let pom = r#"
        <project>
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId>
          <artifactId>app</artifactId>
          <version>1.0</version>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId>
              <artifactId>dep</artifactId>
              <version>1.0-SNAPSHOT</version>
            </dependency>
          </dependencies>
        </project>
    "#;
    std::fs::write(root.join("pom.xml"), pom).expect("write pom.xml");

    let options = LoadOptions {
        maven_repo: Some(repo_root.to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(root, &options).expect("load project");
    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();

    assert!(
        jar_entries.iter().any(|p| p == &fallback_jar_path),
        "expected conventional SNAPSHOT jar to be used as a fallback when timestamped artifact is missing, got: {jar_entries:?}"
    );
    assert!(
        !jar_entries.iter().any(|p| p.to_string_lossy().ends_with(timestamped_jar_name)),
        "did not expect missing timestamped SNAPSHOT jar to appear on classpath, got: {jar_entries:?}"
    );
}

#[test]
fn maven_workspace_model_falls_back_to_conventional_snapshot_jar_when_timestamped_artifact_is_missing(
) {
    let repo = tempdir().expect("temp repo");
    let repo_root = repo.path();

    let version_dir = repo_root.join("com/example/dep/1.0-SNAPSHOT");
    std::fs::create_dir_all(&version_dir).expect("mkdir dep version dir");

    // Typical timestamped SNAPSHOT jar filename, but do not create it on disk.
    let timestamped_jar_name = "dep-1.0-20260112.123456-1.jar";
    assert!(
        !version_dir.join(timestamped_jar_name).exists(),
        "timestamped jar should not exist for this test"
    );

    // Create the conventional `*-SNAPSHOT.jar` artifact that some local repos use.
    let fallback_jar_name = "dep-1.0-SNAPSHOT.jar";
    let fallback_jar_path = version_dir.join(fallback_jar_name);
    std::fs::write(&fallback_jar_path, b"").expect("write fallback jar placeholder");

    // Maven metadata that resolves to the missing timestamped jar.
    let metadata = r#"
        <metadata>
          <groupId>com.example</groupId>
          <artifactId>dep</artifactId>
          <version>1.0-SNAPSHOT</version>
          <versioning>
            <snapshotVersions>
              <snapshotVersion>
                <extension>jar</extension>
                <value>1.0-20260112.123456-1</value>
                <updated>20260112123456</updated>
              </snapshotVersion>
            </snapshotVersions>
          </versioning>
        </metadata>
    "#;
    std::fs::write(version_dir.join("maven-metadata-local.xml"), metadata).expect("write metadata");

    let workspace = tempdir().expect("temp workspace");
    let root = workspace.path();

    let pom = r#"
        <project>
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId>
          <artifactId>app</artifactId>
          <version>1.0</version>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId>
              <artifactId>dep</artifactId>
              <version>1.0-SNAPSHOT</version>
            </dependency>
          </dependencies>
        </project>
    "#;
    std::fs::write(root.join("pom.xml"), pom).expect("write pom.xml");

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("mkdir src/main/java");
    std::fs::write(
        src_dir.join("Main.java"),
        "package com.example; class Main {}",
    )
    .expect("write Main.java");

    let options = LoadOptions {
        maven_repo: Some(repo_root.to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(root, &options).expect("load maven workspace model");
    for module in &model.modules {
        assert!(
            module
                .classpath
                .iter()
                .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == fallback_jar_path),
            "expected conventional SNAPSHOT jar to be used as a fallback when timestamped artifact is missing, got: {:#?}",
            module.classpath
        );
        assert!(
            !module
                .classpath
                .iter()
                .any(|e| e.path.to_string_lossy().ends_with(timestamped_jar_name)),
            "did not expect missing timestamped SNAPSHOT jar to appear on module classpath, got: {:#?}",
            module.classpath
        );
    }
}
