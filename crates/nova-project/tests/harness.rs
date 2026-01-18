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
                    .expect("test file path missing file name")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }

    root_rs_files.sort();
    assert_eq!(
        root_rs_files,
        std::slice::from_ref(&expected),
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
                .expect("test suite file path missing file name")
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
