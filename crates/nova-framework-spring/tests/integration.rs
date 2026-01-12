mod fixture_snapshots;
mod suite;

#[test]
fn integration_tests_are_consolidated_into_integration_rs() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut root_rs_files = Vec::new();

    for entry in std::fs::read_dir(&tests_dir).expect("read tests/ directory") {
        let entry = entry.expect("read tests/ entry");
        let path = entry.path();

        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
            root_rs_files.push(
                path.file_name()
                    .expect("tests/ .rs file name")
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }

    root_rs_files.sort();
    assert_eq!(root_rs_files, vec!["integration.rs"]);
}
