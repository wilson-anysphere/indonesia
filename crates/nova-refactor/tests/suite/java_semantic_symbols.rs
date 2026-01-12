use nova_refactor::{FileId, JavaSymbolKind, RefactorDatabase, RefactorJavaDatabase};
use pretty_assertions::assert_eq;

#[test]
fn symbol_at_resolves_fields_methods_and_types_across_workspace() {
    let file_a = FileId::new("A.java");
    let file_b = FileId::new("B.java");

    let src_a = r#"package p;
class A {
  int foo;
  static int SFOO;

  void bar() {}
  static void sbar() {}

  void test() {
    this.foo = 1;
    this.bar();
  }
}
"#;

    let src_b = r#"package p;
 class B {
   void m() {
     A obj = new A();
     int x = obj.foo;
     obj.bar();
     int y = A.SFOO;
     A.sbar();
     Object raw = obj;
     A casted = (A) raw;
   }
 }
 "#;

    let db = RefactorJavaDatabase::new([
        (file_a.clone(), src_a.to_string()),
        (file_b.clone(), src_b.to_string()),
    ]);

    // Cross-file type usage (`new A()`).
    let new_a_offset = src_b.find("new A()").unwrap() + "new ".len();
    let a_type_from_new = db
        .symbol_at(&file_b, new_a_offset)
        .expect("symbol_at new A()");
    assert_eq!(db.symbol_kind(a_type_from_new), Some(JavaSymbolKind::Type));
    let a_def = db.symbol_definition(a_type_from_new).unwrap();
    assert_eq!(a_def.file, file_a);
    assert_eq!(a_def.name, "A");

    // Type usage in local type annotation (`A obj`).
    let decl_a_offset = src_b.find("A obj").unwrap();
    let a_type_from_decl = db
        .symbol_at(&file_b, decl_a_offset)
        .expect("symbol_at A obj");
    assert_eq!(a_type_from_decl, a_type_from_new);

    // Field reference (`this.foo`).
    let this_foo_offset = src_a.find("this.foo").unwrap() + "this.".len() + 1;
    let foo_field_from_this = db
        .symbol_at(&file_a, this_foo_offset)
        .expect("symbol_at this.foo");
    assert_eq!(
        db.symbol_kind(foo_field_from_this),
        Some(JavaSymbolKind::Field)
    );
    let foo_def = db.symbol_definition(foo_field_from_this).unwrap();
    assert_eq!(foo_def.file, file_a);
    assert_eq!(foo_def.name, "foo");

    // Field reference (`obj.foo`).
    let obj_foo_offset = src_b.find("obj.foo").unwrap() + "obj.".len() + 1;
    let foo_field_from_obj = db
        .symbol_at(&file_b, obj_foo_offset)
        .expect("symbol_at obj.foo");
    assert_eq!(foo_field_from_obj, foo_field_from_this);

    // Method call (`this.bar()`).
    let this_bar_offset = src_a.find("this.bar").unwrap() + "this.".len() + 1;
    let bar_method_from_this = db
        .symbol_at(&file_a, this_bar_offset)
        .expect("symbol_at this.bar()");
    assert_eq!(
        db.symbol_kind(bar_method_from_this),
        Some(JavaSymbolKind::Method)
    );
    let bar_def = db.symbol_definition(bar_method_from_this).unwrap();
    assert_eq!(bar_def.file, file_a);
    assert_eq!(bar_def.name, "bar");

    // Method call (`obj.bar()`).
    let obj_bar_offset = src_b.find("obj.bar").unwrap() + "obj.".len() + 1;
    let bar_method_from_obj = db
        .symbol_at(&file_b, obj_bar_offset)
        .expect("symbol_at obj.bar()");
    assert_eq!(bar_method_from_obj, bar_method_from_this);

    // Static field access (`A.SFOO`).
    let sfoo_offset = src_b.find("A.SFOO").unwrap() + "A.".len() + 1;
    let sfoo_symbol = db
        .symbol_at(&file_b, sfoo_offset)
        .expect("symbol_at A.SFOO");
    assert_eq!(db.symbol_kind(sfoo_symbol), Some(JavaSymbolKind::Field));
    let sfoo_def = db.symbol_definition(sfoo_symbol).unwrap();
    assert_eq!(sfoo_def.file, file_a);
    assert_eq!(sfoo_def.name, "SFOO");

    // Static method call (`A.sbar()`).
    let sbar_offset = src_b.find("A.sbar").unwrap() + "A.".len() + 1;
    let sbar_symbol = db
        .symbol_at(&file_b, sbar_offset)
        .expect("symbol_at A.sbar()");
    assert_eq!(db.symbol_kind(sbar_symbol), Some(JavaSymbolKind::Method));
    let sbar_def = db.symbol_definition(sbar_symbol).unwrap();
    assert_eq!(sbar_def.file, file_a);
    assert_eq!(sbar_def.name, "sbar");

    // Type usage in cast expression (`(A) raw`).
    let cast_offset = src_b.find("(A) raw").unwrap() + "(".len();
    let a_type_from_cast = db
        .symbol_at(&file_b, cast_offset)
        .expect("symbol_at (A) raw");
    assert_eq!(db.symbol_kind(a_type_from_cast), Some(JavaSymbolKind::Type));
    assert_eq!(a_type_from_cast, a_type_from_new);
}
