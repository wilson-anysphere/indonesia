use nova_index::Index;
use nova_refactor::{
    change_signature, workspace_edit_to_lsp, ChangeSignature, ChangeSignatureConflict, FileId,
    HierarchyPropagation, ParameterOperation, WorkspaceEdit,
};
use nova_types::MethodId;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn apply_workspace_edit(files: &mut BTreeMap<String, String>, edit: WorkspaceEdit) {
    let input: BTreeMap<FileId, String> = files
        .iter()
        .map(|(file, text)| (FileId::new(file.clone()), text.clone()))
        .collect();
    let out = nova_refactor::apply_workspace_edit(&input, &edit).expect("apply workspace edit");
    *files = out.into_iter().map(|(file, text)| (file.0, text)).collect();
}

fn build_index(files: Vec<(&str, &str)>) -> (Index, BTreeMap<String, String>) {
    let mut map = BTreeMap::new();
    for (uri, text) in files {
        map.insert(uri.to_string(), text.to_string());
    }
    (Index::new(map.clone()), map)
}

fn method_id(index: &Index, class: &str, name: &str, param_types: &[&str]) -> MethodId {
    let wanted: Vec<String> = param_types.iter().map(|s| s.to_string()).collect();
    let id = index
        .method_symbol_id_by_signature(class, name, &wanted)
        .unwrap_or_else(|| panic!("method not found: {class}.{name}({wanted:?})"));
    MethodId(id.0)
}

#[test]
fn reorder_params_updates_calls() {
    let (index, mut files) = build_index(vec![(
        "file:///A.java",
        r#"class A {
    int sum(int a, int b) {
        return a + b;
    }

    void test() {
        int x = sum(1, 2);
    }
}
"#,
    )]);

    let target = method_id(&index, "A", "sum", &["int", "int"]);
    let change = ChangeSignature {
        target,
        new_name: None,
        parameters: vec![
            ParameterOperation::Existing {
                old_index: 1,
                new_name: None,
                new_type: None,
            },
            ParameterOperation::Existing {
                old_index: 0,
                new_name: None,
                new_type: None,
            },
        ],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let edit = change_signature(&index, &change).expect("refactor succeeds");
    apply_workspace_edit(&mut files, edit);

    assert_eq!(
        files.get("file:///A.java").unwrap(),
        r#"class A {
    int sum(int b, int a) {
        return a + b;
    }

    void test() {
        int x = sum(2, 1);
    }
}
"#
    );
}

#[test]
fn add_param_with_default_updates_calls() {
    let (index, mut files) = build_index(vec![(
        "file:///A.java",
        r#"class A {
    void log(String msg) {
    }

    void test() {
        log("hi");
    }
}
"#,
    )]);

    let target = method_id(&index, "A", "log", &["String"]);
    let change = ChangeSignature {
        target,
        new_name: None,
        parameters: vec![
            ParameterOperation::Existing {
                old_index: 0,
                new_name: None,
                new_type: None,
            },
            ParameterOperation::Add {
                name: "level".to_string(),
                ty: "int".to_string(),
                default_value: Some("0".to_string()),
            },
        ],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let edit = change_signature(&index, &change).expect("refactor succeeds");
    apply_workspace_edit(&mut files, edit);

    assert_eq!(
        files.get("file:///A.java").unwrap(),
        r#"class A {
    void log(String msg, int level) {
    }

    void test() {
        log("hi", 0);
    }
}
"#
    );
}

#[test]
fn rename_method_updates_overrides_and_calls() {
    let (index, mut files) = build_index(vec![
        (
            "file:///A.java",
            r#"class A {
    void foo(int x) {
    }
}
"#,
        ),
        (
            "file:///B.java",
            r#"class B extends A {
    void foo(int x) {
        foo(x);
    }
}
"#,
        ),
        (
            "file:///Main.java",
            r#"class Main {
    void test() {
        new B().foo(1);
    }
}
"#,
        ),
    ]);
    let target = method_id(&index, "A", "foo", &["int"]);
    let change = ChangeSignature {
        target,
        new_name: Some("bar".to_string()),
        parameters: vec![ParameterOperation::Existing {
            old_index: 0,
            new_name: None,
            new_type: None,
        }],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::Both,
    };

    let edit = change_signature(&index, &change).expect("refactor succeeds");
    apply_workspace_edit(&mut files, edit);

    assert_eq!(
        files.get("file:///A.java").unwrap(),
        r#"class A {
    void bar(int x) {
    }
}
"#
    );
    assert_eq!(
        files.get("file:///B.java").unwrap(),
        r#"class B extends A {
    void bar(int x) {
        bar(x);
    }
}
"#
    );
    assert_eq!(
        files.get("file:///Main.java").unwrap(),
        r#"class Main {
    void test() {
        new B().bar(1);
    }
}
"#
    );
}

