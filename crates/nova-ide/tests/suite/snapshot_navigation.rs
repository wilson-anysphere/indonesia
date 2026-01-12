use std::str::FromStr;

use lsp_types::Uri;
use nova_ide::Database;

use crate::framework_harness::offset_to_position;

#[test]
fn snapshot_type_definition_on_member_field_access_returns_field_type() {
    let mut db = Database::new();

    let bar_uri = Uri::from_str("file:///Bar.java").unwrap();
    let foo_uri = Uri::from_str("file:///Foo.java").unwrap();
    let main_uri = Uri::from_str("file:///Main.java").unwrap();

    let bar_text = "class Bar {}";
    let foo_text = "class Foo { Bar bar; }";
    let main_text = "class Main { void m() { Foo foo = new Foo(); foo.bar.toString(); } }";

    db.set_file_content(bar_uri.clone(), bar_text);
    db.set_file_content(foo_uri, foo_text);
    db.set_file_content(main_uri.clone(), main_text);

    let snap = db.snapshot();

    let offset = main_text.find("foo.bar").unwrap() + "foo.".len();
    let pos = offset_to_position(main_text, offset);
    let got = snap
        .type_definition(&main_uri, pos)
        .expect("expected type definition location");

    assert_eq!(got.uri, bar_uri);

    let bar_offset = bar_text.find("Bar").unwrap();
    assert_eq!(
        got.range.start,
        offset_to_position(bar_text, bar_offset),
        "expected to land on the Bar identifier in Bar.java"
    );
}

#[test]
fn snapshot_type_definition_on_inherited_field_access_returns_field_type() {
    let mut db = Database::new();

    let bar_uri = Uri::from_str("file:///Bar.java").unwrap();
    let base_uri = Uri::from_str("file:///Base.java").unwrap();
    let derived_uri = Uri::from_str("file:///Derived.java").unwrap();
    let main_uri = Uri::from_str("file:///Main.java").unwrap();

    let bar_text = "class Bar {}";
    let base_text = "class Base { Bar bar; }";
    let derived_text = "class Derived extends Base {}";
    let main_text = "class Main { void m() { Derived d = new Derived(); d.bar.toString(); } }";

    db.set_file_content(bar_uri.clone(), bar_text);
    db.set_file_content(base_uri, base_text);
    db.set_file_content(derived_uri, derived_text);
    db.set_file_content(main_uri.clone(), main_text);

    let snap = db.snapshot();

    let offset = main_text.find("d.bar").unwrap() + "d.".len();
    let pos = offset_to_position(main_text, offset);
    let got = snap
        .type_definition(&main_uri, pos)
        .expect("expected type definition location");

    assert_eq!(got.uri, bar_uri);

    let bar_offset = bar_text.find("Bar").unwrap();
    assert_eq!(
        got.range.start,
        offset_to_position(bar_text, bar_offset),
        "expected to land on the Bar identifier in Bar.java"
    );
}

#[test]
fn snapshot_type_definition_on_this_field_access_returns_field_type() {
    let mut db = Database::new();

    let bar_uri = Uri::from_str("file:///Bar.java").unwrap();
    let foo_uri = Uri::from_str("file:///Foo.java").unwrap();

    let bar_text = "class Bar {}";
    let foo_text = "class Foo { Bar bar; void m(){ this.bar.toString(); } }";

    db.set_file_content(bar_uri.clone(), bar_text);
    db.set_file_content(foo_uri.clone(), foo_text);

    let snap = db.snapshot();

    let offset = foo_text.find("this.bar").unwrap() + "this.".len();
    let pos = offset_to_position(foo_text, offset);
    let got = snap
        .type_definition(&foo_uri, pos)
        .expect("expected type definition location");

    assert_eq!(got.uri, bar_uri);

    let bar_offset = bar_text.find("Bar").unwrap();
    assert_eq!(got.range.start, offset_to_position(bar_text, bar_offset));
}

#[test]
fn snapshot_type_definition_on_super_field_access_returns_field_type() {
    let mut db = Database::new();

    let bar_uri = Uri::from_str("file:///Bar.java").unwrap();
    let base_uri = Uri::from_str("file:///Base.java").unwrap();
    let derived_uri = Uri::from_str("file:///Derived.java").unwrap();

    let bar_text = "class Bar {}";
    let base_text = "class Base { Bar bar; }";
    let derived_text = "class Derived extends Base { void m(){ super.bar.toString(); } }";

    db.set_file_content(bar_uri.clone(), bar_text);
    db.set_file_content(base_uri, base_text);
    db.set_file_content(derived_uri.clone(), derived_text);

    let snap = db.snapshot();

    let offset = derived_text.find("super.bar").unwrap() + "super.".len();
    let pos = offset_to_position(derived_text, offset);
    let got = snap
        .type_definition(&derived_uri, pos)
        .expect("expected type definition location");

    assert_eq!(got.uri, bar_uri);

    let bar_offset = bar_text.find("Bar").unwrap();
    assert_eq!(got.range.start, offset_to_position(bar_text, bar_offset));
}

