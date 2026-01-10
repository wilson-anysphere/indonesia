use assert_cmd::Command;
use std::path::{Path, PathBuf};

fn nova() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("nova"))
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

fn index_and_query_symbols(root: &Path, query: &str) {
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
}

#[test]
#[ignore]
fn spring_petclinic_cli_smoke() {
    let root = require_fixture("spring-petclinic").join("src/main/java");
    assert!(root.is_dir(), "expected {root:?} to exist");
    index_and_query_symbols(&root, "PetClinicApplication");
}

#[test]
#[ignore]
fn guava_cli_smoke() {
    // Guava uses a non-standard Maven source layout (`src/...` instead of
    // `src/main/java`). Nova's Maven loader doesn't understand that yet, so
    // indexing currently returns zero source files. This smoke test focuses on
    // ensuring the CLI doesn't crash and that file parsing works on real Guava
    // sources.
    let root = require_fixture("guava");

    let optional = root.join("guava/src/com/google/common/base/Optional.java");
    assert!(optional.is_file(), "expected {optional:?} to exist");
    nova()
        .arg("parse")
        .arg(&optional)
        .arg("--json")
        .assert()
        .success();

    nova()
        .arg("index")
        .arg(&root)
        .arg("--json")
        .assert()
        .success();
}

#[test]
#[ignore]
fn maven_resolver_cli_smoke() {
    let root = require_fixture("maven-resolver").join("maven-resolver-api/src/main/java");
    assert!(root.is_dir(), "expected {root:?} to exist");
    index_and_query_symbols(&root, "RepositorySystem");
}