#[test]
fn generic_param_types_do_not_split_and_resolve_overloads() {
    let (index, mut files) = build_index(vec![(
        "file:///A.java",
        r#"class Map<K, V> {}

class A {
    void foo(Map<String, Integer> map) {
    }

    void foo(int x) {
    }

    void test() {
        foo(new Map<String, Integer>());
        foo(1);
    }
}
"#,
    )]);

    // Assert that the sketch index preserves generic type args as a single parameter type entry
    // (i.e. it does not split on commas inside `<...>`).
    let wanted = vec!["Map<String, Integer>".to_string()];
    let sym_id = index
        .method_symbol_id_by_signature("A", "foo", &wanted)
        .expect("index param type extraction should keep `Map<String, Integer>` intact");
    let normalized = vec![nova_index::normalize_type_signature(&wanted[0])];
    assert_eq!(
        index.method_param_types(sym_id).unwrap(),
        normalized.as_slice()
    );

    let target = MethodId(sym_id.0);
    let change = ChangeSignature {
        target,
        new_name: Some("bar".to_string()),
        parameters: vec![ParameterOperation::Existing {
            old_index: 0,
            new_name: None,
            new_type: None,
        }],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let edit = change_signature(&index, &change).expect("refactor succeeds");
    apply_workspace_edit(&mut files, edit);

    assert_eq!(
        files.get("file:///A.java").unwrap(),
        r#"class Map<K, V> {}

class A {
    void bar(Map<String, Integer> map) {
    }

    void foo(int x) {
    }

    void test() {
        bar(new Map<String, Integer>());
        foo(1);
    }
}
"#
    );
}

#[test]
fn rename_annotation_value_element_rewrites_shorthand_usages() {
    let (index, mut files) = build_index(vec![(
        "file:///A.java",
        r#"@interface A {
    int value();
}

@A(1)
class Use {
}
"#,
    )]);

    let target = method_id(&index, "A", "value", &[]);
    let change = ChangeSignature {
        target,
        new_name: Some("v".to_string()),
        parameters: vec![],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let edit = change_signature(&index, &change).expect("refactor succeeds");
    apply_workspace_edit(&mut files, edit);

    assert_eq!(
        files.get("file:///A.java").unwrap(),
        r#"@interface A {
    int v();
}

@A(v = 1)
class Use {
}
"#
    );
}

#[test]
fn rename_annotation_value_element_rewrites_named_pairs() {
    let (index, mut files) = build_index(vec![(
        "file:///A.java",
        r#"@interface A {
    int value();
}

@A(value = 1)
class Use {
}
"#,
    )]);

    let target = method_id(&index, "A", "value", &[]);
    let change = ChangeSignature {
        target,
        new_name: Some("v".to_string()),
        parameters: vec![],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let edit = change_signature(&index, &change).expect("refactor succeeds");
    apply_workspace_edit(&mut files, edit);

    assert_eq!(
        files.get("file:///A.java").unwrap(),
        r#"@interface A {
    int v();
}

@A(v = 1)
class Use {
}
"#
    );
}

