use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nova_project::load_project;

mod suite;

#[test]
fn integration_tests_are_consolidated_into_this_harness() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");

    let expected = std::path::Path::new(file!())
        .file_name()
        .expect("harness filename is missing")
        .to_string_lossy()
        .into_owned();

    assert_eq!(
        expected, "harness.rs",
        "expected nova-project integration test harness to be named harness.rs (so `cargo test --locked -p nova-project --test harness` works); got: {expected}"
    );

    let mut root_rs_files = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).unwrap_or_else(|err| {
        panic!(
            "failed to read nova-project tests dir {}: {err}",
            tests_dir.display()
        )
    }) {
        let entry = entry
            .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", tests_dir.display()));
        let path = entry.path();

        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
            root_rs_files.push(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }

    root_rs_files.sort();
    assert_eq!(
        root_rs_files,
        [expected.clone()],
        "expected a single root integration test harness file (tests/{expected}); found: {root_rs_files:?}"
    );

    // Ensure every `tests/suite/*.rs` module is included in `tests/suite/mod.rs`,
    // otherwise those tests silently won't run.
    let suite_dir = tests_dir.join("suite");
    let suite_mod_path = suite_dir.join("mod.rs");
    let suite_source = std::fs::read_to_string(&suite_mod_path).unwrap_or_else(|err| {
        panic!(
            "failed to read nova-project integration test suite {}: {err}",
            suite_mod_path.display()
        )
    });

    let mut suite_rs_files = Vec::new();
    for entry in std::fs::read_dir(&suite_dir).unwrap_or_else(|err| {
        panic!(
            "failed to read nova-project test suite dir {}: {err}",
            suite_dir.display()
        )
    }) {
        let entry = entry
            .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", suite_dir.display()));
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if file_name != "mod.rs" {
                suite_rs_files.push(file_name);
            }
        }
    }

    suite_rs_files.sort();
    let missing: Vec<_> = suite_rs_files
        .iter()
        .filter(|file| {
            let stem = std::path::Path::new(file)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            !stem.is_empty() && !suite_source.contains(&format!("mod {stem};"))
        })
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "tests/suite/mod.rs is missing module includes for suite files: {missing:?}"
    );
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR should be crates/<name>")
        .to_path_buf()
}

fn fixture_dir(name: &str) -> PathBuf {
    repo_root().join("test-projects").join(name)
}

fn require_fixture(name: &str) -> PathBuf {
    let dir = fixture_dir(name);
    assert!(
        dir.exists(),
        "Missing fixture `{name}` at {dir:?}.\n\
         Run `./scripts/clone-test-projects.sh` from the repo root first."
    );
    dir
}

fn should_run_fixture(name: &str) -> bool {
    let mut any_filter = false;

    for var in ["NOVA_REAL_PROJECT", "NOVA_TEST_PROJECTS"] {
        let Ok(filter) = std::env::var(var) else {
            continue;
        };
        let filter = filter.trim();
        if filter.is_empty() {
            continue;
        }

        any_filter = true;
        if filter
            .split(',')
            .map(str::trim)
            .any(|candidate| candidate == name)
        {
            return true;
        }
    }

    !any_filter
}

fn init_cache_dir() {
    // These tests are ignored and run on-demand. Still, keep Nova's cache writes out
    // of the user's home directory and ensure each run starts from a clean slate so
    // `files_indexed > 0` remains a stable assertion.
    static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();
    let cache_root = CACHE_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        // Keep the directory alive for the duration of the test process.
        std::mem::forget(dir);
        path
    });
    std::env::set_var("NOVA_CACHE_DIR", cache_root);
}

fn assert_file_is_under_source_root(config: &nova_build_model::ProjectConfig, file: &Path) {
    assert!(
        config
            .source_roots
            .iter()
            .any(|root| file.starts_with(&root.path)),
        "expected {} to fall under one of the discovered source roots, got: {:#?}",
        file.display(),
        config.source_roots
    );
}

#[test]
#[ignore]
fn spring_petclinic_smoke() {
    init_cache_dir();
    if !should_run_fixture("spring-petclinic") {
        return;
    }

    let root = require_fixture("spring-petclinic");
    let config = load_project(&root).expect("load project config");
    assert!(
        !config.source_roots.is_empty(),
        "expected project loader to discover source roots, got: {config:#?}"
    );

    let app_file =
        root.join("src/main/java/org/springframework/samples/petclinic/PetClinicApplication.java");
    assert!(app_file.is_file(), "expected {app_file:?} to exist");
    assert_file_is_under_source_root(&config, &app_file);
}

#[test]
#[ignore]
fn spring_petclinic_loads_main_java_source_root() {
    init_cache_dir();
    if !should_run_fixture("spring-petclinic") {
        return;
    }

    let root = require_fixture("spring-petclinic");
    let config = load_project(&root).expect("load project config");
    assert!(
        config
            .source_roots
            .iter()
            .any(|r| r.path.ends_with(Path::new("src/main/java"))),
        "expected loader to discover src/main/java as a source root, got: {config:#?}"
    );
}

#[test]
#[ignore]
fn guava_smoke() {
    init_cache_dir();
    if !should_run_fixture("guava") {
        return;
    }

    // The top-level Guava checkout is a Maven multi-module workspace. Index only the
    // core `guava/` module so this test stays bounded.
    let root = require_fixture("guava").join("guava");
    assert!(root.is_dir(), "expected {root:?} to exist");

    let config = load_project(&root).expect("load project config");
    assert!(
        config.source_roots.iter().any(|r| r.path.ends_with("src")),
        "expected Maven loader to detect legacy `src/` source root, got: {config:#?}"
    );

    let optional_file = root.join("src/com/google/common/base/Optional.java");
    assert!(
        optional_file.is_file(),
        "expected {optional_file:?} to exist"
    );
    assert_file_is_under_source_root(&config, &optional_file);
}

#[test]
#[ignore]
fn maven_resolver_smoke() {
    init_cache_dir();
    if !should_run_fixture("maven-resolver") {
        return;
    }

    // Index the smallest module that still contains `RepositorySystem`.
    let root = require_fixture("maven-resolver").join("maven-resolver-api");
    assert!(root.is_dir(), "expected {root:?} to exist");

    let config = load_project(&root).expect("load project config");
    let repo_system = root.join("src/main/java/org/eclipse/aether/RepositorySystem.java");
    assert!(repo_system.is_file(), "expected {repo_system:?} to exist");
    assert_file_is_under_source_root(&config, &repo_system);
}
