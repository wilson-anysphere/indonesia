use std::collections::BTreeMap;

use nova_refactor::{apply_workspace_edit, convert_to_record, ConvertToRecordOptions, FileId};
use pretty_assertions::assert_eq;

fn apply_edit(source: &str, file: &str, edit: &nova_refactor::WorkspaceEdit) -> String {
    let mut files = BTreeMap::new();
    let file_id = FileId::new(file.to_string());
    files.insert(file_id.clone(), source.to_string());
    let updated = apply_workspace_edit(&files, edit).expect("apply workspace edit");
    updated
        .get(&file_id)
        .cloned()
        .expect("expected workspace edit to update the source file")
}

#[test]
fn converts_minimal_class_and_formats_empty_record_body() {
    let file = "file:///Test.java";
    let source = concat!(
        "public final class Point {\n",
        "    private final int x;\n",
        "    private final int y;\n",
        "\n",
        "    public Point(int x, int y) {\n",
        "        this.x = x;\n",
        "        this.y = y;\n",
        "    }\n",
        "}\n",
    );

    let cursor = source.find("class Point").unwrap();
    let edit = convert_to_record(file, source, cursor, ConvertToRecordOptions::default()).unwrap();

    let result = apply_edit(source, file, &edit);
    assert_eq!(result, "public record Point(int x, int y) {\n}\n");
}

#[test]
fn converts_messy_class_and_formats_record_members() {
    let file = "file:///Test.java";
    let source = concat!(
        "public final class Point{",
        "private final int x;",
        "private final int y;",
        "public Point(int x,int y){this.x = x;this.y = y;}",
        "public int sum(){return x+y;}",
        "}\n",
    );

    let cursor = source.find("class Point").unwrap();
    let edit = convert_to_record(file, source, cursor, ConvertToRecordOptions::default()).unwrap();

    let result = apply_edit(source, file, &edit);
    assert_eq!(
        result,
        concat!(
            "public record Point(int x, int y) {\n",
            "    public int sum() {\n",
            "        return x + y;\n",
            "    }\n",
            "}\n",
        )
    );
}
