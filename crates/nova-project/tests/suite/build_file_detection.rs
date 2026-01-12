use std::fs;
use std::path::{Path, PathBuf};

use nova_build_model::GRADLE_SNAPSHOT_REL_PATH;
use nova_project::{
    is_build_file, load_project_with_options, reload_project, BuildSystem, LoadOptions,
};

fn join(rel: &str) -> PathBuf {
    rel.split('/')
        .filter(|part| !part.is_empty())
        .fold(PathBuf::new(), |p, part| p.join(part))
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

#[test]
fn gradle_is_build_file_recognizes_expected_paths() {
    let positives = [
        "gradlew",
        "gradlew.bat",
        "gradle/wrapper/gradle-wrapper.properties",
        "gradle/wrapper/gradle-wrapper.jar",
        // Dependency locking can change resolved versions/classpaths without touching build scripts.
        "gradle.lockfile",
        "gradle/dependency-locks/compileClasspath.lockfile",
        // Version catalogs:
        // - `libs.versions.toml` is the conventional (and historically supported) default catalog.
        // - Additional catalogs can be configured via `settings.gradle` and are commonly stored
        //   under `gradle/` (e.g. `gradle/deps.versions.toml`).
        "libs.versions.toml",
        "gradle/libs.versions.toml",
        "gradle/deps.versions.toml",
        "gradle/conventions.gradle",
        "gradle/conventions.gradle.kts",
        GRADLE_SNAPSHOT_REL_PATH,
    ];
    for rel in positives {
        let path = join(rel);
        assert!(
            is_build_file(BuildSystem::Gradle, &path),
            "expected Gradle build file: {rel}"
        );
        assert!(
            !is_build_file(BuildSystem::Maven, &path),
            "Gradle build file should not be Maven build file: {rel}"
        );
    }

    let negatives = [
        // Wrapper scripts must be at the build root.
        "sub/gradlew",
        "sub/gradlew.bat",
        // Wrapper properties must be in the canonical wrapper location.
        "gradle-wrapper.properties",
        "gradle/gradle-wrapper.properties",
        "wrapper/gradle-wrapper.properties",
        // Wrapper jar must be in the canonical wrapper location.
        "gradle-wrapper.jar",
        "gradle/gradle-wrapper.jar",
        "wrapper/gradle-wrapper.jar",
        // Dependency lockfiles should not match outside the canonical patterns.
        "foo.lockfile",
        // Version catalogs under ignored dirs should not be treated as build files.
        ".gradle/deps.versions.toml",
        // Lockfiles under ignored dirs should not be treated as build files.
        ".gradle/gradle.lockfile",
        ".gradle/dependency-locks/compileClasspath.lockfile",
        // Additional version catalogs must be under `gradle/`.
        "deps.versions.toml",
        "build/deps.versions.toml",
        "target/deps.versions.toml",
        ".nova/deps.versions.toml",
        // Ignore common non-source trees even if they contain build-looking markers.
        "node_modules/build.gradle",
        ".gradle/build.gradle",
        "bazel-out/build.gradle",
        "bazel-bin/build.gradle",
        "bazel-testlogs/build.gradle",
        "bazel-out/deps.versions.toml",
        // Sanity check: non-build file.
        "gradle/conventions.txt",
        // Dependency locks must use the `.lockfile` extension.
        "gradle/dependency-locks/compileClasspath.lock",
    ];
    for rel in negatives {
        let path = join(rel);
        assert!(
            !is_build_file(BuildSystem::Gradle, &path),
            "unexpected Gradle build file match: {rel}"
        );
    }
}

#[test]
fn maven_is_build_file_recognizes_expected_paths() {
    let positives = [
        "mvnw",
        "mvnw.cmd",
        ".mvn/wrapper/maven-wrapper.properties",
        ".mvn/wrapper/maven-wrapper.jar",
        ".mvn/maven.config",
        ".mvn/jvm.config",
        ".mvn/extensions.xml",
    ];
    for rel in positives {
        let path = join(rel);
        assert!(
            is_build_file(BuildSystem::Maven, &path),
            "expected Maven build file: {rel}"
        );
        assert!(
            !is_build_file(BuildSystem::Gradle, &path),
            "Maven build file should not be Gradle build file: {rel}"
        );
    }

    let negatives = [
        "mvnw.bat",
        // Wrapper properties must be in the canonical wrapper location.
        "maven-wrapper.properties",
        ".mvn/maven-wrapper.properties",
        "wrapper/maven-wrapper.properties",
        // Wrapper jar must be in the canonical wrapper location.
        "maven-wrapper.jar",
        ".mvn/maven-wrapper.jar",
        "wrapper/maven-wrapper.jar",
        // `.mvn` config files must be in `.mvn/`.
        "maven.config",
        "jvm.config",
        "extensions.xml",
        ".mvn/wrapper/maven.config",
        // Ignore common non-source trees even if they contain build-looking markers.
        "node_modules/pom.xml",
        "target/pom.xml",
        "bazel-out/pom.xml",
    ];
    for rel in negatives {
        let path = join(rel);
        assert!(
            !is_build_file(BuildSystem::Maven, &path),
            "unexpected Maven build file match: {rel}"
        );
    }
}

#[test]
fn build_markers_under_ignored_dirs_are_not_build_files() {
    let cases = [
        (
            BuildSystem::Gradle,
            PathBuf::from("node_modules")
                .join("foo")
                .join("build.gradle"),
        ),
        (
            BuildSystem::Bazel,
            PathBuf::from("bazel-out").join("foo").join("BUILD"),
        ),
        (
            BuildSystem::Bazel,
            PathBuf::from("bazel-bin").join("foo").join("BUILD.bazel"),
        ),
        (
            BuildSystem::Bazel,
            PathBuf::from("bazel-testlogs")
                .join("foo")
                .join("rules.bzl"),
        ),
        (
            BuildSystem::Bazel,
            PathBuf::from("bazel-myws").join("foo").join("WORKSPACE"),
        ),
    ];

    for (build_system, path) in cases {
        assert!(
            !is_build_file(build_system, &path),
            "expected {} not to be treated as a build file for {build_system:?}",
            path.display()
        );
    }
}

#[test]
fn reload_project_reloads_on_gradle_wrapper_file_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");

    // Minimal Gradle project with one module.
    write(&root.join("settings.gradle"), r#"include("app")"#);
    write(&root.join("build.gradle"), "// root");
    write(&root.join("app/build.gradle"), "// app");

    let gradle_home = tempfile::tempdir().expect("tempdir");
    let mut options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.modules.len(), 1);

    // Update settings to add another module; this should only be observed if reload_project
    // decides to reload on our chosen changed-file path.
    write(&root.join("settings.gradle"), r#"include("app", "lib")"#);
    write(&root.join("lib/build.gradle"), "// lib");

    // The changed file is *not* settings.gradle; it is a wrapper file that should trigger
    // a full reload.
    let wrapper_props = root.join("gradle/wrapper/gradle-wrapper.properties");
    write(
        &wrapper_props,
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.0-bin.zip\n",
    );

    let reloaded = reload_project(&config, &mut options, &[wrapper_props]).expect("reload project");
    assert_eq!(reloaded.modules.len(), 2);
}

#[test]
fn reload_project_reloads_on_gradle_wrapper_jar_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");

    // Minimal Gradle project with one module.
    write(&root.join("settings.gradle"), r#"include("app")"#);
    write(&root.join("build.gradle"), "// root");
    write(&root.join("app/build.gradle"), "// app");

    let mut options = LoadOptions::default();
    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.modules.len(), 1);

    // Update settings to add another module; this should only be observed if reload_project
    // decides to reload on our chosen changed-file path.
    write(&root.join("settings.gradle"), r#"include("app", "lib")"#);
    write(&root.join("lib/build.gradle"), "// lib");

    // The changed file is *not* settings.gradle; it is a wrapper jar file that should trigger
    // a full reload.
    let wrapper_jar = root.join("gradle/wrapper/gradle-wrapper.jar");
    write(&wrapper_jar, "jar bytes are not relevant for this test\n");

    let reloaded = reload_project(&config, &mut options, &[wrapper_jar]).expect("reload project");
    assert_eq!(reloaded.modules.len(), 2);
}

#[test]
fn reload_project_reloads_on_gradle_lockfile_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");

    // Minimal Gradle project with one module.
    write(&root.join("settings.gradle"), r#"include("app")"#);
    write(&root.join("build.gradle"), "// root");
    write(&root.join("app/build.gradle"), "// app");
    write(&root.join("gradle.lockfile"), "locked=1\n");

    let gradle_home = tempfile::tempdir().expect("tempdir");
    let mut options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.modules.len(), 1);

    // Update settings to add another module; this should only be observed if reload_project
    // decides to reload on our chosen changed-file path.
    write(&root.join("settings.gradle"), r#"include("app", "lib")"#);
    write(&root.join("lib/build.gradle"), "// lib");

    // The changed file is not settings.gradle; it is a dependency lockfile that should trigger a
    // full reload.
    let lockfile = root.join("gradle.lockfile");
    write(&lockfile, "locked=2\n");

    let reloaded = reload_project(&config, &mut options, &[lockfile]).expect("reload project");
    assert_eq!(reloaded.modules.len(), 2);
}

#[test]
fn reload_project_reloads_on_gradle_dependency_lockfile_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");

    // Minimal Gradle project with one module.
    write(&root.join("settings.gradle"), r#"include("app")"#);
    write(&root.join("build.gradle"), "// root");
    write(&root.join("app/build.gradle"), "// app");
    write(
        &root.join("gradle/dependency-locks/compileClasspath.lockfile"),
        "locked=1\n",
    );

    let gradle_home = tempfile::tempdir().expect("tempdir");
    let mut options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.modules.len(), 1);

    // Update settings to add another module; this should only be observed if reload_project decides
    // to reload on our chosen changed-file path.
    write(&root.join("settings.gradle"), r#"include("app", "lib")"#);
    write(&root.join("lib/build.gradle"), "// lib");

    // The changed file is not settings.gradle; it is a dependency lockfile that should trigger a
    // full reload.
    let lockfile = root.join("gradle/dependency-locks/compileClasspath.lockfile");
    write(&lockfile, "locked=2\n");

    let reloaded = reload_project(&config, &mut options, &[lockfile]).expect("reload project");
    assert_eq!(reloaded.modules.len(), 2);
}

#[test]
fn reload_project_reloads_on_maven_wrapper_file_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");

    // Minimal Maven aggregator with one module.
    write(
        &root.join("pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>demo</artifactId>
              <version>0.0.1</version>
              <packaging>pom</packaging>
              <modules>
                <module>app</module>
              </modules>
            </project>
        "#,
    );
    write(
        &root.join("app/pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>app</artifactId>
              <version>0.0.1</version>
            </project>
        "#,
    );

    let repo_dir = tempfile::tempdir().expect("tempdir");
    let mut options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");
    assert_eq!(config.build_system, BuildSystem::Maven);
    assert_eq!(config.modules.len(), 1);

    // Modify the root pom to add another module.
    write(
        &root.join("pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>demo</artifactId>
              <version>0.0.1</version>
              <packaging>pom</packaging>
              <modules>
                <module>app</module>
                <module>lib</module>
              </modules>
            </project>
        "#,
    );
    write(
        &root.join("lib/pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>lib</artifactId>
              <version>0.0.1</version>
            </project>
        "#,
    );

    // The changed file is *not* pom.xml; it is a Maven wrapper config file.
    let maven_config = root.join(".mvn/maven.config");
    write(&maven_config, "-DskipTests\n");

    let reloaded = reload_project(&config, &mut options, &[maven_config]).expect("reload project");
    assert_eq!(reloaded.modules.len(), 2);
}

#[test]
fn reload_project_reloads_on_maven_wrapper_jar_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");

    // Minimal Maven aggregator with one module.
    write(
        &root.join("pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>demo</artifactId>
              <version>0.0.1</version>
              <packaging>pom</packaging>
              <modules>
                <module>app</module>
              </modules>
            </project>
        "#,
    );
    write(
        &root.join("app/pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>app</artifactId>
              <version>0.0.1</version>
            </project>
        "#,
    );

    let mut options = LoadOptions::default();
    let config = load_project_with_options(&root, &options).expect("load maven project");
    assert_eq!(config.build_system, BuildSystem::Maven);
    assert_eq!(config.modules.len(), 1);

    // Modify the root pom to add another module.
    write(
        &root.join("pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>demo</artifactId>
              <version>0.0.1</version>
              <packaging>pom</packaging>
              <modules>
                <module>app</module>
                <module>lib</module>
              </modules>
            </project>
        "#,
    );
    write(
        &root.join("lib/pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>lib</artifactId>
              <version>0.0.1</version>
            </project>
        "#,
    );

    // The changed file is *not* pom.xml; it is a Maven wrapper jar file.
    let wrapper_jar = root.join(".mvn/wrapper/maven-wrapper.jar");
    write(&wrapper_jar, "jar bytes are not relevant for this test\n");

    let reloaded = reload_project(&config, &mut options, &[wrapper_jar]).expect("reload project");
    assert_eq!(reloaded.modules.len(), 2);
}
