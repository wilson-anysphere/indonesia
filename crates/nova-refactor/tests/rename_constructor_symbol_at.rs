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

