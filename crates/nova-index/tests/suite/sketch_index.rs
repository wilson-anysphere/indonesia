use std::collections::BTreeMap;

use nova_index::{Index, SymbolId, SymbolKind};

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
    assert_eq!(index.method_param_names(id_int).unwrap(), ["x".to_string()]);

    let sig_map = vec!["Map<String, Integer>".to_string()];
    let id_map = index
        .method_overload_by_param_types("A", "foo", &sig_map)
        .expect("foo(Map<String, Integer>) should be indexed");
    assert_eq!(
        index.method_param_types(id_map).unwrap(),
        ["Map<String, Integer>"]
    );
    assert_eq!(index.method_param_names(id_map).unwrap(), ["m".to_string()]);

    let sig_string_int = vec!["String".to_string(), "int".to_string()];
    let id_string_int = index
        .method_overload_by_param_types("A", "foo", &sig_string_int)
        .expect("foo(String, int) should be indexed");
    assert_eq!(
        index.method_param_types(id_string_int).unwrap(),
        ["String", "int"]
    );
    assert_eq!(
        index.method_param_names(id_string_int).unwrap(),
        ["s".to_string(), "y".to_string()]
    );

    // Legacy name-only API is overload-safe: it returns `None` when multiple overloads exist.
    assert_eq!(index.method_symbol_id("A", "foo"), None);
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
    assert_eq!(
        index.method_param_names(id).unwrap(),
        ["s".to_string(), "m".to_string()]
    );
}

#[test]
fn method_param_parsing_ignores_block_comments_with_parens() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"class A {
    void baz(int a /* ) */ , int b) {}
}
"#
        .to_string(),
    );
    let index = Index::new(files);

    let sig = vec!["int".to_string(), "int".to_string()];
    let id = index
        .method_overload_by_param_types("A", "baz", &sig)
        .expect("baz(int, int) should be indexed");
    assert_eq!(index.method_param_types(id).unwrap(), ["int", "int"]);
    assert_eq!(
        index.method_param_names(id).unwrap(),
        ["a".to_string(), "b".to_string()]
    );
}

#[test]
fn overload_lookup_ignores_whitespace_in_param_types() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"class A {
    void foo(Map<String, Integer> m) {}
}
"#
        .to_string(),
    );
    let index = Index::new(files);

    let sig = vec!["Map<String,Integer>".to_string()];
    let id = index
        .method_overload_by_param_types("A", "foo", &sig)
        .expect("expected foo(Map<String,Integer>) to match foo(Map<String, Integer>)");
    assert_eq!(
        index.method_param_types(id).unwrap(),
        ["Map<String, Integer>"]
    );
}

#[test]
fn method_decl_range_includes_annotations() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"class A {
    @Deprecated
    void foo() {}
}
"#
        .to_string(),
    );
    let index = Index::new(files);
    let foo = index.find_method("A", "foo").expect("method exists");
    let text = index.file_text("A.java").expect("file text");
    let decl = &text[foo.decl_range.start..foo.decl_range.end];
    assert!(
        decl.contains("@Deprecated"),
        "expected decl_range to include annotations, got: {decl:?}"
    );
    assert!(decl.contains("void foo()"));
}

#[test]
fn fields_are_indexed_including_multiple_declarators() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"class A {
    int a, b = 1;
    boolean lt = 1 < 2, ge = true;
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
    assert_eq!(fields.len(), 5, "expected five fields in class A");

    let a = fields.iter().find(|f| f.name == "a").expect("field a");
    let b = fields.iter().find(|f| f.name == "b").expect("field b");
    let lt = fields.iter().find(|f| f.name == "lt").expect("field lt");
    let ge = fields.iter().find(|f| f.name == "ge").expect("field ge");
    let name = fields
        .iter()
        .find(|f| f.name == "name")
        .expect("field name");

    assert_eq!(&text[a.name_range.start..a.name_range.end], "a");
    assert_eq!(&text[b.name_range.start..b.name_range.end], "b");
    assert_eq!(&text[lt.name_range.start..lt.name_range.end], "lt");
    assert_eq!(&text[ge.name_range.start..ge.name_range.end], "ge");
    assert_eq!(&text[name.name_range.start..name.name_range.end], "name");

    assert_eq!(a.decl_range, b.decl_range);
    assert_eq!(
        &text[a.decl_range.start..a.decl_range.end],
        "    int a, b = 1;"
    );
    assert_eq!(lt.decl_range, ge.decl_range);
    assert_eq!(
        &text[lt.decl_range.start..lt.decl_range.end],
        "    boolean lt = 1 < 2, ge = true;"
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

#[test]
fn multi_line_field_declarations_are_indexed() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        "class A {\n    int a,\n        b;\n}\n".to_string(),
    );
    let index = Index::new(files);
    let text = index.file_text("A.java").expect("file text");

    let fields: Vec<_> = index
        .symbols()
        .iter()
        .filter(|sym| sym.kind == SymbolKind::Field && sym.container.as_deref() == Some("A"))
        .collect();
    assert_eq!(fields.len(), 2, "expected two fields in class A");

    let a = fields.iter().find(|f| f.name == "a").expect("field a");
    let b = fields.iter().find(|f| f.name == "b").expect("field b");

    assert_eq!(&text[a.name_range.start..a.name_range.end], "a");
    assert_eq!(&text[b.name_range.start..b.name_range.end], "b");
    assert_eq!(a.decl_range, b.decl_range);
    assert_eq!(
        &text[a.decl_range.start..a.decl_range.end],
        "    int a,\n        b;"
    );
}

