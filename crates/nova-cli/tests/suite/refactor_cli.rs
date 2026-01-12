use assert_cmd::Command;
use assert_fs::TempDir;
use std::fs;
use std::path::{Path, PathBuf};

fn nova() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("nova"))
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ty = entry.file_type().unwrap();
        if ty.is_dir() {
            copy_dir_recursive(&src_path, &dst_path);
        } else if ty.is_file() {
            fs::copy(&src_path, &dst_path).unwrap();
        }
    }
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let ty = entry.file_type().unwrap();
            if ty.is_dir() {
                walk(&path, base, out);
            } else if ty.is_file() {
                let rel = path.strip_prefix(base).unwrap().to_path_buf();
                out.push(rel);
            }
        }
    }

    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn assert_tree_matches(expected: &Path, actual: &Path) {
    let expected_files = collect_files(expected);
    let actual_files = collect_files(actual);
    assert_eq!(
        actual_files, expected_files,
        "file sets differ.\nexpected: {expected_files:#?}\nactual: {actual_files:#?}"
    );

    for rel in expected_files {
        let expected_path = expected.join(&rel);
        let actual_path = actual.join(&rel);
        let expected_bytes = fs::read(&expected_path).unwrap();
        let actual_bytes = fs::read(&actual_path).unwrap();
        assert_eq!(
            actual_bytes,
            expected_bytes,
            "file contents differ for {}",
            rel.display()
        );
    }
}

#[test]
fn format_in_place_matches_fixture() {
    let fixture = fixture_root().join("format");
    let before = fixture.join("before");
    let after = fixture.join("after");

    let temp = TempDir::new().unwrap();
    copy_dir_recursive(&before, temp.path());

    nova()
        .current_dir(temp.path())
        .arg("format")
        .arg("src/Foo.java")
        .arg("--in-place")
        .assert()
        .success();

    assert_tree_matches(&after, temp.path());
}

#[test]
fn organize_imports_in_place_matches_fixture() {
    let fixture = fixture_root().join("organize_imports");
    let before = fixture.join("before");
    let after = fixture.join("after");

    let temp = TempDir::new().unwrap();
    copy_dir_recursive(&before, temp.path());

    nova()
        .current_dir(temp.path())
        .arg("organize-imports")
        .arg("src/Test.java")
        .arg("--in-place")
        .assert()
        .success();

    assert_tree_matches(&after, temp.path());
}

#[test]
fn rename_in_place_matches_fixture() {
    let fixture = fixture_root().join("rename");
    let before = fixture.join("before");
    let after = fixture.join("after");

    let temp = TempDir::new().unwrap();
    copy_dir_recursive(&before, temp.path());

    nova()
        .current_dir(temp.path())
        .arg("refactor")
        .arg("rename")
        .arg("src/Test.java")
        .arg("--line")
        .arg("3")
        .arg("--col")
        .arg("10")
        .arg("--new-name")
        .arg("bar")
        .arg("--in-place")
        .assert()
        .success();

    assert_tree_matches(&after, temp.path());
}
