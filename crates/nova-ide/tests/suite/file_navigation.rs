use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::{Position, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{FileId, InMemoryFileStore};
use nova_ide::{declaration, implementation, type_definition};
use tempfile::TempDir;

use crate::text_fixture::offset_to_position;

struct FileIdFixture {
    _temp_dir: TempDir,
    db: InMemoryFileStore,
    files: HashMap<FileId, String>,
    paths: HashMap<FileId, PathBuf>,
    markers: HashMap<u32, (FileId, usize)>,
}

impl FileIdFixture {
    fn parse(fixture: &str) -> Self {
        let temp_dir = TempDir::new().expect("tempdir");
        let root = temp_dir.path();

        let mut current_path: Option<PathBuf> = None;
        let mut current_text = String::new();
        let mut files: Vec<(PathBuf, String)> = Vec::new();

        for line in fixture.lines() {
            if let Some(rest) = line.strip_prefix("//-") {
                if let Some(path) = current_path.take() {
                    files.push((path, current_text));
                    current_text = String::new();
                }

                let rel = rest.trim().trim_start_matches('/');
                current_path = Some(root.join(rel));
                continue;
            }

            if !current_text.is_empty() {
                current_text.push('\n');
            }
            current_text.push_str(line);
        }

        if let Some(path) = current_path.take() {
            files.push((path, current_text));
        }

        let mut db = InMemoryFileStore::new();
        let mut file_texts: HashMap<FileId, String> = HashMap::new();
        let mut file_paths: HashMap<FileId, PathBuf> = HashMap::new();
        let mut markers: HashMap<u32, (FileId, usize)> = HashMap::new();

        for (path, text) in files {
            let (text, file_markers) = strip_markers(&text);
            let file_id = db.file_id_for_path(&path);
            db.set_file_text(file_id, text.clone());

            file_texts.insert(file_id, text);
            file_paths.insert(file_id, path);
            for (id, offset) in file_markers {
                markers.insert(id, (file_id, offset));
            }
        }

        Self {
            _temp_dir: temp_dir,
            db,
            files: file_texts,
            paths: file_paths,
            markers,
        }
    }

    fn marker_file(&self, id: u32) -> FileId {
        self.markers.get(&id).unwrap().0
    }

    fn marker_position(&self, id: u32) -> Position {
        let (file_id, offset) = self.markers.get(&id).unwrap();
        let text = self.files.get(file_id).unwrap();
        offset_to_position(text, *offset)
    }

    fn marker_uri(&self, id: u32) -> Uri {
        let (file_id, _) = self.markers.get(&id).unwrap();
        let path = self.paths.get(file_id).unwrap();
        uri_for_path(path)
    }
}

#[test]
fn go_to_implementation_on_interface_method_returns_implementing_method() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I.java
interface I {
    void $0foo();
}
//- /C.java
class C implements I {
    public void $1foo() {}
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_interface_method_works_through_extended_interfaces() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I0.java
interface I0 {
    void $0foo();
}
//- /I1.java
interface I1 extends I0 {}
//- /C.java
class C implements I1 {
    public void $1foo() {}
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_concrete_method_returns_overriding_method() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Base.java
class Base {
    void $0foo() {}
}
//- /Sub.java
class Sub extends Base {
    void $1foo() {}
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_interface_default_method_returns_overriding_method() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I.java
interface I {
    default void $0bar() {}
}
//- /C.java
class C implements I {
    public void $1bar() {}
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_receiverless_call_returns_method_definition() {
    let fixture = FileIdFixture::parse(
        r#"
//- /C.java
class C {
  void $1foo() {}
  void test(){ $0foo(); }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_static_receiver_call_resolves_type_name() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class Foo {
    static void $1bar() {}
}
//- /Main.java
class Main {
    void m() {
        Foo.$0bar();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_receiver_call_resolves_interface_default_method() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I.java
interface I { default void $1foo() {} }
//- /C.java
class C implements I { void test(){ C c=null; c.$0foo(); } }
"#,
    );
    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_does_not_trigger_on_constructor_call() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class Foo {}
//- /C.java
class C {
  void $1Foo() {}
  void test(){ new $0Foo(); }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert!(got.is_empty());
}

#[test]
fn go_to_implementation_on_generic_member_call_uses_receiver_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /D.java
class D {
  <T> void $1bar() {}
}
//- /C.java
class C {
  D d = new D();
  void bar() {}
  void test(){ d.<String>$0bar(); }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_param_receiver_resolves_receiver_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class Foo { void $1bar() {} }
//- /Main.java
class Main { void test(Foo foo){ foo.$0bar(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_final_param_receiver_resolves_receiver_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class Foo { void $1bar() {} }
//- /Main.java
class Main { void test(final Foo foo){ foo.$0bar(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_generic_param_receiver_ignores_type_args() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Map.java
class Map { void $1put() {} }
//- /Foo.java
class Foo {}
//- /Main.java
class Main { void test(Map<String, Foo> map, Foo foo){ map.$0put(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_dollar_receiver_call_resolves_receiver_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class Foo { void $1bar() {} }
//- /Main.java
class Main {
    void test() {
        Foo foo$bar = new Foo();
        foo$bar.$0bar();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_static_receiver_call_returns_static_method_definition() {
    let fixture = FileIdFixture::parse(
        r#"
//- /A.java
class A { static void $1foo() {} }
//- /Main.java
class Main { void test(){ A.$0foo(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_does_not_trigger_on_if_keyword() {
    let fixture = FileIdFixture::parse(
        r#"
//- /C.java
class C {
  void test(boolean cond) {
    $0if (cond) {}
  }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert!(got.is_empty());
}

#[test]
fn go_to_declaration_on_param_usage_returns_param_in_signature() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Main.java
class Main { void test(Foo $1foo){ $0foo.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_second_local_in_comma_separated_decl() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Main.java
class Main { void test(){ Foo a, $1b; $0b.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_second_local_receiver_in_comma_separated_decl() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class Foo { void $1bar(){} }
//- /Main.java
class Main { void test(){ Foo a, b = new Foo(); b.$0bar(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_second_field_in_comma_separated_decl() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Main.java
class Main { Foo a, $1b; void test(){ $0b.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_dollar_local_returns_declaration() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Main.java
class Main {
    void test() {
        Foo $1foo$bar = new Foo();
        foo$bar$0.toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_variable_returns_class() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test() {
        Foo foo = new Foo();
        $0foo.toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_variable_returns_enum() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Color.java
enum $1Color { RED }
//- /Main.java
class Main {
    void test() {
        Color c = Color.RED;
        $0c.toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_variable_returns_record() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Point.java
record $1Point(int x, int y) {}
//- /Main.java
class Main {
    void test() {
        Point p = new Point(1, 2);
        $0p.toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_dollar_identifier_returns_class() {
    // NOTE: `$<digits>` sequences are reserved for fixture markers; use `$$0` to place a marker
    // after a literal `$` in the source.
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test() {
        Foo foo$bar = new Foo();
        foo$$0bar.toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_for_each_variable_returns_class() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test(java.util.List<Foo> xs){
        for (Foo x : xs) { $0x.toString(); }
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_param_usage_returns_param_type_definition() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main { void test(Foo foo){ $0foo.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_catch_variable_returns_variable() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Main.java
class Main {
    void test(){
        try {} catch (Exception $1e) { $0e.toString(); }
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_and_type_definition_on_instanceof_pattern_variable() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test(Object o){
        if (o instanceof Foo $2f) { $0f.toString(); }
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);

    let decl = declaration(&fixture.db, file, pos).expect("expected declaration location");
    assert_eq!(decl.uri, fixture.marker_uri(2));
    assert_eq!(decl.range.start, fixture.marker_position(2));

    let ty = type_definition(&fixture.db, file, pos).expect("expected type definition location");
    assert_eq!(ty.uri, fixture.marker_uri(1));
    assert_eq!(ty.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_c_style_array_local_returns_class() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test() {
        Foo foo[] = new Foo[0];
        $0foo.toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_and_type_definition_on_instanceof_pattern_variable_in_and_condition() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test(Object o){
        if (o instanceof Foo $2f && $0f.toString() != null) {}
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);

    let decl = declaration(&fixture.db, file, pos).expect("expected declaration location");
    assert_eq!(decl.uri, fixture.marker_uri(2));
    assert_eq!(decl.range.start, fixture.marker_position(2));

    let ty = type_definition(&fixture.db, file, pos).expect("expected type definition location");
    assert_eq!(ty.uri, fixture.marker_uri(1));
    assert_eq!(ty.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_and_type_definition_on_instanceof_pattern_variable_in_ternary() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test(Object o){
        String s = (o instanceof Foo $2f) ? $0f.toString() : "";
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);

    let decl = declaration(&fixture.db, file, pos).expect("expected declaration location");
    assert_eq!(decl.uri, fixture.marker_uri(2));
    assert_eq!(decl.range.start, fixture.marker_position(2));

    let ty = type_definition(&fixture.db, file, pos).expect("expected type definition location");
    assert_eq!(ty.uri, fixture.marker_uri(1));
    assert_eq!(ty.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_and_type_definition_on_switch_pattern_variable() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test(Object o){
        switch (o) {
            case Foo $2f -> { $0f.toString(); }
            default -> {}
        }
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);

    let decl = declaration(&fixture.db, file, pos).expect("expected declaration location");
    assert_eq!(decl.uri, fixture.marker_uri(2));
    assert_eq!(decl.range.start, fixture.marker_position(2));

    let ty = type_definition(&fixture.db, file, pos).expect("expected type definition location");
    assert_eq!(ty.uri, fixture.marker_uri(1));
    assert_eq!(ty.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_and_type_definition_on_switch_pattern_variable_in_when_guard() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test(Object o){
        switch (o) {
            case Foo $2f when $0f.toString() != null -> {}
            default -> {}
        }
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);

    let decl = declaration(&fixture.db, file, pos).expect("expected declaration location");
    assert_eq!(decl.uri, fixture.marker_uri(2));
    assert_eq!(decl.range.start, fixture.marker_position(2));

    let ty = type_definition(&fixture.db, file, pos).expect("expected type definition location");
    assert_eq!(ty.uri, fixture.marker_uri(1));
    assert_eq!(ty.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_annotated_array_param_usage_returns_param_type_definition() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Ann.java
@interface Ann {}
//- /Main.java
class Main { void test(Foo @Ann [] foo){ $0foo.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_array_param_usage_returns_element_type_definition() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main { void test(Foo[] foos){ $0foos.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_direct_field_access_returns_field_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /A.java
class A { Foo bar; }
//- /Main.java
class Main { void test(){ A a$var = new A(); a$var.$0bar.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_static_field_access_returns_field_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /A.java
class A { static Foo bar; }
//- /Main.java
class Main { void test(){ A.$0bar.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_field_access_with_dollar_in_receiver_returns_field_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /A.java
class A { Foo bar; }
//- /Main.java
class Main { void test(){ A a$recv = new A(); a$recv.$0bar.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_field_access_with_whitespace_returns_field_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /A.java
class A { Foo bar; }
//- /Main.java
class Main {
    void test() {
        A a = new A();
        a .
            $0bar
            .toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_inherited_field_access_returns_field_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Base.java
class Base { Foo baz; }
//- /Derived.java
class Derived extends Base {}
//- /Main.java
class Main { void test(){ Derived d = new Derived(); d.$0baz.toString(); } }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_supports_dollar_in_identifier() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo$Bar.java
class $1Foo$Bar {}
//- /Main.java
class Main { Foo$Bar x = new Foo$Bar(); $0Foo$Bar y = x; }
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_this_field_access_returns_field_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Bar.java
class $1Bar {}
//- /Foo.java
class Foo {
    Bar bar;
    void test() { this.$0bar.toString(); }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_super_field_access_returns_field_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Bar.java
class $1Bar {}
//- /Base.java
class Base { Bar bar; }
//- /Derived.java
class Derived extends Base {
    void test() { super.$0bar.toString(); }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_override_returns_interface_declaration() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I.java
interface I {
    void $1foo();
}
//- /C.java
class C implements I {
    public void $0foo() {}
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_override_searches_superinterfaces_transitively() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I0.java
interface I0 {
    void $1foo();
}
//- /I1.java
interface I1 extends I0 {}
//- /C.java
class C implements I1 {
    public void $0foo() {}
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_override_prefers_closer_interface_declaration() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I0.java
interface I0 {
    void foo();
}
//- /I1.java
interface I1 extends I0 {
    void $1foo();
}
//- /C.java
class C implements I1 {
    public void $0foo() {}
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_field_receiver_resolves_receiver_type() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class Foo {
    void $1bar() {}
}
//- /Main.java
class Main {
    private Foo foo = new Foo();
    void test(){ foo.$0bar(); }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_parameter_returns_class() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test(Foo $0foo) {
        foo.toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = type_definition(&fixture.db, file, pos).expect("expected type definition location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_parameter_returns_parameter() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test(Foo $0foo) {
        foo.toString();
    }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(0));
    assert_eq!(got.range.start, fixture.marker_position(0));
}

#[test]
fn go_to_declaration_on_field_usage_returns_field_declaration() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
class Foo {}
//- /Main.java
class Main {
    private Foo $1foo = new Foo();
    void test(){ $0foo.toString(); }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = declaration(&fixture.db, file, pos).expect("expected declaration location");

    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn navigation_cache_is_invalidated_when_a_java_file_changes() {
    let mut fixture = FileIdFixture::parse(
        r#"
//- /I.java
interface I {
    void $0foo();
}
//- /C.java
class C implements I {
    public void $1foo() {}
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);

    let got_first = implementation(&fixture.db, file, pos);
    let got_second = implementation(&fixture.db, file, pos);
    assert_eq!(got_first, got_second);
    assert_eq!(got_first.len(), 1);
    assert_eq!(got_first[0].uri, fixture.marker_uri(1));
    assert_eq!(got_first[0].range.start, fixture.marker_position(1));

    // Change the implementation location (and length) so the cached workspace fingerprint must
    // differ, forcing a rebuild.
    let c_file = fixture.marker_file(1);
    let c_path = fixture.paths.get(&c_file).unwrap().clone();
    let (new_text, markers) = strip_markers(
        r#"
class C implements I {
    // added comment to change file length
    public void $2foo() {}
}
"#,
    );
    let new_offset = markers
        .iter()
        .find(|(id, _)| *id == 2)
        .map(|(_, offset)| *offset)
        .expect("expected updated marker");

    fixture.db.set_file_text(c_file, new_text.clone());

    let got_third = implementation(&fixture.db, file, pos);
    assert_eq!(got_third.len(), 1);
    assert_eq!(got_third[0].uri, uri_for_path(&c_path));
    assert_eq!(
        got_third[0].range.start,
        offset_to_position(&new_text, new_offset)
    );
}

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::new(path.to_path_buf()).expect("fixture paths should be absolute");
    let uri = path_to_file_uri(&abs).expect("path should convert to a file URI");
    Uri::from_str(&uri).expect("URI should parse")
}

fn strip_markers(text: &str) -> (String, Vec<(u32, usize)>) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut markers = Vec::new();

    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                j += 1;
            }

            if j > i + 1 {
                let id: u32 = text[i + 1..j].parse().unwrap();
                markers.push((id, out.len()));
                i = j;
                continue;
            }
        }

        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    (out, markers)
}
