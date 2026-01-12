use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use serde_json::json;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    index.position(text, offset)
}

fn minimal_interface_classfile(internal_name: &str) -> Vec<u8> {
    fn push_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
    push_u16(&mut out, 0); // minor
    push_u16(&mut out, 52); // major (Java 8)

    // Constant pool:
    // 1: Utf8 <internal_name>
    // 2: Class #1
    // 3: Utf8 java/lang/Object
    // 4: Class #3
    push_u16(&mut out, 5); // constant_pool_count

    out.push(1); // Utf8
    push_u16(
        &mut out,
        u16::try_from(internal_name.len()).expect("internal name length fits"),
    );
    out.extend_from_slice(internal_name.as_bytes());

    out.push(7); // Class
    push_u16(&mut out, 1);

    let super_name = "java/lang/Object";
    out.push(1); // Utf8
    push_u16(&mut out, super_name.len() as u16);
    out.extend_from_slice(super_name.as_bytes());

    out.push(7); // Class
    push_u16(&mut out, 3);

    // Class header.
    const ACC_PUBLIC: u16 = 0x0001;
    const ACC_INTERFACE: u16 = 0x0200;
    const ACC_ABSTRACT: u16 = 0x0400;
    push_u16(&mut out, ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT);
    push_u16(&mut out, 2); // this_class
    push_u16(&mut out, 4); // super_class

    push_u16(&mut out, 0); // interfaces_count
    push_u16(&mut out, 0); // fields_count
    push_u16(&mut out, 0); // methods_count
    push_u16(&mut out, 0); // attributes_count

    out
}

fn fake_jdk_with_list_entry(base_fake_jdk_root: &Path) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let jmods_dir = temp.path().join("jmods");
    std::fs::create_dir_all(&jmods_dir).expect("create jmods dir");

    let base_jmod_path = base_fake_jdk_root.join("jmods/java.base.jmod");
    let out_jmod_path = jmods_dir.join("java.base.jmod");

    let base_file = std::fs::File::open(&base_jmod_path).expect("open base java.base.jmod");
    let mut archive = zip::ZipArchive::new(base_file).expect("open base java.base.jmod zip");

    let out_file = std::fs::File::create(&out_jmod_path).expect("create patched java.base.jmod");
    let mut zip = zip::ZipWriter::new(out_file);
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).expect("zip entry");
        let name = entry.name().to_owned();
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes).expect("read entry bytes");
        zip.start_file(name, options).expect("start file");
        zip.write_all(&bytes).expect("write entry bytes");
    }

    // Add a nested type to exercise `Outer.Inner` import resolution.
    let entry_internal = "java/util/List$Entry";
    let entry_bytes = minimal_interface_classfile(entry_internal);
    zip.start_file(format!("classes/{entry_internal}.class"), options)
        .expect("start List$Entry.class");
    zip.write_all(&entry_bytes)
        .expect("write List$Entry.class bytes");

    zip.finish().expect("finish patched java.base.jmod");
    temp
}

#[test]
fn stdio_definition_into_jdk_resolves_nested_type_imports() {
    let _lock = stdio_server_lock();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
    assert!(
        fake_jdk_root.is_dir(),
        "expected fake JDK at {}",
        fake_jdk_root.display()
    );

    let fake_jdk = fake_jdk_with_list_entry(&fake_jdk_root);

    // Compute the expected URI by reading the classfile bytes out of the fake JDK.
    let jdk = nova_jdk::JdkIndex::from_jdk_root(fake_jdk.path()).expect("index fake JDK");
    let stub = jdk
        .lookup_type("java.util.List$Entry")
        .expect("lookup java.util.List$Entry")
        .expect("java.util.List$Entry stub");
    let bytes = jdk
        .read_class_bytes(&stub.internal_name)
        .expect("read class bytes")
        .expect("java.util.List$Entry bytes");
    let expected_uri = nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let main_path = root.join("Main.java");
    let main_uri = uri_for_path(&main_path);
    let explicit_text = "import java.util.List.Entry;\nclass Main { Entry e; }\n";
    std::fs::write(&main_path, explicit_text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("JAVA_HOME", fake_jdk.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": main_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": explicit_text,
                }
            }
        }),
    );

    let offset = explicit_text.rfind("Entry").expect("Entry token exists");
    let position = utf16_position(explicit_text, offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": main_uri.as_str() },
                "position": { "line": position.line, "character": position.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let location = resp.get("result").expect("definition result");
    let Some(uri) = location.get("uri").and_then(|v| v.as_str()) else {
        panic!("expected definition uri, got: {resp:?}");
    };
    assert_eq!(uri, expected_uri);

    // Update the file to use an on-demand import of nested types (`import Outer.*;`) and
    // re-run definition.
    let wildcard_text = "import java.util.List.*;\nclass Main { Entry e; }\n";
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": main_uri.as_str(), "version": 2 },
                "contentChanges": [{ "text": wildcard_text }]
            }
        }),
    );

    let offset = wildcard_text.rfind("Entry").expect("Entry token exists");
    let position = utf16_position(wildcard_text, offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": main_uri.as_str() },
                "position": { "line": position.line, "character": position.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let location = resp.get("result").expect("definition result");
    let Some(uri) = location.get("uri").and_then(|v| v.as_str()) else {
        panic!("expected definition uri, got: {resp:?}");
    };
    assert_eq!(uri, expected_uri);

    // Nested type via imported outer: `import java.util.List; List.Entry e;`.
    let outer_text = "import java.util.List;\nclass Main { List.Entry e; }\n";
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": main_uri.as_str(), "version": 3 },
                "contentChanges": [{ "text": outer_text }]
            }
        }),
    );

    let offset = outer_text.rfind("Entry").expect("Entry token exists");
    let position = utf16_position(outer_text, offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": main_uri.as_str() },
                "position": { "line": position.line, "character": position.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let location = resp.get("result").expect("definition result");
    let Some(uri) = location.get("uri").and_then(|v| v.as_str()) else {
        panic!("expected definition uri, got: {resp:?}");
    };
    assert_eq!(uri, expected_uri);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