#[test]
fn snapshot_type_definition_on_field_access_with_whitespace_returns_field_type() {
    let mut db = Database::new();

    let bar_uri = Uri::from_str("file:///Bar.java").unwrap();
    let foo_uri = Uri::from_str("file:///Foo.java").unwrap();
    let main_uri = Uri::from_str("file:///Main.java").unwrap();

    let bar_text = "class Bar {}";
    let foo_text = "class Foo { Bar bar; }";
    let main_text = "class Main { void m(){ Foo foo = new Foo(); foo .\n    bar.toString(); } }";

    db.set_file_content(bar_uri.clone(), bar_text);
    db.set_file_content(foo_uri, foo_text);
    db.set_file_content(main_uri.clone(), main_text);

    let snap = db.snapshot();

    let offset = main_text.find("bar.toString").unwrap();
    let pos = offset_to_position(main_text, offset);
    let got = snap
        .type_definition(&main_uri, pos)
        .expect("expected type definition location");

    assert_eq!(got.uri, bar_uri);

    let bar_offset = bar_text.find("Bar").unwrap();
    assert_eq!(got.range.start, offset_to_position(bar_text, bar_offset));
}

#[test]
fn snapshot_type_definition_on_chained_field_access_returns_field_type() {
    let mut db = Database::new();

    let leaf_uri = Uri::from_str("file:///Leaf.java").unwrap();
    let middle_uri = Uri::from_str("file:///Middle.java").unwrap();
    let outer_uri = Uri::from_str("file:///Outer.java").unwrap();
    let main_uri = Uri::from_str("file:///Main.java").unwrap();

    let leaf_text = "class Leaf {}";
    let middle_text = "class Middle { Leaf leaf; }";
    let outer_text = "class Outer { Middle middle; }";
    let main_text = "class Main { void m(){ Outer o = new Outer(); o.middle.leaf.toString(); } }";

    db.set_file_content(leaf_uri.clone(), leaf_text);
    db.set_file_content(middle_uri, middle_text);
    db.set_file_content(outer_uri, outer_text);
    db.set_file_content(main_uri.clone(), main_text);

    let snap = db.snapshot();

    let offset = main_text.find("o.middle.leaf").unwrap() + "o.middle.".len();
    let pos = offset_to_position(main_text, offset);
    let got = snap
        .type_definition(&main_uri, pos)
        .expect("expected type definition location");

    assert_eq!(got.uri, leaf_uri);

    let leaf_offset = leaf_text.find("Leaf").unwrap();
    assert_eq!(got.range.start, offset_to_position(leaf_text, leaf_offset));
}

#[test]
fn snapshot_type_definition_on_new_expression_chained_field_access_returns_field_type() {
    let mut db = Database::new();

    let leaf_uri = Uri::from_str("file:///Leaf.java").unwrap();
    let middle_uri = Uri::from_str("file:///Middle.java").unwrap();
    let outer_uri = Uri::from_str("file:///Outer.java").unwrap();
    let main_uri = Uri::from_str("file:///Main.java").unwrap();

    let leaf_text = "class Leaf {}";
    let middle_text = "class Middle { Leaf leaf; }";
    let outer_text = "class Outer { Middle middle; }";
    let main_text = "class Main { void m(){ new Outer().middle.leaf.toString(); } }";

    db.set_file_content(leaf_uri.clone(), leaf_text);
    db.set_file_content(middle_uri, middle_text);
    db.set_file_content(outer_uri, outer_text);
    db.set_file_content(main_uri.clone(), main_text);

    let snap = db.snapshot();

    let offset = main_text.find("new Outer().middle.leaf").unwrap() + "new Outer().middle.".len();
    let pos = offset_to_position(main_text, offset);
    let got = snap
        .type_definition(&main_uri, pos)
        .expect("expected type definition location");

    assert_eq!(got.uri, leaf_uri);

    let leaf_offset = leaf_text.find("Leaf").unwrap();
    assert_eq!(got.range.start, offset_to_position(leaf_text, leaf_offset));
}
