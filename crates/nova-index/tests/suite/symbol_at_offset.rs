use std::collections::BTreeMap;

use nova_index::{Index, SymbolKind};

#[test]
fn symbol_at_offset_prefers_most_nested_class() {
    let file = "file:///Test.java";
    let source = r#"
class Outer {
    class Inner {
        void innerMethod() {
            int x = 0;
        }
    }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.to_string(), source.to_string());

    let index = Index::new(files);

    let inner_name_offset = source.find("Inner").expect("inner class name");
    let sym = index
        .symbol_at_offset(file, inner_name_offset, Some(&[SymbolKind::Class]))
        .expect("class at offset");
    assert_eq!(sym.kind, SymbolKind::Class);
    assert_eq!(sym.name, "Inner");

    // Without kind filtering we should still prefer the most nested symbol at that position.
    let sym = index
        .symbol_at_offset(file, inner_name_offset, None)
        .expect("symbol at offset");
    assert_eq!(sym.kind, SymbolKind::Class);
    assert_eq!(sym.name, "Inner");
}

#[test]
fn symbol_at_offset_inside_method_body_returns_method() {
    let file = "file:///Test.java";
    let source = r#"
class Outer {
    class Inner {
        void innerMethod() {
            int x = 0;
        }
    }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.to_string(), source.to_string());

    let index = Index::new(files);

    let body_offset = source.find("x = 0").expect("method body");
    let sym = index
        .symbol_at_offset(file, body_offset, None)
        .expect("symbol at offset");
    assert_eq!(sym.kind, SymbolKind::Method);
    assert_eq!(sym.name, "innerMethod");

    // If we filter to classes, we should fall back to the containing class.
    let sym = index
        .symbol_at_offset(file, body_offset, Some(&[SymbolKind::Class]))
        .expect("class at offset");
    assert_eq!(sym.kind, SymbolKind::Class);
    assert_eq!(sym.name, "Inner");
}

#[test]
fn symbol_at_offset_on_field_name_returns_field() {
    let file = "file:///Test.java";
    let source = r#"
class A {
    int field = 1;

    void method() {
        field++;
    }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.to_string(), source.to_string());

    let index = Index::new(files);

    let field_name_offset = source.find("field =").expect("field decl") + "field".len() / 2;
    let sym = index
        .symbol_at_offset(file, field_name_offset, Some(&[SymbolKind::Field]))
        .expect("field at offset");
    assert_eq!(sym.kind, SymbolKind::Field);
    assert_eq!(sym.name, "field");
}
