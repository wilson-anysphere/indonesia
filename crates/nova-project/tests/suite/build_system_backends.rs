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
    assert!(maven
        .watch_files()
        .contains(&PathPattern::ExactFileName("pom.xml")));
    assert!(maven
        .watch_files()
        .contains(&PathPattern::Glob("**/.mvn/jvm.config")));
    assert!(maven
        .watch_files()
        .contains(&PathPattern::Glob("**/.mvn/wrapper/maven-wrapper.jar")));
    assert!(maven
        .watch_files()
        .contains(&PathPattern::ExactFileName("module-info.java")));

    let gradle = GradleBuildSystem::new(LoadOptions::default());
    assert!(gradle
        .watch_files()
        .contains(&PathPattern::ExactFileName("settings.gradle")));
    assert!(gradle
        .watch_files()
        .contains(&PathPattern::ExactFileName("libs.versions.toml")));
    assert!(gradle
        .watch_files()
        .contains(&PathPattern::Glob("**/gradle/*.versions.toml")));
    assert!(
        !gradle
            .watch_files()
            .contains(&PathPattern::Glob("**/*.versions.toml")),
        "Gradle backends should only watch `libs.versions.toml` and direct children of `gradle/` \
         to match build-file fingerprinting semantics"
    );
    assert!(gradle
        .watch_files()
        .contains(&PathPattern::Glob("**/*.gradle")));
    assert!(gradle
        .watch_files()
        .contains(&PathPattern::Glob("**/gradle/wrapper/gradle-wrapper.jar")));
    assert!(gradle
        .watch_files()
        .contains(&PathPattern::ExactFileName("gradle.lockfile")));
    assert!(gradle
        .watch_files()
        .contains(&PathPattern::Glob("**/dependency-locks/**/*.lockfile")));
    assert!(gradle
        .watch_files()
        .contains(&PathPattern::Glob(GRADLE_SNAPSHOT_GLOB)));

    let bazel = BazelBuildSystem::new(LoadOptions::default());
    assert!(bazel
        .watch_files()
        .contains(&PathPattern::ExactFileName("WORKSPACE")));
    assert!(bazel
        .watch_files()
        .contains(&PathPattern::ExactFileName(".bazelignore")));
    assert!(bazel
        .watch_files()
        .contains(&PathPattern::Glob("**/.bsp/*.json")));
    assert!(bazel.watch_files().contains(&PathPattern::Glob("**/*.bzl")));

    let simple = SimpleBuildSystem::new(LoadOptions::default());
    assert!(simple
        .watch_files()
        .contains(&PathPattern::ExactFileName("module-info.java")));
    assert!(simple
        .watch_files()
        .contains(&PathPattern::ExactFileName("pom.xml")));
    assert!(simple
        .watch_files()
        .contains(&PathPattern::ExactFileName("build.gradle")));
    assert!(simple
        .watch_files()
        .contains(&PathPattern::Glob("**/gradle/wrapper/gradle-wrapper.jar")));
    assert!(simple
        .watch_files()
        .contains(&PathPattern::Glob("**/*.bzl")));
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
