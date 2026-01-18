use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use nova_project::{load_project_with_options, ClasspathEntryKind, LoadOptions};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct EnvVarGuard {
    key: &'static str,
    prior: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let prior = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn with_home_dir<T>(home: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());

    // Set both to behave deterministically on Windows/Linux.
    let _home = EnvVarGuard::set("HOME", home);
    let _userprofile = EnvVarGuard::set("USERPROFILE", home);
    let maven_user_home = home.join(".m2");
    let _maven_user_home = EnvVarGuard::set("MAVEN_USER_HOME", &maven_user_home);

    f()
}

fn write_settings_xml(home: &Path, maven_repo: &Path) {
    let m2 = home.join(".m2");
    std::fs::create_dir_all(&m2).expect("create ~/.m2");

    let contents = format!(
        r#"<settings><localRepository>{}</localRepository></settings>"#,
        maven_repo.to_string_lossy()
    );
    std::fs::write(m2.join("settings.xml"), contents).expect("write ~/.m2/settings.xml");
}

fn write_pom_xml(workspace_root: &Path) {
    let pom = r#"
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId>
          <artifactId>demo</artifactId>
          <version>1.0</version>

          <dependencies>
            <dependency>
              <groupId>com.google.guava</groupId>
              <artifactId>guava</artifactId>
              <version>33.0.0-jre</version>
            </dependency>
          </dependencies>
        </project>
    "#;
    std::fs::write(workspace_root.join("pom.xml"), pom).expect("write pom.xml");
}

fn expected_guava_jar(repo: &Path) -> PathBuf {
    repo.join("com")
        .join("google")
        .join("guava")
        .join("guava")
        .join("33.0.0-jre")
        .join("guava-33.0.0-jre.jar")
}

fn touch_expected_jar(repo: &Path) {
    let jar = expected_guava_jar(repo);
    std::fs::create_dir_all(jar.parent().expect("jar parent")).expect("create jar parent");
    std::fs::write(&jar, b"not really a jar").expect("write fake jar");
}

fn assert_jar_entries_contain_expected(jar_entries: &[PathBuf], expected: &Path) {
    let expected = expected.to_path_buf();
    let expected_canon = std::fs::canonicalize(&expected).unwrap_or(expected.clone());
    assert!(
        jar_entries.iter().any(|jar| {
            let jar_canon = std::fs::canonicalize(jar).unwrap_or_else(|_| jar.clone());
            jar_canon == expected_canon
        }),
        "expected jar path {expected:?} (canonicalized to {expected_canon:?}) in classpath entries: {jar_entries:?}"
    );
}

#[test]
fn settings_xml_local_repository_is_used_for_dependency_jars() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo-from-settings");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&repo).expect("create repo");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    write_settings_xml(&home, &repo);
    write_pom_xml(&workspace_root);
    touch_expected_jar(&repo);

    with_home_dir(&home, || {
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&repo);
        assert_jar_entries_contain_expected(&jar_entries, &expected);
    });
}

#[test]
fn settings_xml_placeholder_local_repository_expands_user_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    // The Maven local repository path commonly uses the `${user.home}` placeholder.
    // Expand it and resolve dependency jars from the resulting directory.
    write_settings_xml(&home, Path::new("${user.home}/.m2/custom-repo"));

    let expanded_repo = home.join(".m2").join("custom-repo");
    write_pom_xml(&workspace_root);
    touch_expected_jar(&expanded_repo);

    with_home_dir(&home, || {
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&expanded_repo);
        assert_jar_entries_contain_expected(&jar_entries, &expected);
    });
}

#[test]
fn settings_xml_tilde_local_repository_expands_user_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    // Maven configs commonly use `~/.m2/...` for repository paths.
    let m2 = home.join(".m2");
    std::fs::create_dir_all(&m2).expect("create ~/.m2");
    std::fs::write(
        m2.join("settings.xml"),
        r#"<settings><localRepository>~/.m2/custom-repo</localRepository></settings>"#,
    )
    .expect("write ~/.m2/settings.xml");

    let expanded_repo = home.join(".m2").join("custom-repo");
    write_pom_xml(&workspace_root);
    touch_expected_jar(&expanded_repo);

    with_home_dir(&home, || {
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&expanded_repo);
        assert_jar_entries_contain_expected(&jar_entries, &expected);
    });
}

#[test]
fn settings_xml_env_local_repository_expands_env_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    // Maven settings.xml supports `${env.*}` placeholders.
    write_settings_xml(&home, Path::new("${env.HOME}/.m2/custom-repo"));

    let expanded_repo = home.join(".m2").join("custom-repo");
    write_pom_xml(&workspace_root);
    touch_expected_jar(&expanded_repo);

    with_home_dir(&home, || {
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&expanded_repo);
        assert_jar_entries_contain_expected(&jar_entries, &expected);
    });
}

