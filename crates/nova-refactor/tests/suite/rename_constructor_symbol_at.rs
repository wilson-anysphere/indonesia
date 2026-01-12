use nova_refactor::{FileId, JavaSymbolKind, RefactorJavaDatabase};
use pretty_assertions::assert_eq;

#[test]
fn symbol_at_constructor_name_returns_type_symbol() {
    let file = FileId::new("Foo.java");
    let src = r#"package p; public class Foo { public Foo() {} }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let offset = src.find("public Foo()").unwrap() + "public ".len() + 1;
    let symbol = db
        .symbol_at(&file, offset)
        .expect("symbol at constructor name");

    assert_eq!(db.symbol_kind(symbol), Some(JavaSymbolKind::Type));
}

#[test]
fn symbol_at_constructor_name_resolves_to_owning_type_symbol() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer { Outer() {} class Inner { Inner() {} } }"#;
    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);

    let outer_decl_offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let outer_symbol = db
        .symbol_at(&file, outer_decl_offset)
        .expect("symbol at outer type name");
    assert_eq!(db.symbol_kind(outer_symbol), Some(JavaSymbolKind::Type));

    let outer_ctor_offset = src.find("Outer()").unwrap() + 1;
    let outer_ctor_symbol = db
        .symbol_at(&file, outer_ctor_offset)
        .expect("symbol at outer constructor name");
    assert_eq!(outer_ctor_symbol, outer_symbol);

    let inner_decl_offset = src.find("class Inner").unwrap() + "class ".len() + 1;
    let inner_symbol = db
        .symbol_at(&file, inner_decl_offset)
        .expect("symbol at inner type name");
    assert_eq!(db.symbol_kind(inner_symbol), Some(JavaSymbolKind::Type));

    let inner_ctor_offset = src.find("Inner()").unwrap() + 1;
    let inner_ctor_symbol = db
        .symbol_at(&file, inner_ctor_offset)
        .expect("symbol at inner constructor name");
    assert_eq!(inner_ctor_symbol, inner_symbol);
    assert_ne!(inner_symbol, outer_symbol);
}
