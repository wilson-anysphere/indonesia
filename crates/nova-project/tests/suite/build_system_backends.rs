use std::path::PathBuf;

use nova_build_model::{BuildSystemBackend, PathPattern, GRADLE_SNAPSHOT_GLOB};
use nova_project::{
    BazelBuildSystem, GradleBuildSystem, LoadOptions, MavenBuildSystem, SimpleBuildSystem,
};

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

#[test]
fn maven_backend_detects_pom_xml() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("pom.xml"), "<project></project>").expect("write pom.xml");

    let backend = MavenBuildSystem::new(LoadOptions::default());
    assert!(backend.detect(tmp.path()));
}

#[test]
fn gradle_backend_detects_settings_or_build_files() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let backend = GradleBuildSystem::new(LoadOptions::default());
    assert!(
        !backend.detect(tmp.path()),
        "empty dir should not detect as Gradle"
    );

    std::fs::write(
        tmp.path().join("settings.gradle"),
        "rootProject.name = \"x\"",
    )
    .expect("write settings.gradle");
    assert!(backend.detect(tmp.path()));
}

#[test]
fn bazel_backend_detects_workspace_markers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("WORKSPACE"), "#").expect("write WORKSPACE");

    let backend = BazelBuildSystem::new(LoadOptions::default());
    assert!(backend.detect(tmp.path()));
}

#[test]
fn watch_files_contains_canonical_markers() {
    let maven = MavenBuildSystem::new(LoadOptions::default());
    let maven_watch_files = maven.watch_files();
    assert!(
        maven_watch_files.contains(&PathPattern::ExactFileName("pom.xml")),
        "expected Maven watch_files to contain ExactFileName(\"pom.xml\"); got: {maven_watch_files:?}",
    );
    assert!(
        maven_watch_files.contains(&PathPattern::Glob("**/.mvn/jvm.config")),
        "expected Maven watch_files to contain Glob(\"**/.mvn/jvm.config\"); got: {maven_watch_files:?}",
    );
    assert!(
        maven_watch_files.contains(&PathPattern::Glob("**/.mvn/wrapper/maven-wrapper.jar")),
        "expected Maven watch_files to contain Glob(\"**/.mvn/wrapper/maven-wrapper.jar\"); got: {maven_watch_files:?}",
    );
    assert!(
        maven_watch_files.contains(&PathPattern::ExactFileName("module-info.java")),
        "expected Maven watch_files to contain ExactFileName(\"module-info.java\"); got: {maven_watch_files:?}",
    );

    let gradle = GradleBuildSystem::new(LoadOptions::default());
    let gradle_watch_files = gradle.watch_files();
    assert!(
        gradle_watch_files.contains(&PathPattern::ExactFileName("settings.gradle")),
        "expected Gradle watch_files to contain ExactFileName(\"settings.gradle\"); got: {gradle_watch_files:?}",
    );
    assert!(
        gradle_watch_files.contains(&PathPattern::ExactFileName("libs.versions.toml")),
        "expected Gradle watch_files to contain ExactFileName(\"libs.versions.toml\"); got: {gradle_watch_files:?}",
    );
    assert!(
        gradle_watch_files.contains(&PathPattern::Glob("**/gradle/*.versions.toml")),
        "expected Gradle watch_files to contain Glob(\"**/gradle/*.versions.toml\"); got: {gradle_watch_files:?}",
    );
    assert!(
        !gradle_watch_files.contains(&PathPattern::Glob("**/*.versions.toml")),
        "Gradle backends should only watch `libs.versions.toml` and direct children of `gradle/` \
         to match build-file fingerprinting semantics; got: {gradle_watch_files:?}"
    );
    assert!(
        gradle_watch_files.contains(&PathPattern::Glob("**/*.gradle")),
        "expected Gradle watch_files to contain Glob(\"**/*.gradle\"); got: {gradle_watch_files:?}",
    );
    assert!(
        gradle_watch_files.contains(&PathPattern::Glob("**/gradle/wrapper/gradle-wrapper.jar")),
        "expected Gradle watch_files to contain Glob(\"**/gradle/wrapper/gradle-wrapper.jar\"); got: {gradle_watch_files:?}",
    );
    assert!(
        gradle_watch_files.contains(&PathPattern::ExactFileName("gradle.lockfile")),
        "expected Gradle watch_files to contain ExactFileName(\"gradle.lockfile\"); got: {gradle_watch_files:?}",
    );
    assert!(
        gradle_watch_files.contains(&PathPattern::Glob("**/dependency-locks/**/*.lockfile")),
        "expected Gradle watch_files to contain Glob(\"**/dependency-locks/**/*.lockfile\"); got: {gradle_watch_files:?}",
    );
    assert!(
        gradle_watch_files.contains(&PathPattern::Glob(GRADLE_SNAPSHOT_GLOB)),
        "expected Gradle watch_files to contain Glob(GRADLE_SNAPSHOT_GLOB); got: {gradle_watch_files:?}",
    );

    let bazel = BazelBuildSystem::new(LoadOptions::default());
    let bazel_watch_files = bazel.watch_files();
    assert!(
        bazel_watch_files.contains(&PathPattern::ExactFileName("WORKSPACE")),
        "expected Bazel watch_files to contain ExactFileName(\"WORKSPACE\"); got: {bazel_watch_files:?}",
    );
    assert!(
        bazel_watch_files.contains(&PathPattern::ExactFileName(".bazelignore")),
        "expected Bazel watch_files to contain ExactFileName(\".bazelignore\"); got: {bazel_watch_files:?}",
    );
    assert!(
        bazel_watch_files.contains(&PathPattern::Glob("**/.bsp/*.json")),
        "expected Bazel watch_files to contain Glob(\"**/.bsp/*.json\"); got: {bazel_watch_files:?}",
    );
    assert!(
        bazel_watch_files.contains(&PathPattern::Glob("**/*.bzl")),
        "expected Bazel watch_files to contain Glob(\"**/*.bzl\"); got: {bazel_watch_files:?}",
    );

    let simple = SimpleBuildSystem::new(LoadOptions::default());
    let simple_watch_files = simple.watch_files();
    assert!(
        simple_watch_files.contains(&PathPattern::ExactFileName("module-info.java")),
        "expected Simple watch_files to contain ExactFileName(\"module-info.java\"); got: {simple_watch_files:?}",
    );
    assert!(
        simple_watch_files.contains(&PathPattern::ExactFileName("pom.xml")),
        "expected Simple watch_files to contain ExactFileName(\"pom.xml\"); got: {simple_watch_files:?}",
    );
    assert!(
        simple_watch_files.contains(&PathPattern::ExactFileName("build.gradle")),
        "expected Simple watch_files to contain ExactFileName(\"build.gradle\"); got: {simple_watch_files:?}",
    );
    assert!(
        simple_watch_files.contains(&PathPattern::Glob("**/gradle/wrapper/gradle-wrapper.jar")),
        "expected Simple watch_files to contain Glob(\"**/gradle/wrapper/gradle-wrapper.jar\"); got: {simple_watch_files:?}",
    );
    assert!(
        simple_watch_files.contains(&PathPattern::Glob("**/*.bzl")),
        "expected Simple watch_files to contain Glob(\"**/*.bzl\"); got: {simple_watch_files:?}",
    );
}

