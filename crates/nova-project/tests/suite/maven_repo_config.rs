use std::fs;
use std::path::{Path, PathBuf};

use nova_project::{load_project_with_options, ClasspathEntryKind, LoadOptions};
use tempfile::tempdir;

fn write_pom_xml(workspace_root: &Path) {
    fs::write(
        workspace_root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>

  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>33.0.0-jre</version>
    </dependency>
  </dependencies>
</project>
"#,
    )
    .unwrap();
}

fn touch_guava_jar(repo: &Path) {
    let jar = repo.join("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar");
    fs::create_dir_all(jar.parent().expect("jar parent")).expect("mkdir jar parent");
    fs::write(&jar, b"not really a jar").expect("write fake jar");
}

fn assert_jar_entries_are_under_repo(jar_entries: &[PathBuf], repo_root: &Path) {
    let repo_root_canon = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    assert!(
        jar_entries.iter().all(|jar| {
            let jar_canon = fs::canonicalize(jar).unwrap_or_else(|_| jar.clone());
            jar_canon.starts_with(&repo_root_canon)
        }),
        "expected jar paths to start with repo {repo_root:?} (canonicalized to {repo_root_canon:?}), got: {jar_entries:?}"
    );
}

#[test]
fn loads_maven_repo_from_mvn_maven_config_and_allows_override() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();

    write_pom_xml(workspace_root);

    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_dir = tempdir().unwrap();
    let repo_path: PathBuf = repo_dir.path().to_path_buf();
    touch_guava_jar(&repo_path);
    fs::write(
        workspace_root.join(".mvn/maven.config"),
        format!("-Dmaven.repo.local={}", repo_path.display()),
    )
    .unwrap();
    let config = load_project_with_options(
        workspace_root,
        &LoadOptions {
            maven_repo: None,
            ..Default::default()
        },
    )
    .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_path);

    let override_repo_dir = tempdir().unwrap();
    let override_repo: PathBuf = override_repo_dir.path().to_path_buf();
    touch_guava_jar(&override_repo);
    let config_override = load_project_with_options(
        workspace_root,
        &LoadOptions {
            maven_repo: Some(override_repo.clone()),
            ..Default::default()
        },
    )
    .expect("load maven project with explicit maven_repo override");

    let override_jar_entries = config_override
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !override_jar_entries.is_empty(),
        "expected override to still produce jar entries, got: {:?}",
        config_override.classpath
    );
    assert_jar_entries_are_under_repo(&override_jar_entries, &override_repo);
}

#[test]
fn loads_maven_repo_from_mvn_maven_config_with_quoted_path_containing_spaces() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();
    write_pom_xml(workspace_root);
    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_dir = tempdir().unwrap();
    let repo_path = repo_dir.path().join("my maven repo");
    fs::create_dir_all(&repo_path).unwrap();
    touch_guava_jar(&repo_path);

    fs::write(
        workspace_root.join(".mvn/maven.config"),
        format!("-Dmaven.repo.local=\"{}\"", repo_path.display()),
    )
    .unwrap();

    let config = load_project_with_options(workspace_root, &LoadOptions::default())
        .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_path);
}

#[test]
fn loads_maven_repo_from_mvn_maven_config_with_space_separated_repo_local_arg() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();
    write_pom_xml(workspace_root);
    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_dir = tempdir().unwrap();
    let repo_path = repo_dir.path().join("repo local");
    fs::create_dir_all(&repo_path).unwrap();
    touch_guava_jar(&repo_path);

    fs::write(
        workspace_root.join(".mvn/maven.config"),
        format!("-Dmaven.repo.local \"{}\"", repo_path.display()),
    )
    .unwrap();

    let config = load_project_with_options(workspace_root, &LoadOptions::default())
        .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_path);
}

#[test]
fn loads_maven_repo_from_mvn_maven_config_with_single_quoted_path_containing_spaces() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();
    write_pom_xml(workspace_root);
    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_dir = tempdir().unwrap();
    let repo_path = repo_dir.path().join("my maven repo");
    fs::create_dir_all(&repo_path).unwrap();
    touch_guava_jar(&repo_path);

    fs::write(
        workspace_root.join(".mvn/maven.config"),
        format!("-Dmaven.repo.local='{}'", repo_path.display()),
    )
    .unwrap();

    let config = load_project_with_options(workspace_root, &LoadOptions::default())
        .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_path);
}

