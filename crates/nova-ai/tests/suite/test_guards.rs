use std::path::PathBuf;

/// Guard against accidentally using the default Tokio multi-thread runtime in integration tests.
///
/// `#[tokio::test]` defaults to the multi-thread runtime, which can spawn many threads (and
/// allocate significant TLS/stack memory) when the Rust test harness runs tests in parallel.
/// Under the agent environment (RLIMIT_AS=4G), this can lead to OOM/abort failures.
///
/// We allow Tokio tests, but require they specify an explicit runtime flavor (e.g.
/// `flavor = "current_thread"` or `flavor = "multi_thread"` with a bounded worker thread count).
#[test]
fn tokio_tests_must_specify_runtime_flavor() {
    let tests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");

    let mut offenders = Vec::new();
    for entry in walkdir::WalkDir::new(&tests_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            continue;
        };

        for (line_no, line) in contents.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                continue;
            }

            // Canonicalize by stripping whitespace so `#[tokio::test ]` is also detected.
            let canonical: String = trimmed.split_whitespace().collect();
            if canonical.starts_with("#[tokio::test]") {
                offenders.push(format!(
                    "{}:{}: {trimmed}",
                    entry.path().display(),
                    line_no + 1
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "Found bare #[tokio::test] attributes (use an explicit runtime flavor, e.g. \
         #[tokio::test(flavor = \"current_thread\")]):\n{}",
        offenders.join("\n")
    );
}