#[test]
fn fields_on_same_line_get_distinct_decl_ranges() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        "class A { int a; int b; }\n".to_string(),
    );
    let index = Index::new(files);
    let text = index.file_text("A.java").expect("file text");

    let fields: Vec<_> = index
        .symbols()
        .iter()
        .filter(|sym| sym.kind == SymbolKind::Field && sym.container.as_deref() == Some("A"))
        .collect();
    assert_eq!(fields.len(), 2);

    let a = fields.iter().find(|f| f.name == "a").expect("field a");
    let b = fields.iter().find(|f| f.name == "b").expect("field b");

    assert_eq!(&text[a.decl_range.start..a.decl_range.end], " int a;");
    assert_eq!(&text[b.decl_range.start..b.decl_range.end], " int b;");
}

#[test]
fn find_field_returns_some_for_unambiguous_field() {
    let mut files = BTreeMap::new();
    files.insert("A.java".to_string(), "class A { int x; }\n".to_string());
    let index = Index::new(files);

    let x = index.find_field("A", "x").expect("expected A.x field");
    assert_eq!(x.kind, SymbolKind::Field);
    assert_eq!(x.name, "x");
    assert_eq!(x.container.as_deref(), Some("A"));
}

#[test]
fn find_field_ignores_local_variables_with_same_name() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"class A {
    int x;
    void foo() {
        int x = 1;
    }
}
"#
        .to_string(),
    );
    let index = Index::new(files);

    let x = index.find_field("A", "x").expect("expected A.x field");
    let text = index.file_text("A.java").expect("file text");
    let decl = &text[x.decl_range.start..x.decl_range.end];
    assert!(
        decl.contains("int x;"),
        "expected decl_range to point at field declaration, got: {decl:?}"
    );
}

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

    let method = index
        .find_method("A", "foo")
        .expect("method symbol missing");
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

#[test]
fn field_initializer_expression_call_does_not_create_method_symbol() {
    let input = r#"
class A {
    int x = 1 + foo(1);
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

    let method = index
        .find_method("A", "foo")
        .expect("method symbol missing");
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
        field_decl_text.contains("int x = 1 + foo(1);"),
        "expected field decl_range to cover full field statement, got: {field_decl_text:?}"
    );

    assert!(
        decl_text.contains("int foo("),
        "expected decl_range to cover real method declaration, got: {decl_text:?}"
    );
    assert!(
        !decl_text.contains("int x = 1 + foo(1)"),
        "expected decl_range to not point at field initializer, got: {decl_text:?}"
    );
    assert!(
        decl_text.contains("return a"),
        "expected decl_range to include method body, got: {decl_text:?}"
    );
}

