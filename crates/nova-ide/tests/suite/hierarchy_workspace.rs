use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::{Position, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{FileId, InMemoryFileStore};
use nova_ide::{
    call_hierarchy_incoming_calls, call_hierarchy_outgoing_calls, prepare_call_hierarchy,
    prepare_type_hierarchy, type_hierarchy_subtypes, type_hierarchy_supertypes,
};
use tempfile::TempDir;

use crate::framework_harness::offset_to_position;

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
fn type_hierarchy_across_files_resolves_supertypes_and_subtypes() {
    let fixture = FileIdFixture::parse(
        r#"
//- /A.java
class $1A {}
//- /B.java
class $0B extends A {}
"#,
    );

    let file_b = fixture.marker_file(0);
    let pos_b = fixture.marker_position(0);

    let items =
        prepare_type_hierarchy(&fixture.db, file_b, pos_b).expect("expected type hierarchy items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "B");

    let supers = type_hierarchy_supertypes(&fixture.db, file_b, "B");
    assert!(
        supers
            .iter()
            .any(|item| item.name == "A" && item.uri == fixture.marker_uri(1)),
        "expected supertypes to include A; got {supers:#?}"
    );

    let subs = type_hierarchy_subtypes(&fixture.db, file_b, "A");
    assert!(
        subs.iter()
            .any(|item| item.name == "B" && item.uri == fixture.marker_uri(0)),
        "expected subtypes to include B; got {subs:#?}"
    );
}

#[test]
fn type_hierarchy_across_files_resolves_interface_extends_edges() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I0.java
interface $1I0 {}
//- /I1.java
interface $0I1 extends I0 {}
"#,
    );

    let file_i1 = fixture.marker_file(0);
    let pos_i1 = fixture.marker_position(0);

    let items = prepare_type_hierarchy(&fixture.db, file_i1, pos_i1)
        .expect("expected type hierarchy items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "I1");
    assert_eq!(items[0].detail.as_deref(), Some("extends I0"));

    let supers = type_hierarchy_supertypes(&fixture.db, file_i1, "I1");
    assert!(
        supers
            .iter()
            .any(|item| item.name == "I0" && item.uri == fixture.marker_uri(1)),
        "expected supertypes to include I0; got {supers:#?}"
    );

    let subs = type_hierarchy_subtypes(&fixture.db, file_i1, "I0");
    assert!(
        subs.iter()
            .any(|item| item.name == "I1" && item.uri == fixture.marker_uri(0)),
        "expected subtypes to include I1; got {subs:#?}"
    );
}

#[test]
fn call_hierarchy_across_files_resolves_receiver_calls() {
    let fixture = FileIdFixture::parse(
        r#"
//- /A.java
class A { void $1bar() {} }
//- /B.java
class B { void $0foo(){ A a = new A(); a.bar(); } }
"#,
    );

    let file_b = fixture.marker_file(0);
    let pos_foo = fixture.marker_position(0);
    let items = prepare_call_hierarchy(&fixture.db, file_b, pos_foo)
        .expect("expected call hierarchy preparation");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "foo");

    let outgoing = call_hierarchy_outgoing_calls(&fixture.db, file_b, "foo");
    assert!(
        outgoing
            .iter()
            .any(|call| call.to.name == "bar" && call.to.uri == fixture.marker_uri(1)),
        "expected outgoing calls to include bar in A.java; got {outgoing:#?}"
    );

    let file_a = fixture.marker_file(1);
    let incoming = call_hierarchy_incoming_calls(&fixture.db, file_a, "bar");
    assert!(
        incoming
            .iter()
            .any(|call| call.from.name == "foo" && call.from.uri == fixture.marker_uri(0)),
        "expected incoming calls to include foo in B.java; got {incoming:#?}"
    );
}

