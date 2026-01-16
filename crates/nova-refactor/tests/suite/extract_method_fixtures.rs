use nova_refactor::extract_method::{ExtractMethod, InsertionStrategy, Visibility};
use nova_refactor::{apply_workspace_edit, FileId, WorkspaceEdit};
use nova_test_utils::{assert_fixture_transformed, extract_range};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn fixture_dir(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn apply_edit(files: &mut BTreeMap<PathBuf, String>, edit: &WorkspaceEdit) {
    let by_id: BTreeMap<FileId, String> = files
        .iter()
        .map(|(path, text)| {
            (
                FileId::new(path.to_string_lossy().into_owned()),
                text.clone(),
            )
        })
        .collect();

    let updated = apply_workspace_edit(&by_id, edit).expect("workspace edit applies cleanly");
    *files = updated
        .into_iter()
        .map(|(file, text)| (PathBuf::from(file.0), text))
        .collect();
}

fn assert_extract_method_fixture(before: &Path, after: &Path) {
    assert_fixture_transformed(before, after, |files| {
        let path = PathBuf::from("Main.java");
        let before_text = files
            .get(&path)
            .expect("fixture must contain Main.java")
            .clone();

        let (source, selection) = extract_range(&before_text);
        files.insert(path.clone(), source.clone());

        let refactoring = ExtractMethod {
            file: "Main.java".to_string(),
            selection,
            name: "extracted".to_string(),
            visibility: Visibility::Private,
            insertion_strategy: InsertionStrategy::AfterCurrentMethod,
        };

        let edit = refactoring.apply(&source).expect("refactoring succeeds");
        apply_edit(files, &edit);
    });
}

#[test]
fn extract_method_multi_statement_selection() {
    assert_extract_method_fixture(
        &fixture_dir("tests/fixtures/extract_method_multi_statement/before"),
        &fixture_dir("tests/fixtures/extract_method_multi_statement/after"),
    );
}

#[test]
fn extract_method_expression() {
    assert_extract_method_fixture(
        &fixture_dir("tests/fixtures/extract_method_expression/before"),
        &fixture_dir("tests/fixtures/extract_method_expression/after"),
    );
}

#[test]
fn extract_method_static_method() {
    assert_extract_method_fixture(
        &fixture_dir("tests/fixtures/extract_method_static_method/before"),
        &fixture_dir("tests/fixtures/extract_method_static_method/after"),
    );
}

#[test]
fn extract_method_try_with_resources_parameter() {
    assert_extract_method_fixture(
        &fixture_dir("tests/fixtures/extract_method_try_with_resources_parameter/before"),
        &fixture_dir("tests/fixtures/extract_method_try_with_resources_parameter/after"),
    );
}

#[test]
fn extract_method_record_compact_constructor() {
    assert_extract_method_fixture(
        &fixture_dir("tests/fixtures/extract_method_record_compact_constructor/before"),
        &fixture_dir("tests/fixtures/extract_method_record_compact_constructor/after"),
    );
}

#[test]
fn extract_method_expression_record_compact_constructor() {
    assert_extract_method_fixture(
        &fixture_dir("tests/fixtures/extract_method_expression_record_compact_constructor/before"),
        &fixture_dir("tests/fixtures/extract_method_expression_record_compact_constructor/after"),
    );
}

#[test]
fn extract_method_multi_statement_record_compact_constructor() {
    assert_extract_method_fixture(
        &fixture_dir(
            "tests/fixtures/extract_method_multi_statement_record_compact_constructor/before",
        ),
        &fixture_dir(
            "tests/fixtures/extract_method_multi_statement_record_compact_constructor/after",
        ),
    );
}