#[test]
fn maven_config_tilde_repo_local_expands_user_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    write_pom_xml(&workspace_root);

    let expanded_repo = home.join(".m2").join("custom-repo");
    touch_expected_jar(&expanded_repo);

    let mvn_dir = workspace_root.join(".mvn");
    std::fs::create_dir_all(&mvn_dir).expect("create .mvn");
    std::fs::write(
        mvn_dir.join("maven.config"),
        "-Dmaven.repo.local=~/.m2/custom-repo\n",
    )
    .expect("write .mvn/maven.config");

    with_home_dir(&home, || {
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&expanded_repo);
        assert_jar_entries_contain_expected(&jar_entries, &expected);
    });
}

#[test]
fn maven_config_placeholder_repo_local_expands_user_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    write_pom_xml(&workspace_root);

    let expanded_repo = home.join(".m2").join("custom-repo");
    touch_expected_jar(&expanded_repo);

    let mvn_dir = workspace_root.join(".mvn");
    std::fs::create_dir_all(&mvn_dir).expect("create .mvn");
    std::fs::write(
        mvn_dir.join("maven.config"),
        "-Dmaven.repo.local=${user.home}/.m2/custom-repo\n",
    )
    .expect("write .mvn/maven.config");

    with_home_dir(&home, || {
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&expanded_repo);
        assert_jar_entries_contain_expected(&jar_entries, &expected);
    });
}

#[test]
fn maven_config_env_repo_local_expands_env_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let m2_home = temp.path().join("m2-home");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&m2_home).expect("create m2 home");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    write_pom_xml(&workspace_root);

    let expanded_repo = m2_home.join("custom-repo");
    touch_expected_jar(&expanded_repo);

    let mvn_dir = workspace_root.join(".mvn");
    std::fs::create_dir_all(&mvn_dir).expect("create .mvn");
    std::fs::write(
        mvn_dir.join("maven.config"),
        "-Dmaven.repo.local=${env.M2_HOME}/custom-repo\n",
    )
    .expect("write .mvn/maven.config");

    with_home_dir(&home, || {
        let _m2_home_guard = EnvVarGuard::set("M2_HOME", &m2_home);
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&expanded_repo);
        assert_jar_entries_contain_expected(&jar_entries, &expected);
    });
}

#[test]
fn maven_config_repo_local_overrides_settings_xml() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo_from_settings = temp.path().join("repo-from-settings");
    let repo_from_config = temp.path().join("repo-from-maven-config");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&repo_from_settings).expect("create repo");
    std::fs::create_dir_all(&repo_from_config).expect("create repo");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    write_settings_xml(&home, &repo_from_settings);
    write_pom_xml(&workspace_root);
    touch_expected_jar(&repo_from_config);

    let mvn_dir = workspace_root.join(".mvn");
    std::fs::create_dir_all(&mvn_dir).expect("create .mvn");
    std::fs::write(
        mvn_dir.join("maven.config"),
        format!(
            "-Dmaven.repo.local={}\n",
            repo_from_config.to_string_lossy()
        ),
    )
    .expect("write .mvn/maven.config");

    with_home_dir(&home, || {
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&repo_from_config);
        assert_jar_entries_contain_expected(&jar_entries, &expected);

        let repo_from_settings_canon =
            std::fs::canonicalize(&repo_from_settings).unwrap_or(repo_from_settings.clone());
        assert!(
            jar_entries.iter().all(|p| {
                let p_canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                !p_canon.starts_with(&repo_from_settings_canon)
            }),
            "expected maven.config override (no jars under settings.xml repo). settings repo={repo_from_settings:?} (canonicalized to {repo_from_settings_canon:?}). Got: {jar_entries:?}"
        );
    });
}

#[test]
fn settings_xml_is_discovered_via_maven_user_home_env_var() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let maven_user_home = temp.path().join("maven-user-home");
    let repo_from_settings = temp.path().join("repo-from-settings");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&maven_user_home).expect("create maven user home");
    std::fs::create_dir_all(&repo_from_settings).expect("create repo");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    // When `MAVEN_USER_HOME` is set, Maven reads user-level configuration (like `settings.xml`)
    // from that directory instead of `${user.home}/.m2`.
    std::fs::write(
        maven_user_home.join("settings.xml"),
        format!(
            r#"<settings><localRepository>{}</localRepository></settings>"#,
            repo_from_settings.to_string_lossy()
        ),
    )
    .expect("write settings.xml");

    write_pom_xml(&workspace_root);
    touch_expected_jar(&repo_from_settings);

    with_home_dir(&home, || {
        let _maven_user_home = EnvVarGuard::set("MAVEN_USER_HOME", &maven_user_home);
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&repo_from_settings);
        assert_jar_entries_contain_expected(&jar_entries, &expected);
    });
}
