// Consolidated integration test harness.
//
// Each `tests/*.rs` file becomes a separate Cargo integration test binary. Under
// the `cargo_agent` RLIMIT_AS constraints this is expensive, so `nova-dap`
// intentionally uses a single harness file (`tests/real_jvm.rs`) that `mod`s the rest of the suite.
mod harness;
mod suite;

#[test]
fn harness_is_single_root_test_file() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");

    let expected = std::path::Path::new(file!())
        .file_name()
        .expect("harness filename is missing")
        .to_string_lossy()
        .into_owned();

    let mut root_rs_files = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).unwrap_or_else(|err| {
        panic!(
            "failed to read nova-dap tests dir {}: {err}",
            tests_dir.display()
        )
    }) {
        let entry = entry
            .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", tests_dir.display()));
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
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
        expected, "real_jvm.rs",
        "expected nova-dap integration test harness to be named real_jvm.rs (so `cargo test --locked -p nova-dap --test real_jvm` works); got: {expected}"
    );

    assert_eq!(
        root_rs_files,
        [expected.clone()],
        "expected a single root integration test harness file (tests/{expected}); found: {root_rs_files:?}"
    );
}
