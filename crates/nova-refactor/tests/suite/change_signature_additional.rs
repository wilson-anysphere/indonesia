use nova_index::Index;
use nova_refactor::{
    change_signature, ChangeSignature, FileId, HierarchyPropagation, ParameterOperation,
    WorkspaceEdit,
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

#[test]
fn change_signature_does_not_rewrite_other_overload_declaration_header() {
    let mut files = BTreeMap::new();
    files.insert(
        "file:///A.java".to_string(),
        r#"class A {
    int foo(int a, int b) { return a + b; }
    int foo(String a, String b) { return 0; }
    void test() {
        int x = foo(1, 2);
        int y = foo("a", "b");
    }
}
"#
        .to_string(),
    );
    let index = Index::new(files.clone());

    let param_types = vec!["int".to_string(), "int".to_string()];
    let target = index
        .method_overload_by_param_types("A", "foo", &param_types)
        .map(|id| MethodId(id.0))
        .expect("method not found: A.foo(int, int)");

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
    int foo(int b, int a) { return a + b; }
    int foo(String a, String b) { return 0; }
    void test() {
        int x = foo(2, 1);
        int y = foo("a", "b");
    }
}
"#
    );
}
