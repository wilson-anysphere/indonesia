use nova_index::{Index, SymbolKind};
use nova_refactor::{
    change_signature, workspace_edit_to_lsp, ChangeSignature, ChangeSignatureConflict, FileId,
    HierarchyPropagation, ParameterOperation, WorkspaceEdit,
};
use nova_types::MethodId;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn apply_workspace_edit(files: &mut BTreeMap<String, String>, mut edit: WorkspaceEdit) {
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
    for sym in index.symbols() {
        if sym.kind != SymbolKind::Method {
            continue;
        }
        if sym.container.as_deref() != Some(class) {
            continue;
        }
        if sym.name != name {
            continue;
        }
        let parsed = parse_param_types(index, sym);
        if parsed == wanted {
            return MethodId(sym.id.0);
        }
    }
    panic!("method not found: {class}.{name}({wanted:?})");
}

fn parse_param_types(index: &Index, sym: &nova_index::Symbol) -> Vec<String> {
    let text = index.file_text(&sym.file).expect("file text");
    let bytes = text.as_bytes();
    let mut open = sym.name_range.end;
    while open < bytes.len() && bytes[open].is_ascii_whitespace() {
        open += 1;
    }
    assert_eq!(
        bytes.get(open),
        Some(&b'('),
        "expected `(` after method name"
    );
    let close = find_matching_paren(text, open).expect("matching paren");
    let params_src = &text[open + 1..close - 1];
    parse_params(params_src)
}

fn parse_params(params: &str) -> Vec<String> {
    let params = params.trim();
    if params.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for part in split_top_level(params, ',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = p.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let ty = tokens[..tokens.len() - 1].join(" ");
        out.push(ty);
    }
    out
}

fn find_matching_paren(text: &str, open_paren: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open_paren;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b'"' => {
                // Skip strings
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level(text: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match ch {
            '"' => in_string = true,
            '(' => depth_paren += 1,
            ')' => depth_paren -= 1,
            '[' => depth_brack += 1,
            ']' => depth_brack -= 1,
            '{' => depth_brace += 1,
            '}' => depth_brace -= 1,
            _ => {}
        }

        if ch == sep && depth_paren == 0 && depth_brack == 0 && depth_brace == 0 {
            out.push(text[start..i].to_string());
            start = i + 1;
        }
        i += 1;
    }
    out.push(text[start..].to_string());
    out
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
