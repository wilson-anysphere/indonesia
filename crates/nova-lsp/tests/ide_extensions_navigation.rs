use std::sync::Arc;

use lsp_types::Uri;
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::ProjectId;
use nova_lsp::NovaLspIdeState;
use tempfile::TempDir;

fn lsp_position(text: &str, offset: usize) -> lsp_types::Position {
    let index = nova_core::LineIndex::new(text);
    let offset = nova_core::TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    let pos = index.position(text, offset);
    lsp_types::Position::new(pos.line, pos.character)
}

#[test]
fn ide_state_supports_implementation_declaration_and_type_definition() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let iface_path = root.join("I.java");
    let impl_path = root.join("C.java");
    let foo_path = root.join("Foo.java");
    let main_path = root.join("Main.java");

    let iface_text = "interface I {\n    void foo();\n}\n";
    let impl_text = "class C implements I {\n    public void foo() {}\n}\n";
    let foo_text = "class Foo {}\n";
    let main_text = concat!(
        "class Main {\n",
        "    void test() {\n",
        "        Foo foo = new Foo();\n",
        "        foo.toString();\n",
        "    }\n",
        "}\n",
    );

    let mut db = InMemoryFileStore::new();
    for (path, text) in [
        (&iface_path, iface_text),
        (&impl_path, impl_text),
        (&foo_path, foo_text),
        (&main_path, main_text),
    ] {
        let file = db.file_id_for_path(path);
        db.set_file_text(file, text.to_string());
    }

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let state = NovaLspIdeState::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let iface_uri: Uri = url::Url::from_file_path(&iface_path)
        .expect("file path to url")
        .as_str()
        .parse()
        .expect("parse uri");
    let impl_uri: Uri = url::Url::from_file_path(&impl_path)
        .expect("file path to url")
        .as_str()
        .parse()
        .expect("parse uri");
    let foo_uri: Uri = url::Url::from_file_path(&foo_path)
        .expect("file path to url")
        .as_str()
        .parse()
        .expect("parse uri");
    let main_uri: Uri = url::Url::from_file_path(&main_path)
        .expect("file path to url")
        .as_str()
        .parse()
        .expect("parse uri");

    let iface_foo_offset = iface_text.find("foo").expect("foo in interface");
    let iface_foo_pos = lsp_position(iface_text, iface_foo_offset);

    let impl_foo_offset = impl_text.find("foo").expect("foo in impl");
    let impl_foo_pos = lsp_position(impl_text, impl_foo_offset);

    let foo_def_offset = foo_text.find("Foo").expect("Foo in definition");
    let foo_def_pos = lsp_position(foo_text, foo_def_offset);

    let usage_offset = main_text.find("foo.toString").expect("foo usage in main");
    let usage_pos = lsp_position(main_text, usage_offset);

    // implementation: interface method -> implementing method.
    let impls = state.implementation(&iface_uri, iface_foo_pos);
    assert_eq!(impls.len(), 1);
    assert_eq!(impls[0].uri, impl_uri);
    assert_eq!(impls[0].range.start, impl_foo_pos);

    // declaration: override -> interface declaration.
    let decl = state
        .declaration(&impl_uri, impl_foo_pos)
        .expect("declaration location");
    assert_eq!(decl.uri, iface_uri);
    assert_eq!(decl.range.start, iface_foo_pos);

    // typeDefinition: variable usage -> class definition.
    let ty_def = state
        .type_definition(&main_uri, usage_pos)
        .expect("type definition location");
    assert_eq!(ty_def.uri, foo_uri);
    assert_eq!(ty_def.range.start, foo_def_pos);
}