#[test]
fn parse_project_returns_non_empty_models_for_fixtures() {
    let maven_root = testdata_path("maven-multi");
    let maven = MavenBuildSystem::new(LoadOptions::default());
    let maven_model = maven
        .parse_project(&maven_root)
        .expect("parse maven project");
    assert!(
        !maven_model.modules.is_empty(),
        "expected Maven fixture to return at least one module"
    );

    let gradle_root = testdata_path("gradle-multi");
    let gradle = GradleBuildSystem::new(LoadOptions::default());
    let gradle_model = gradle
        .parse_project(&gradle_root)
        .expect("parse gradle project");
    assert!(
        !gradle_model.modules.is_empty(),
        "expected Gradle fixture to return at least one module"
    );

    let bazel_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("nova-build-bazel")
        .join("testdata")
        .join("minimal_workspace");
    let bazel = BazelBuildSystem::new(LoadOptions::default());
    let bazel_model = bazel
        .parse_project(&bazel_root)
        .expect("parse bazel project");
    assert!(
        !bazel_model.modules.is_empty(),
        "expected Bazel fixture to return at least one module"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir src");
    std::fs::write(tmp.path().join("src/Main.java"), "class Main {}").expect("write java");
    let simple = SimpleBuildSystem::new(LoadOptions::default());
    let simple_model = simple
        .parse_project(tmp.path())
        .expect("parse simple project");
    assert!(
        !simple_model.modules.is_empty(),
        "expected Simple project to return at least one module"
    );
}
