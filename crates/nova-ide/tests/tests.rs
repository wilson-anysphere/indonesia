// Integration test harness for `nova-ide`.
//
// Keep all integration tests as submodules of this harness (e.g. under `tests/suite/`) rather
// than adding new top-level `tests/*.rs` files, which would compile as additional test binaries
// and significantly increase build/link time (see AGENTS.md).
mod framework_harness;
mod suite;
#[path = "framework_harness/text_fixture.rs"]
mod text_fixture;

#[test]
fn tests_root_contains_only_tests_rs_harness() {
    use std::path::Path;

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests_dir = manifest_dir.join("tests");

    let mut root_rs_files: Vec<String> = std::fs::read_dir(&tests_dir)
        .expect("read nova-ide tests/ directory")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                Some(path.file_name()?.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    root_rs_files.sort();

    assert_eq!(
        root_rs_files,
        vec!["tests.rs".to_string()],
        "unexpected root integration test files in {tests_dir:?}; keep a single tests.rs harness and put modules under tests/suite/"
    );
}