#[test]
fn prepare_call_hierarchy_on_call_site_resolves_callee_across_files() {
    let fixture = FileIdFixture::parse(
        r#"
//- /A.java
class A { void $1bar() {} }
//- /B.java
class B { void foo(){ A a = new A(); a.$0bar(); } }
"#,
    );

    let file_b = fixture.marker_file(0);
    let pos_bar = fixture.marker_position(0);

    let items = prepare_call_hierarchy(&fixture.db, file_b, pos_bar)
        .expect("expected call hierarchy preparation");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "bar");
    assert_eq!(items[0].uri, fixture.marker_uri(1));
}

#[test]
fn prepare_call_hierarchy_on_receiverless_inherited_call_site_resolves_callee_across_files() {
    let fixture = FileIdFixture::parse(
        r#"
//- /A.java
class A { void $1bar() {} }
//- /B.java
class B extends A { void foo(){ $0bar(); } }
"#,
    );

    let file_b = fixture.marker_file(0);
    let pos_bar = fixture.marker_position(0);

    let items = prepare_call_hierarchy(&fixture.db, file_b, pos_bar)
        .expect("expected call hierarchy preparation");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "bar");
    assert_eq!(items[0].uri, fixture.marker_uri(1));
}

#[test]
fn prepare_call_hierarchy_on_static_call_site_resolves_callee_across_files() {
    let fixture = FileIdFixture::parse(
        r#"
//- /A.java
class A { static void $1bar() {} }
//- /B.java
class B { void foo(){ A.$0bar(); } }
"#,
    );

    let file_b = fixture.marker_file(0);
    let pos_bar = fixture.marker_position(0);

    let items = prepare_call_hierarchy(&fixture.db, file_b, pos_bar)
        .expect("expected call hierarchy preparation");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "bar");
    assert_eq!(items[0].uri, fixture.marker_uri(1));
}

#[test]
fn call_hierarchy_outgoing_resolves_interface_default_method() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I.java
interface I { default void $1foo() {} }
//- /C.java
class C implements I { void $0test(){ C c=null; c.foo(); } }
"#,
    );

    let file_c = fixture.marker_file(0);
    let pos_test = fixture.marker_position(0);
    let items = prepare_call_hierarchy(&fixture.db, file_c, pos_test)
        .expect("expected call hierarchy preparation");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "test");

    let outgoing = call_hierarchy_outgoing_calls(&fixture.db, file_c, "test");
    assert!(
        outgoing.iter().any(|call| {
            call.to.name == "foo"
                && call.to.uri == fixture.marker_uri(1)
                && call.to.selection_range.start == fixture.marker_position(1)
        }),
        "expected outgoing calls to include foo in I.java; got {outgoing:#?}"
    );
}

#[test]
fn call_hierarchy_resolves_receiverless_interface_default_method() {
    let fixture = FileIdFixture::parse(
        r#"
//- /I.java
interface I { default void $1foo(){} }
//- /C.java
class C implements I { void $0bar(){ foo(); } }
"#,
    );

    let file_c = fixture.marker_file(0);
    let pos_bar = fixture.marker_position(0);
    let items = prepare_call_hierarchy(&fixture.db, file_c, pos_bar)
        .expect("expected call hierarchy preparation");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "bar");

    let outgoing = call_hierarchy_outgoing_calls(&fixture.db, file_c, "bar");
    assert!(
        outgoing.iter().any(|call| {
            call.to.name == "foo"
                && call.to.uri == fixture.marker_uri(1)
                && call.to.selection_range.start == fixture.marker_position(1)
        }),
        "expected outgoing calls to include foo in I.java; got {outgoing:#?}"
    );

    let file_i = fixture.marker_file(1);
    let incoming = call_hierarchy_incoming_calls(&fixture.db, file_i, "foo");
    assert!(
        incoming
            .iter()
            .any(|call| call.from.name == "bar" && call.from.uri == fixture.marker_uri(0)),
        "expected incoming calls to include bar in C.java; got {incoming:#?}"
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
