mod remote_rpc_util;
mod suite;

#[test]
fn only_one_root_integration_test_harness_exists() {
    use std::path::Path;

    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut root_rs_files = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).expect("read tests dir") {
        let entry = entry.expect("read tests dir entry");
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "rs") {
            continue;
        }
        root_rs_files.push(path);
    }
    root_rs_files.sort();

    let expected = vec![tests_dir.join("integration.rs")];
    assert_eq!(
        root_rs_files, expected,
        "expected exactly one root integration test harness; move any additional `tests/*.rs` files into `tests/suite/` and wire them from `tests/integration.rs`"
    );
}
