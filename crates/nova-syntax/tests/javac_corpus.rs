use std::fs;
use std::path::{Path, PathBuf};

use nova_syntax::{parse_java, parse_module_info};
use nova_test_utils::javac::{javac_available, run_javac_files_with_options, JavacOptions};

fn collect_java_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).expect("fixture directory readable") {
        let entry = entry.expect("fixture directory entry readable");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("java") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn javac_opts() -> JavacOptions {
    // Keep the corpus compatible with widely-available JDKs, while still exercising
    // modern syntax.
    JavacOptions {
        release: Some(17),
        ..JavacOptions::default()
    }
}

#[test]
fn javac_corpus_ok() {
    if !javac_available() {
        eprintln!("javac not found in PATH; skipping");
        return;
    }

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/javac/ok");
    let files = collect_java_files(&dir);
    assert!(!files.is_empty(), "no ok corpus files found at {}", dir.display());

    let opts = javac_opts();

    for path in files {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("fixture filename utf-8")
            .to_string();
        let src = fs::read_to_string(&path).expect("fixture readable");

        let out = run_javac_files_with_options(&[(&filename, &src)], &opts)
            .expect("javac invocation succeeds");
        assert!(
            out.success(),
            "javac failed for {}:\n{}",
            filename,
            out.stderr
        );

        if filename == "module-info.java" {
            parse_module_info(&src).expect("module-info should parse");
        } else {
            let parsed = parse_java(&src);
            assert!(
                parsed.errors.is_empty(),
                "Nova parse errors in {}: {:#?}",
                filename,
                parsed.errors
            );
        }
    }
}

#[test]
fn javac_corpus_err() {
    if !javac_available() {
        eprintln!("javac not found in PATH; skipping");
        return;
    }

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/javac/err");
    let files = collect_java_files(&dir);
    assert!(
        !files.is_empty(),
        "no err corpus files found at {}",
        dir.display()
    );

    let opts = javac_opts();

    for path in files {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("fixture filename utf-8")
            .to_string();
        let src = fs::read_to_string(&path).expect("fixture readable");

        let out = run_javac_files_with_options(&[(&filename, &src)], &opts)
            .expect("javac invocation succeeds");
        assert!(
            !out.success(),
            "expected javac failure for {}, but javac succeeded",
            filename
        );

        let diags = out.diagnostics();
        assert!(
            !diags.is_empty(),
            "expected at least one diagnostic for {}:\n{}",
            filename,
            out.stderr
        );

        if filename == "module-info.java" {
            assert!(
                parse_module_info(&src).is_err(),
                "expected module-info parse error for {}",
                filename
            );
        } else {
            let parsed = parse_java(&src);
            assert!(
                !parsed.errors.is_empty(),
                "expected Nova parse errors for {}, but got none",
                filename
            );
        }
    }
}

