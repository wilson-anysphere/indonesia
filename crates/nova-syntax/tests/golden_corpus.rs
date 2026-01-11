use pretty_assertions::assert_eq;

use nova_syntax::{parse_java, ParseError, SyntaxNode};
use rowan::NodeOrToken;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[test]
fn golden_corpus() -> io::Result<()> {
    let bless = std::env::var_os("BLESS").is_some();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let testdata_dir = manifest_dir.join("testdata");

    let parser_root = testdata_dir.join("parser");
    let recovery_root = testdata_dir.join("recovery");

    run_parser_fixtures(&parser_root, bless)?;
    run_recovery_fixtures(&recovery_root, bless)?;

    Ok(())
}

fn run_parser_fixtures(root: &Path, bless: bool) -> io::Result<()> {
    let fixtures = collect_java_files(root)?;

    for java_path in fixtures {
        let input = read_normalized(&java_path)?;
        let parsed = parse_java(&input);

        if !parsed.errors.is_empty() {
            let errors_dump = format_errors(&input, &parsed.errors);
            panic!(
                "expected no parse errors for parser fixture `{}`\n{}",
                java_path.display(),
                errors_dump
            );
        }

        let tree_dump = normalize_newlines(&debug_dump(&parsed.syntax()));
        let tree_path = java_path.with_extension("tree");

        if bless {
            write_if_changed(&tree_path, &tree_dump)?;
        } else {
            let expected = read_expected(&tree_path)?;
            assert_eq!(
                tree_dump, expected,
                "tree mismatch for parser fixture `{}`",
                java_path.display()
            );
        }
    }

    Ok(())
}

fn run_recovery_fixtures(root: &Path, bless: bool) -> io::Result<()> {
    let fixtures = collect_java_files(root)?;

    for java_path in fixtures {
        let input = read_normalized(&java_path)?;
        let parsed = parse_java(&input);

        let tree_dump = normalize_newlines(&debug_dump(&parsed.syntax()));
        let errors_dump = format_errors(&input, &parsed.errors);

        let tree_path = java_path.with_extension("tree");
        let errors_path = java_path.with_extension("errors");

        if bless {
            write_if_changed(&tree_path, &tree_dump)?;
            write_if_changed(&errors_path, &errors_dump)?;
        } else {
            let expected_tree = read_expected(&tree_path)?;
            assert_eq!(
                tree_dump, expected_tree,
                "tree mismatch for recovery fixture `{}`",
                java_path.display()
            );

            let expected_errors = read_expected(&errors_path)?;
            assert_eq!(
                errors_dump, expected_errors,
                "errors mismatch for recovery fixture `{}`",
                java_path.display()
            );
        }
    }

    Ok(())
}

fn collect_java_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    collect_java_files_impl(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_java_files_impl(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_java_files_impl(&path, out)?;
            continue;
        }
        if file_type.is_file() && path.extension() == Some(OsStr::new("java")) {
            out.push(path);
        }
    }
    Ok(())
}

fn read_normalized(path: &Path) -> io::Result<String> {
    let raw = fs::read_to_string(path)?;
    Ok(normalize_newlines(&raw))
}

fn read_expected(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(normalize_newlines(&contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "missing expected file `{}` (run with `BLESS=1` to generate)",
                    path.display()
                ),
            ))
        }
        Err(err) => Err(err),
    }
}

fn write_if_changed(path: &Path, contents: &str) -> io::Result<()> {
    let contents = normalize_newlines(contents);
    if let Ok(existing) = fs::read_to_string(path) {
        if normalize_newlines(&existing) == contents {
            return Ok(());
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn format_errors(source: &str, errors: &[ParseError]) -> String {
    let mut out = String::new();
    for error in errors {
        let offset = error.range.start as usize;
        let (line, col) = byte_offset_to_line_col(source, offset);
        out.push_str(&format!("{line}:{col}: {}\n", error.message));
    }
    out
}

fn byte_offset_to_line_col(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;

    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }

    (line, col)
}

fn debug_dump(node: &SyntaxNode) -> String {
    fn go(node: &SyntaxNode, indent: usize, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(out, "{:indent$}{:?}", "", node.kind(), indent = indent);
        for child in node.children_with_tokens() {
            match child {
                NodeOrToken::Node(n) => go(&n, indent + 2, out),
                NodeOrToken::Token(t) => {
                    let _ = writeln!(
                        out,
                        "{:indent$}{:?} {:?}",
                        "",
                        t.kind(),
                        t.text(),
                        indent = indent + 2
                    );
                }
            }
        }
    }

    let mut out = String::new();
    go(node, 0, &mut out);
    out
}

