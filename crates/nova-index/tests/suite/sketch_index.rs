use std::collections::BTreeMap;

use nova_index::{Index, SymbolKind};

#[test]
fn overloaded_methods_indexed_with_signatures_and_lookup_works() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"class A {
    void foo(int x) {}
    void foo(String s, int y) {}
    void foo(Map<String, Integer> m) {}
}
"#
        .to_string(),
    );
    let index = Index::new(files);

    let overloads = index.method_overloads("A", "foo");
    assert_eq!(overloads.len(), 3, "expected three foo overloads");

    let one_arg = index.method_overloads_by_arity("A", "foo", 1);
    assert_eq!(one_arg.len(), 2, "expected two single-arg overloads");

    let sig_int = vec!["int".to_string()];
    let id_int = index
        .method_overload_by_param_types("A", "foo", &sig_int)
        .expect("foo(int) should be indexed");
    assert_eq!(index.method_param_types(id_int).unwrap(), ["int"]);

    let sig_map = vec!["Map<String, Integer>".to_string()];
    let id_map = index
        .method_overload_by_param_types("A", "foo", &sig_map)
        .expect("foo(Map<String, Integer>) should be indexed");
    assert_eq!(
        index.method_param_types(id_map).unwrap(),
        ["Map<String, Integer>"]
    );

    let sig_string_int = vec!["String".to_string(), "int".to_string()];
    let id_string_int = index
        .method_overload_by_param_types("A", "foo", &sig_string_int)
        .expect("foo(String, int) should be indexed");
    assert_eq!(
        index.method_param_types(id_string_int).unwrap(),
        ["String", "int"]
    );

    // Existing API should still return *some* method symbol id for the name.
    let legacy = index
        .method_symbol_id("A", "foo")
        .expect("method_symbol_id should return a method for foo");
    assert!(
        overloads.contains(&legacy),
        "legacy lookup should return one of the overload ids"
    );
}

#[test]
fn method_param_splitting_handles_generic_commas_and_string_literals() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"class A {
    void bar(@Ann(values = {"a,b", "c"}) String s, Map<String, Integer> m) {}
}
"#
        .to_string(),
    );
    let index = Index::new(files);

    let sig = vec!["String".to_string(), "Map<String, Integer>".to_string()];
    let id = index
        .method_overload_by_param_types("A", "bar", &sig)
        .expect("bar(String, Map<String, Integer>) should be indexed");
    assert_eq!(
        index.method_param_types(id).unwrap(),
        ["String", "Map<String, Integer>"]
    );
}

#[test]
fn fields_are_indexed_including_multiple_declarators() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"class A {
    int a, b = 1;
    private String name;

    void method() {
        int notAField = 0;
    }
}
"#
        .to_string(),
    );
    let index = Index::new(files);
    let text = index.file_text("A.java").expect("file text");

    let fields: Vec<_> = index
        .symbols()
        .iter()
        .filter(|sym| sym.kind == SymbolKind::Field && sym.container.as_deref() == Some("A"))
        .collect();
    assert_eq!(fields.len(), 3, "expected three fields in class A");

    let a = fields.iter().find(|f| f.name == "a").expect("field a");
    let b = fields.iter().find(|f| f.name == "b").expect("field b");
    let name = fields
        .iter()
        .find(|f| f.name == "name")
        .expect("field name");

    assert_eq!(&text[a.name_range.start..a.name_range.end], "a");
    assert_eq!(&text[b.name_range.start..b.name_range.end], "b");
    assert_eq!(&text[name.name_range.start..name.name_range.end], "name");

    assert_eq!(a.decl_range, b.decl_range);
    assert_eq!(
        &text[a.decl_range.start..a.decl_range.end],
        "    int a, b = 1;"
    );
    assert_eq!(
        &text[name.decl_range.start..name.decl_range.end],
        "    private String name;"
    );

    assert!(
        fields.iter().all(|f| f.name != "notAField"),
        "local variables should not be indexed as fields"
    );
}