#[test]
fn rename_annotation_value_element_skips_other_qualified_annotation_types() {
    let (index, mut files) = build_index(vec![
        (
            "file:///p/A.java",
            r#"package p;

@interface A {
    int value();
}
"#,
        ),
        (
            "file:///q/A.java",
            r#"package q;

@interface A {
    int value();
}
"#,
        ),
        (
            "file:///Use.java",
            r#"import p.A;

@A(1)
@q.A(2)
class Use {}
"#,
        ),
    ]);

    let target = method_id(&index, "A", "value", &[]);
    let change = ChangeSignature {
        target,
        new_name: Some("v".to_string()),
        parameters: vec![],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let edit = change_signature(&index, &change).expect("refactor succeeds");
    apply_workspace_edit(&mut files, edit);

    assert_eq!(
        files.get("file:///p/A.java").unwrap(),
        r#"package p;

@interface A {
    int v();
}
"#
    );
    assert_eq!(
        files.get("file:///q/A.java").unwrap(),
        r#"package q;

@interface A {
    int value();
}
"#
    );
    assert_eq!(
        files.get("file:///Use.java").unwrap(),
        r#"import p.A;

@A(v = 1)
@q.A(2)
class Use {}
"#
    );
}

#[test]
fn conflict_removed_param_still_used_in_body() {
    let (index, _files) = build_index(vec![(
        "file:///A.java",
        r#"class A {
    int add(int a, int b) {
        return a + b;
    }
}
"#,
    )]);

    let target = method_id(&index, "A", "add", &["int", "int"]);
    let change = ChangeSignature {
        target,
        new_name: None,
        parameters: vec![ParameterOperation::Existing {
            old_index: 0,
            new_name: None,
            new_type: None,
        }],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let err = change_signature(&index, &change).expect_err("should conflict");
    assert!(
        err.conflicts
            .iter()
            .any(|c| matches!(c, ChangeSignatureConflict::RemovedParameterStillUsed { .. })),
        "expected RemovedParameterStillUsed, got: {:?}",
        err.conflicts
    );
}

#[test]
fn conflict_overload_collision() {
    let (index, _files) = build_index(vec![(
        "file:///A.java",
        r#"class A {
    void foo(int a) {
    }

    void foo(String a) {
    }
}
"#,
    )]);

    let target = method_id(&index, "A", "foo", &["String"]);
    let change = ChangeSignature {
        target,
        new_name: None,
        parameters: vec![ParameterOperation::Existing {
            old_index: 0,
            new_name: None,
            new_type: Some("int".to_string()),
        }],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let err = change_signature(&index, &change).expect_err("should conflict");
    assert!(
        err.conflicts
            .iter()
            .any(|c| matches!(c, ChangeSignatureConflict::OverloadCollision { .. })),
        "expected OverloadCollision, got: {:?}",
        err.conflicts
    );
}

#[test]
fn unicode_identifiers_round_trip_to_utf16_lsp_positions() {
    let source = "class A {\n    int sum(int a, int b) {\n        return a + b;\n    }\n\n    void test() {\n        int 𝒂 = sum(1, 2);\n    }\n}\n";
    let (index, _files) = build_index(vec![("file:///A.java", source)]);

    let target = method_id(&index, "A", "sum", &["int", "int"]);
    let change = ChangeSignature {
        target,
        new_name: None,
        parameters: vec![
            ParameterOperation::Existing {
                old_index: 1,
                new_name: None,
                new_type: None,
            },
            ParameterOperation::Existing {
                old_index: 0,
                new_name: None,
                new_type: None,
            },
        ],
        new_return_type: None,
        new_throws: None,
        propagate_hierarchy: HierarchyPropagation::None,
    };

    let edit = change_signature(&index, &change).expect("refactor succeeds");
    let lsp_edit = workspace_edit_to_lsp(&index, &edit).expect("convert to lsp");

    let uri: lsp_types::Uri = "file:///A.java".parse().unwrap();
    let changes = lsp_edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for A.java");

    let call_edit = edits
        .iter()
        .find(|edit| edit.new_text == "sum(2, 1)")
        .expect("call edit");

    // The identifier `𝒂` is a non-BMP character. In UTF-16 it occupies two code units,
    // so the `sum` call starts at character 17, not 16.
    assert_eq!(call_edit.range.start.line, 6);
    assert_eq!(call_edit.range.start.character, 17);
    assert_eq!(call_edit.range.end.line, 6);
    assert_eq!(call_edit.range.end.character, 26);
}

#[test]
fn signature_lookup_handles_generic_commas_inside_type_arguments() {
    let (index, _files) = build_index(vec![(
        "file:///A.java",
        r#"class A {
    void foo(java.util.Map<String, Integer> m) {}
}
"#,
    )]);

    let sig = vec!["java.util.Map<String,Integer>".to_string()];
    let id = index
        .method_symbol_id_by_signature("A", "foo", &sig)
        .expect("foo(Map<String,Integer>) should be indexed");
    assert_eq!(index.find_symbol(id).unwrap().name, "foo");
}
