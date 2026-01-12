use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nova_project::load_project;
use nova_workspace::{Workspace, WorkspaceSymbol};

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

fn assert_symbols_contain_file(symbols: &[WorkspaceSymbol], expected_file: &str) {
    let expected_file = expected_file.replace('\\', "/");
    let found = symbols.iter().any(|sym| {
        sym.location.file.replace('\\', "/") == expected_file
            || sym
                .location
                .file
                .replace('\\', "/")
                .ends_with(&expected_file)
    });
    assert!(
        found,
        "expected workspace symbols to include a location in {expected_file:?}, got: {symbols:#?}"
    );
}

fn assert_parses(ws: &Workspace, file: &Path) {
    let parsed = ws
        .parse_file(file)
        .unwrap_or_else(|err| panic!("parse_file({}): {err:#}", file.display()));
    assert!(
        !parsed.tree.trim().is_empty(),
        "expected parse tree for {}, got empty output",
        file.display()
    );
    assert!(
        parsed.tree.contains("CompilationUnit"),
        "expected parse tree for {} to contain CompilationUnit, got:\n{}",
        file.display(),
        parsed.tree
    );
}

fn assert_diagnostics(ws: &Workspace, file: &Path) {
    let diagnostics = ws
        .diagnostics(file)
        .unwrap_or_else(|err| panic!("diagnostics({}): {err:#}", file.display()));
    assert_eq!(
        diagnostics.root.as_path(),
        ws.root(),
        "expected diagnostics to report the same workspace root"
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
    let ws = Workspace::open(&root).expect("open workspace");

    let report = ws.index_and_write_cache().expect("index project");
    assert!(
        report.metrics.files_indexed > 0,
        "expected indexing to scan files, got: {report:#?}"
    );

    let symbols = ws
        .workspace_symbols("PetClinicApplication")
        .expect("workspace symbols");
    assert!(
        !symbols.is_empty(),
        "expected PetClinicApplication to appear in workspace symbols"
    );
    assert_symbols_contain_file(
        &symbols,
        "src/main/java/org/springframework/samples/petclinic/PetClinicApplication.java",
    );

    let app_file =
        root.join("src/main/java/org/springframework/samples/petclinic/PetClinicApplication.java");
    assert!(app_file.is_file(), "expected {app_file:?} to exist");
    assert_parses(&ws, &app_file);
    assert_diagnostics(&ws, &app_file);
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

    let ws = Workspace::open(&root).expect("open workspace");

    let report = ws.index_and_write_cache().expect("index project");
    assert!(
        report.metrics.files_indexed > 0,
        "expected indexing to scan files, got: {report:#?}"
    );

    let symbols = ws.workspace_symbols("Optional").expect("workspace symbols");
    assert!(
        !symbols.is_empty(),
        "expected Optional to appear in workspace symbols"
    );
    assert_symbols_contain_file(&symbols, "src/com/google/common/base/Optional.java");

    let optional_file = root.join("src/com/google/common/base/Optional.java");
    assert!(
        optional_file.is_file(),
        "expected {optional_file:?} to exist"
    );
    assert_parses(&ws, &optional_file);
    assert_diagnostics(&ws, &optional_file);
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

    let ws = Workspace::open(&root).expect("open workspace");

    let report = ws.index_and_write_cache().expect("index project");
    assert!(
        report.metrics.files_indexed > 0,
        "expected indexing to scan files, got: {report:#?}"
    );

    let symbols = ws
        .workspace_symbols("RepositorySystem")
        .expect("workspace symbols");
    assert!(
        !symbols.is_empty(),
        "expected RepositorySystem to appear in workspace symbols"
    );
    assert_symbols_contain_file(
        &symbols,
        "src/main/java/org/eclipse/aether/RepositorySystem.java",
    );

    let repo_system = root.join("src/main/java/org/eclipse/aether/RepositorySystem.java");
    assert!(repo_system.is_file(), "expected {repo_system:?} to exist");
    assert_parses(&ws, &repo_system);
    assert_diagnostics(&ws, &repo_system);
}