#[test]
fn loads_maven_repo_from_mvn_maven_config_with_space_separated_single_quoted_repo_local_arg() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();
    write_pom_xml(workspace_root);
    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_dir = tempdir().unwrap();
    let repo_path = repo_dir.path().join("repo local");
    fs::create_dir_all(&repo_path).unwrap();
    touch_guava_jar(&repo_path);

    fs::write(
        workspace_root.join(".mvn/maven.config"),
        format!("-Dmaven.repo.local '{}'", repo_path.display()),
    )
    .unwrap();

    let config = load_project_with_options(workspace_root, &LoadOptions::default())
        .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_path);
}

#[test]
fn loads_maven_repo_from_mvn_maven_config_resolves_relative_repo_local_to_workspace_root() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();
    write_pom_xml(workspace_root);
    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_path = workspace_root.join("relative-m2");
    touch_guava_jar(&repo_path);

    // Relative paths should be interpreted relative to the workspace root, since `.mvn/maven.config`
    // behaves like command line args and Maven is typically run from the project root.
    fs::write(workspace_root.join(".mvn/maven.config"), "-Dmaven.repo.local=relative-m2").unwrap();

    let config = load_project_with_options(workspace_root, &LoadOptions::default())
        .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_path);
}

#[test]
fn loads_maven_repo_from_mvn_maven_config_prefers_last_repo_local_value() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();
    write_pom_xml(workspace_root);
    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_1 = tempdir().unwrap();
    let repo_2 = tempdir().unwrap();
    let repo_1_path = repo_1.path().to_path_buf();
    let repo_2_path = repo_2.path().to_path_buf();
    touch_guava_jar(&repo_2_path);

    // The last valid `-Dmaven.repo.local` value should win.
    fs::write(
        workspace_root.join(".mvn/maven.config"),
        format!(
            "-Dmaven.repo.local={}\n-Dmaven.repo.local={}\n",
            repo_1_path.display(),
            repo_2_path.display()
        ),
    )
    .unwrap();

    let config = load_project_with_options(workspace_root, &LoadOptions::default())
        .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_2_path);
}

#[test]
fn loads_maven_repo_from_mvn_jvm_config() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();

    write_pom_xml(workspace_root);
    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_dir = tempdir().unwrap();
    let repo_path: PathBuf = repo_dir.path().to_path_buf();
    touch_guava_jar(&repo_path);

    fs::write(
        workspace_root.join(".mvn/jvm.config"),
        format!("-Dmaven.repo.local={}", repo_path.display()),
    )
    .unwrap();

    let config = load_project_with_options(
        workspace_root,
        &LoadOptions {
            maven_repo: None,
            ..Default::default()
        },
    )
    .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_path);
}

#[test]
fn maven_config_repo_local_overrides_jvm_config() {
    let workspace_dir = tempdir().unwrap();
    let workspace_root = workspace_dir.path();

    write_pom_xml(workspace_root);
    fs::create_dir_all(workspace_root.join(".mvn")).unwrap();

    let repo_from_maven_config_dir = tempdir().unwrap();
    let repo_from_maven_config: PathBuf = repo_from_maven_config_dir.path().to_path_buf();
    touch_guava_jar(&repo_from_maven_config);

    let repo_from_jvm_config_dir = tempdir().unwrap();
    let repo_from_jvm_config: PathBuf = repo_from_jvm_config_dir.path().to_path_buf();
    touch_guava_jar(&repo_from_jvm_config);

    fs::write(
        workspace_root.join(".mvn/maven.config"),
        format!("-Dmaven.repo.local={}", repo_from_maven_config.display()),
    )
    .unwrap();
    fs::write(
        workspace_root.join(".mvn/jvm.config"),
        format!("-Dmaven.repo.local={}", repo_from_jvm_config.display()),
    )
    .unwrap();

    let config = load_project_with_options(
        workspace_root,
        &LoadOptions {
            maven_repo: None,
            ..Default::default()
        },
    )
    .expect("load maven project");

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        !jar_entries.is_empty(),
        "expected at least one jar entry, got: {:?}",
        config.classpath
    );
    assert_jar_entries_are_under_repo(&jar_entries, &repo_from_maven_config);

    let repo_from_jvm_config_canon =
        fs::canonicalize(&repo_from_jvm_config).unwrap_or(repo_from_jvm_config.clone());
    assert!(
        jar_entries.iter().all(|jar| {
            let jar_canon = fs::canonicalize(jar).unwrap_or_else(|_| jar.clone());
            !jar_canon.starts_with(&repo_from_jvm_config_canon)
        }),
        "expected maven.config override (no jars under jvm.config repo). jvm repo={repo_from_jvm_config:?} (canonicalized to {repo_from_jvm_config_canon:?}). Got: {jar_entries:?}"
    );
}
