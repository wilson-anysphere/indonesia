use nova_index::{Index, SymbolKind};
use std::collections::BTreeMap;

#[test]
fn field_initializer_call_does_not_create_method_symbol() {
    let input = r#"
class A {
    int x = foo(1);
    int foo(int a) { return a; }
}
"#;

    let mut files = BTreeMap::new();
    files.insert("A.java".to_string(), input.to_string());
    let index = Index::new(files);

    let foo_methods: Vec<_> = index
        .symbols()
        .iter()
        .filter(|sym| {
            sym.kind == SymbolKind::Method
                && sym.container.as_deref() == Some("A")
                && sym.name == "foo"
        })
        .collect();
    assert_eq!(foo_methods.len(), 1);

    let method = index.find_method("A", "foo").expect("method symbol missing");
    let file_text = index.file_text(&method.file).unwrap();
    let decl_text = &file_text[method.decl_range.start..method.decl_range.end];

    let x_fields: Vec<_> = index
        .symbols()
        .iter()
        .filter(|sym| {
            sym.kind == SymbolKind::Field
                && sym.container.as_deref() == Some("A")
                && sym.name == "x"
        })
        .collect();
    assert_eq!(x_fields.len(), 1);
    let field_decl_text = &file_text[x_fields[0].decl_range.start..x_fields[0].decl_range.end];
    assert!(
        field_decl_text.contains("int x = foo(1);"),
        "expected field decl_range to cover full field statement, got: {field_decl_text:?}"
    );

    assert!(
        decl_text.contains("int foo("),
        "expected decl_range to cover real method declaration, got: {decl_text:?}"
    );
    assert!(
        !decl_text.contains("int x = foo(1)"),
        "expected decl_range to not point at field initializer, got: {decl_text:?}"
    );
    assert!(
        decl_text.contains("return a"),
        "expected decl_range to include method body, got: {decl_text:?}"
    );
}
