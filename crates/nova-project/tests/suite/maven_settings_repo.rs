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
        .expect("env lock poisoned");

    // Set both to behave deterministically on Windows/Linux.
    let _home = EnvVarGuard::set("HOME", home);
    let _userprofile = EnvVarGuard::set("USERPROFILE", home);

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
        assert!(
            jar_entries.contains(&expected),
            "expected jar path {expected:?} in classpath entries: {jar_entries:?}"
        );
    });
}

#[test]
fn settings_xml_placeholder_local_repository_is_ignored() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace_root = temp.path().join("workspace");

    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    // Best-effort: Nova ignores Maven repo config values containing `${...}` placeholders.
    // If we *didn't* ignore this, we'd end up trying to resolve jars under
    // `<home>/${user.home}/...`, which is almost certainly wrong.
    write_settings_xml(&home, Path::new("${user.home}/.m2/custom-repo"));

    // With the placeholder ignored, discovery should fall back to the default repo under HOME.
    let default_repo = home.join(".m2").join("repository");
    write_pom_xml(&workspace_root);
    touch_expected_jar(&default_repo);

    with_home_dir(&home, || {
        let options = LoadOptions::default();
        let cfg = load_project_with_options(&workspace_root, &options).expect("load project");

        let jar_entries = cfg
            .classpath
            .iter()
            .filter(|e| e.kind == ClasspathEntryKind::Jar)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>();

        let expected = expected_guava_jar(&default_repo);
        assert!(
            jar_entries.contains(&expected),
            "expected fallback jar path {expected:?} in classpath entries: {jar_entries:?}"
        );
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
        assert!(
            jar_entries.contains(&expected),
            "expected jar path {expected:?} in classpath entries: {jar_entries:?}"
        );

        assert!(
            jar_entries.iter().all(|p| !p.starts_with(&repo_from_settings)),
            "expected maven.config override (no jars under settings.xml repo). Got: {jar_entries:?}"
        );
    });
}
