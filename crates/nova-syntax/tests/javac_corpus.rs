use std::fs;
use std::path::{Path, PathBuf};

use nova_syntax::{parse_java_with_options, JavaLanguageLevel, ParseOptions, SyntaxKind};
use nova_test_utils::javac::{
    javac_available, javac_version, run_javac_files_with_options, JavacOptions,
};

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

fn nova_opts() -> ParseOptions {
    // Match the `javac --release` used for the corpus compilation.
    ParseOptions {
        language_level: JavaLanguageLevel::JAVA_17,
        ..ParseOptions::default()
    }
}

#[test]
fn javac_corpus_ok() {
    if !javac_available() {
        eprintln!("javac not found in PATH; skipping");
        return;
    }
    if javac_version().is_some_and(|v| v < 17) {
        eprintln!("javac is too old for --release 17; skipping");
        return;
    }

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/javac/ok");
    let files = collect_java_files(&dir);
    assert!(
        !files.is_empty(),
        "no ok corpus files found at {}",
        dir.display()
    );

    let opts = javac_opts();
    let parse_opts = nova_opts();

    for path in files {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("fixture filename utf-8")
            .to_string();
        let src = fs::read_to_string(&path).expect("fixture readable");

        let out = if filename == "module-info.java" {
            // `module-info.java` is only valid as part of a module, so when we
            // test it we include a minimal package from this corpus. This lets
            // us exercise directives like `exports foo;` without forcing every
            // other corpus file into a named module.
            let packaged = fs::read_to_string(dir.join("PackagedClass.java"))
                .expect("PackagedClass.java fixture readable");
            let package_info = fs::read_to_string(dir.join("package-info.java"))
                .expect("package-info.java fixture readable");
            let files = vec![
                ("module-info.java", src.as_str()),
                ("PackagedClass.java", packaged.as_str()),
                ("package-info.java", package_info.as_str()),
            ];
            run_javac_files_with_options(files.as_slice(), &opts)
                .expect("javac invocation succeeds")
        } else {
            run_javac_files_with_options(&[(&filename, &src)], &opts)
                .expect("javac invocation succeeds")
        };
        assert!(
            out.success(),
            "javac failed for {}:\n{}",
            filename,
            out.stderr
        );

        let parsed = parse_java_with_options(&src, parse_opts);
        assert!(
            parsed.result.errors.is_empty(),
            "Nova parse errors in {}: {:#?}",
            filename,
            parsed.result.errors
        );
        assert!(
            parsed.diagnostics.is_empty(),
            "Nova feature-gate diagnostics in {}: {:#?}",
            filename,
            parsed.diagnostics
        );

        if filename == "module-info.java" {
            assert!(
                parsed
                    .result
                    .syntax()
                    .children()
                    .any(|n| n.kind() == SyntaxKind::ModuleDeclaration),
                "expected module declaration in {}",
                filename
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
    if javac_version().is_some_and(|v| v < 17) {
        eprintln!("javac is too old for --release 17; skipping");
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
    let parse_opts = nova_opts();

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

        let parsed = parse_java_with_options(&src, parse_opts);
        assert!(
            !parsed.result.errors.is_empty(),
            "expected Nova parse errors for {}, but got none",
            filename
        );
    }
}
