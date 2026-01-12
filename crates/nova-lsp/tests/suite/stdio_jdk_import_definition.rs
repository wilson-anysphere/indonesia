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

fn fake_jdk_with_duplicate_list(base_fake_jdk_root: &Path) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let jmods_dir = temp.path().join("jmods");
    std::fs::create_dir_all(&jmods_dir).expect("create jmods dir");

    let base_jmod_path = base_fake_jdk_root.join("jmods/java.base.jmod");
    let out_jmod_path = jmods_dir.join("java.base.jmod");

    let base_file = std::fs::File::open(&base_jmod_path).expect("open base java.base.jmod");
    let mut archive = zip::ZipArchive::new(base_file).expect("open base java.base.jmod zip");

    let list_bytes = {
        let mut entry = archive
            .by_name("classes/java/util/List.class")
            .expect("read java.util.List class bytes");
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes).expect("read class bytes");
        bytes
    };

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

    // Add a second `.List` type in a different package so suffix-based fallback resolution
    // (`*.List`) becomes ambiguous and the server is forced to honor imports.
    zip.start_file("classes/java/awt/List.class", options)
        .expect("start java.awt.List");
    zip.write_all(&list_bytes)
        .expect("write java.awt.List bytes");

    zip.finish().expect("finish patched java.base.jmod");
    temp
}

#[test]
fn stdio_definition_into_jdk_resolves_explicit_and_wildcard_imported_type_name() {
    let _lock = stdio_server_lock();

    // Point JDK discovery at the tiny fake JDK shipped in this repository.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
    assert!(
        fake_jdk_root.is_dir(),
        "expected fake JDK at {}",
        fake_jdk_root.display()
    );

    // Create a modified fake JDK that contains two `*.List` types so the suffix-search fallback
    // can't pick a unique match.
    let fake_jdk = fake_jdk_with_duplicate_list(&fake_jdk_root);

    // Compute the expected URI by reading the classfile bytes out of the fake JDK.
    let jdk = nova_jdk::JdkIndex::from_jdk_root(fake_jdk.path()).expect("index fake JDK");
    let stub = jdk
        .lookup_type("java.util.List")
        .expect("lookup java.util.List")
        .expect("java.util.List stub");
    let bytes = jdk
        .read_class_bytes(&stub.internal_name)
        .expect("read class bytes")
        .expect("java.util.List bytes");
    let expected_uri = nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let main_path = root.join("Main.java");
    let main_uri = uri_for_path(&main_path);
    let explicit_text = "import java.util.List;\nclass Main { List l; }\n";
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

    let offset = explicit_text.rfind("List").expect("List token exists");
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

    // Now update the file to use a wildcard import and re-run definition.
    let wildcard_text = "import java.util.*;\nclass Main { List l; }\n";
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

    let offset = wildcard_text.rfind("List").expect("List token exists");
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

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_definition_into_jdk_on_import_line_is_not_shadowed_by_workspace_type() {
    let _lock = support::stdio_server_lock();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let base_fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
    assert!(
        base_fake_jdk_root.is_dir(),
        "expected fake JDK at {}",
        base_fake_jdk_root.display()
    );

    // Use the patched fake JDK from the other test so suffix-search fallback cannot pick a unique
    // `*.List` without honoring the explicit import.
    let fake_jdk = fake_jdk_with_duplicate_list(&base_fake_jdk_root);

    let jdk = nova_jdk::JdkIndex::from_jdk_root(fake_jdk.path()).expect("index fake JDK");
    let stub = jdk
        .lookup_type("java.util.List")
        .expect("lookup java.util.List")
        .expect("java.util.List stub");
    let bytes = jdk
        .read_class_bytes(&stub.internal_name)
        .expect("read class bytes")
        .expect("java.util.List bytes");
    let expected_uri = nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    // Add a workspace type named `List` to ensure the core resolver does not accidentally pick it
    // when the cursor is on `List` in the import statement.
    let ws_list_path = root.join("p/List.java");
    std::fs::create_dir_all(ws_list_path.parent().expect("parent dir")).expect("create p/");
    let ws_list_text = "package p; public class List {}".to_string();
    std::fs::write(&ws_list_path, &ws_list_text).expect("write p/List.java");

    let main_path = root.join("Main.java");
    let main_uri = uri_for_path(&main_path);
    let main_text = "import java.util.List;\nclass Main { }\n";
    std::fs::write(&main_path, main_text).expect("write Main.java");

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

    let ws_list_uri = uri_for_path(&ws_list_path);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": ws_list_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": ws_list_text,
                }
            }
        }),
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
                    "text": main_text,
                }
            }
        }),
    );

    let offset = main_text.find("List").expect("List token exists");
    let position = utf16_position(main_text, offset);
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

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