#[test]
fn method_annotations_with_equals_do_not_break_method_detection() {
    let input = r#"
class A {
    @Ann(value = 1)
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

    let method = index
        .find_method("A", "foo")
        .expect("method symbol missing");
    let file_text = index.file_text(&method.file).unwrap();
    let decl_text = &file_text[method.decl_range.start..method.decl_range.end];
    assert!(
        decl_text.contains("int foo("),
        "expected decl_range to cover real method declaration, got: {decl_text:?}"
    );
}

#[test]
fn override_annotation_survives_other_annotations_with_braces() {
    let input = r#"
class A {
    @Override
    @Ann(values = {1, 2})
    void foo() {}
}
"#;

    let mut files = BTreeMap::new();
    files.insert("A.java".to_string(), input.to_string());
    let index = Index::new(files);

    let method = index
        .find_method("A", "foo")
        .expect("method symbol missing");
    assert!(
        method.is_override,
        "expected method to be marked as override"
    );
}

#[test]
fn override_annotation_with_annotation_array_on_abstract_method_is_preserved() {
    let input = r#"
abstract class A {
    @Override
    @Ann(values = {1, 2})
    abstract void foo();
}
"#;

    let mut files = BTreeMap::new();
    files.insert("A.java".to_string(), input.to_string());
    let index = Index::new(files);

    let method = index
        .find_method("A", "foo")
        .expect("method symbol missing");
    assert!(
        method.is_override,
        "expected method to be marked as override"
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

    assert_ne!(
        no_args.id, int_arg.id,
        "overloads should be distinct symbols"
    );
    assert_eq!(no_args.param_types.as_deref(), Some(&[][..]));
    assert_eq!(
        int_arg.param_types.as_deref(),
        Some(&["int".to_string()][..])
    );
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

#[test]
fn field_initializer_with_uppercase_comparison_does_not_break_declarator_splitting() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        "class A { boolean lt = A < B, ge = true; }\n".to_string(),
    );
    let index = Index::new(files);

    let fields: Vec<_> = index
        .symbols()
        .iter()
        .filter(|sym| sym.kind == SymbolKind::Field && sym.container.as_deref() == Some("A"))
        .collect();
    assert_eq!(
        fields.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["lt", "ge"]
    );
}

#[test]
fn interface_declarations_are_indexed_as_classes() {
    let files = BTreeMap::from([("Test.java".to_string(), "interface I {}".to_string())]);
    let index = Index::new(files);

    assert!(
        index
            .symbols()
            .iter()
            .any(|sym| sym.kind == SymbolKind::Class && sym.name == "I"),
        "expected interface I to be indexed as a Class symbol"
    );
    assert!(index.is_interface("I"));
}

#[test]
fn implements_and_interface_extends_clauses_are_indexed() {
    let source = r#"
interface I {}
class A implements I {}
interface J extends I {}
"#;
    let files = BTreeMap::from([("Test.java".to_string(), source.to_string())]);
    let index = Index::new(files);

    assert_eq!(
        index
            .class_implements("A")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["I"]
    );
    assert_eq!(
        index
            .interface_extends("J")
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["I"]
    );
}

#[test]
fn record_and_enum_declarations_are_indexed_and_implementations_recorded() {
    let source = r#"
interface I {}
record R(int x) implements I {}
enum E implements I { A; }
"#;
    let files = BTreeMap::from([("Test.java".to_string(), source.to_string())]);
    let index = Index::new(files);

    assert!(
        index
            .symbols()
            .iter()
            .any(|sym| sym.kind == SymbolKind::Class && sym.name == "R"),
        "expected record R to be indexed"
    );
    assert!(
        index
            .symbols()
            .iter()
            .any(|sym| sym.kind == SymbolKind::Class && sym.name == "E"),
        "expected enum E to be indexed"
    );

    assert_eq!(
        index
            .class_implements("R")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["I"]
    );
    assert_eq!(
        index
            .class_implements("E")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["I"]
    );
}

#[test]
fn nested_type_ranges_are_file_relative() {
    let source = r#"
class Outer {
    interface Inner {}
    class Nested {}
}
"#;
    let files = BTreeMap::from([("Test.java".to_string(), source.to_string())]);
    let index = Index::new(files);
    let text = index.file_text("Test.java").expect("file text");

    let inner = index
        .symbols()
        .iter()
        .find(|sym| sym.kind == SymbolKind::Class && sym.name == "Inner")
        .expect("expected Inner to be indexed");
    let nested = index
        .symbols()
        .iter()
        .find(|sym| sym.kind == SymbolKind::Class && sym.name == "Nested")
        .expect("expected Nested to be indexed");

    assert_eq!(&text[inner.name_range.start..inner.name_range.end], "Inner");
    assert_eq!(
        &text[nested.name_range.start..nested.name_range.end],
        "Nested"
    );

    let expected_inner_decl_start = source
        .find("interface Inner")
        .expect("expected interface Inner substring");
    let expected_nested_decl_start = source
        .find("class Nested")
        .expect("expected class Nested substring");
    assert_eq!(inner.decl_range.start, expected_inner_decl_start);
    assert_eq!(nested.decl_range.start, expected_nested_decl_start);
}
