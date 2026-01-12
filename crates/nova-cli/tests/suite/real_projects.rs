use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn nova() -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nova"));
    cmd.env("NOVA_CACHE_DIR", cache_dir());
    cmd
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

fn cache_dir() -> &'static Path {
    static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();
    CACHE_DIR.get_or_init(|| {
        let dir = repo_root().join("target").join("nova-real-project-cache");
        std::fs::create_dir_all(&dir).expect("create NOVA_CACHE_DIR");
        dir
    })
}

fn index_and_query_symbols(root: &Path, query: &str, expected_file_contains: &str) {
    let output = nova()
        .arg("index")
        .arg(root)
        .arg("--json")
        .output()
        .expect("run nova index");
    assert!(
        output.status.success(),
        "nova index failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let output = nova()
        .arg("symbols")
        .arg(query)
        .arg("--path")
        .arg(root)
        .arg("--json")
        .output()
        .expect("run nova symbols");
    assert!(
        output.status.success(),
        "nova symbols failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse nova symbols JSON");
    let results = value
        .as_array()
        .expect("nova symbols JSON should be an array");
    assert!(
        !results.is_empty(),
        "expected nova symbols to return results for query {query:?}"
    );

    let expected_file_contains = expected_file_contains.replace('\\', "/");
    let found = results.iter().any(|sym| {
        let mut locations = sym.get("location").into_iter().chain(
            sym.get("locations")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten(),
        );
        locations.any(|loc| {
            loc.get("file")
                .and_then(|file| file.as_str())
                .is_some_and(|file| file.replace('\\', "/").contains(&expected_file_contains))
        })
    });

    assert!(
        found,
        "expected nova symbols {query:?} to include a location with path containing {expected_file_contains:?}, got:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
#[ignore]
fn spring_petclinic_cli_smoke() {
    if !should_run_fixture("spring-petclinic") {
        return;
    }
    let root = require_fixture("spring-petclinic").join("src/main/java");
    assert!(root.is_dir(), "expected {root:?} to exist");
    index_and_query_symbols(
        &root,
        "PetClinicApplication",
        "src/main/java/org/springframework/samples/petclinic/PetClinicApplication.java",
    );
}

#[test]
#[ignore]
fn guava_cli_smoke() {
    if !should_run_fixture("guava") {
        return;
    }

    // Guava is a Maven multi-module workspace; index the `guava/` module only so
    // this test stays bounded and repeatable.
    let root = require_fixture("guava").join("guava");
    assert!(root.is_dir(), "expected {root:?} to exist");
    index_and_query_symbols(
        &root,
        "Optional",
        "src/com/google/common/base/Optional.java",
    );
}

#[test]
#[ignore]
fn maven_resolver_cli_smoke() {
    if !should_run_fixture("maven-resolver") {
        return;
    }
    let root = require_fixture("maven-resolver").join("maven-resolver-api/src/main/java");
    assert!(root.is_dir(), "expected {root:?} to exist");
    index_and_query_symbols(
        &root,
        "RepositorySystem",
        "src/main/java/org/eclipse/aether/RepositorySystem.java",
    );
}
