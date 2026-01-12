use nova_index::{Index, SymbolId, SymbolKind};
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
            sym.kind == SymbolKind::Method && sym.container.as_deref() == Some("A") && sym.name == "foo"
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
            sym.kind == SymbolKind::Field && sym.container.as_deref() == Some("A") && sym.name == "x"
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

#[test]
fn find_method_returns_none_for_overloaded_methods() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    void foo() {}
    void foo(int x) {}
}
"#
        .to_string(),
    );

    let index = Index::new(files);
    assert!(
        index.find_method("A", "foo").is_none(),
        "find_method should return None when overloads exist"
    );
}

#[test]
fn find_method_by_signature_resolves_correct_overload() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    void foo() {}
    void foo(int x) {}
}
"#
        .to_string(),
    );

    let index = Index::new(files);

    let no_args = index
        .find_method_by_signature("A", "foo", &[])
        .expect("no-arg overload exists");
    let int_arg = index
        .find_method_by_signature("A", "foo", &["int"])
        .expect("int overload exists");

    assert_ne!(no_args.id, int_arg.id, "overloads should be distinct symbols");
    assert_eq!(no_args.param_types.as_deref(), Some(&[][..]));
    assert_eq!(int_arg.param_types.as_deref(), Some(&["int".to_string()][..]));
}

#[test]
fn find_symbol_returns_some_for_known_ids() {
    let mut files = BTreeMap::new();
    files.insert(
        "Foo.java".to_string(),
        r#"
class Foo {
    void bar() {}
    int baz(int x) { return x; }
}
"#
        .to_string(),
    );

    let index = Index::new(files);

    let bar_id = index
        .method_symbol_id("Foo", "bar")
        .expect("expected Foo.bar to be indexed");
    let bar = index
        .find_symbol(bar_id)
        .expect("expected find_symbol to return Foo.bar");
    assert_eq!(bar.kind, SymbolKind::Method);
    assert_eq!(bar.name, "bar");
    assert_eq!(bar.container.as_deref(), Some("Foo"));

    let foo_class_id = index
        .symbols()
        .iter()
        .find(|sym| sym.kind == SymbolKind::Class && sym.name == "Foo")
        .expect("expected Foo class symbol")
        .id;
    let foo_class = index
        .find_symbol(foo_class_id)
        .expect("expected find_symbol to return Foo class");
    assert_eq!(foo_class.kind, SymbolKind::Class);
    assert_eq!(foo_class.name, "Foo");
    assert_eq!(foo_class.container, None);
}

#[test]
fn find_symbol_returns_none_for_unknown_ids() {
    let mut files = BTreeMap::new();
    files.insert("Foo.java".to_string(), "class Foo {}".to_string());
    let index = Index::new(files);

    assert_eq!(index.find_symbol(SymbolId(999_999)), None);
}

#[test]
fn find_symbol_is_consistent_for_all_indexed_symbols() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    void a() {}
}
"#
        .to_string(),
    );
    files.insert(
        "B.java".to_string(),
        r#"
class B {
    void b() {}
}
"#
        .to_string(),
    );

    let index = Index::new(files);

    for sym in index.symbols() {
        let found = index
            .find_symbol(sym.id)
            .unwrap_or_else(|| panic!("missing symbol id: {:?}", sym.id));
        assert!(std::ptr::eq(sym, found));
    }
}

