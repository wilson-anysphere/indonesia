// Integration test harness for `nova-ide`.
//
// Keep integration tests as submodules of this harness (e.g. under `tests/suite/`) rather than
// adding new top-level `tests/*.rs` files, which would compile as additional test binaries and
// significantly increase build/link time (see AGENTS.md).
//
// Exception: `tests/file_navigation_cache.rs` is intentionally a standalone test binary so it can
// run in a fresh process (it validates global-cache behavior).
mod framework_harness;
mod suite;
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
        vec!["file_navigation_cache.rs".to_string(), "tests.rs".to_string()],
        "unexpected root integration test files in {tests_dir:?}; keep `tests.rs` (suite harness) and `file_navigation_cache.rs` (isolated cache test) and put other tests under tests/suite/"
    );
}

#[test]
fn suite_mod_is_in_sync_with_suite_directory() {
    use std::collections::BTreeSet;
    use std::path::Path;

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let suite_dir = manifest_dir.join("tests").join("suite");
    let suite_mod_rs = suite_dir.join("mod.rs");

    let suite_mod_source =
        std::fs::read_to_string(&suite_mod_rs).expect("read nova-ide tests/suite/mod.rs");

    let suite_files: BTreeSet<String> = std::fs::read_dir(&suite_dir)
        .expect("read nova-ide tests/suite directory")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                let stem = path.file_stem()?.to_string_lossy().into_owned();
                if stem == "mod" {
                    None
                } else {
                    Some(stem)
                }
            } else {
                None
            }
        })
        .collect();

    let mod_decls: BTreeSet<String> = {
        let re = regex::Regex::new(r"(?m)^\s*(?:#\[[^\]]*\]\s*)*mod\s+([A-Za-z0-9_]+)\s*;")
            .expect("suite mod.rs module declaration regex");
        re.captures_iter(&suite_mod_source)
            .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
            .collect()
    };

    let missing: Vec<_> = suite_files.difference(&mod_decls).cloned().collect();
    let extra: Vec<_> = mod_decls.difference(&suite_files).cloned().collect();

    assert!(
        missing.is_empty() && extra.is_empty(),
        "tests/suite/mod.rs is out of sync with tests/suite/*.rs (excluding mod.rs).\n\
Missing module declarations for: {missing:?}\n\
Extra module declarations for: {extra:?}"
    );
}
