use lsp_types::{Location, Position, Range, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use serde_json::json;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    index.position(text, offset)
}

fn read_jmod_class_bytes(jmod_path: &Path, entry: &str) -> Vec<u8> {
    let file = std::fs::File::open(jmod_path).expect("open jmod");
    let mut zip = zip::ZipArchive::new(file).expect("open zip");
    let mut member = zip.by_name(entry).expect("zip member");
    let mut buf = Vec::new();
    member.read_to_end(&mut buf).expect("read zip member");
    buf
}

fn lsp_range(range: nova_core::Range) -> Range {
    Range::new(
        Position::new(range.start.line, range.start.character),
        Position::new(range.end.line, range.end.character),
    )
}

#[test]
fn stdio_definition_into_jdk_member_methods_and_fields_returns_symbol_ranges() {
    let _lock = stdio_server_lock();

    // Point JDK discovery at the tiny fake JDK shipped in this repository.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
    assert!(
        fake_jdk_root.is_dir(),
        "expected fake JDK at {}",
        fake_jdk_root.display()
    );

    let java_base_jmod = fake_jdk_root.join("jmods/java.base.jmod");

    // Pre-compute the expected decompiled URI + ranges by decompiling the classfiles directly.
    let list_bytes = read_jmod_class_bytes(&java_base_jmod, "classes/java/util/List.class");
    let expected_list_uri =
        nova_decompile::decompiled_uri_for_classfile(&list_bytes, "java/util/List");
    let list_decompiled =
        nova_decompile::decompile_classfile(&list_bytes).expect("decompile List.class");
    let expected_get_range = list_decompiled
        .range_for(&nova_decompile::SymbolKey::Method {
            name: "get".to_string(),
            descriptor: "(I)Ljava/lang/Object;".to_string(),
        })
        .map(lsp_range)
        .expect("range for List.get");

    let custom_bytes = read_jmod_class_bytes(&java_base_jmod, "classes/java/lang/Custom.class");
    let expected_custom_uri =
        nova_decompile::decompiled_uri_for_classfile(&custom_bytes, "java/lang/Custom");
    let custom_decompiled =
        nova_decompile::decompile_classfile(&custom_bytes).expect("decompile Custom.class");
    let expected_foo_range = custom_decompiled
        .range_for(&nova_decompile::SymbolKey::Field {
            name: "FOO".to_string(),
            descriptor: "I".to_string(),
        })
        .map(lsp_range)
        .expect("range for Custom.FOO");

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let method_path = root.join("Main.java");
    let method_uri = uri_for_path(&method_path);
    let method_text = r#"class Main {
  void m() {
    java.util.List l = null;
    l.get(0);
  }
}
"#;
    std::fs::write(&method_path, method_text).expect("write Main.java");

    let field_path = root.join("MainField.java");
    let field_uri = uri_for_path(&field_path);
    let field_text = r#"class MainField {
  int x = java.lang.Custom.FOO;
}
"#;
    std::fs::write(&field_path, field_text).expect("write MainField.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("JAVA_HOME", &fake_jdk_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
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

    // 2) Method navigation: `l.get(0)` -> decompiled `List.get`.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": method_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": method_text,
                }
            }
        }),
    );

    let get_offset = method_text.find("get(0)").expect("get call");
    let get_position = utf16_position(method_text, get_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": method_uri.as_str() },
                "position": { "line": get_position.line, "character": get_position.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let location: Location = serde_json::from_value(resp["result"].clone()).expect("location");

    assert_eq!(location.uri.as_str(), expected_list_uri);
    assert_eq!(location.range, expected_get_range);

    // 3) Field navigation: `java.lang.Custom.FOO` -> decompiled `Custom.FOO`.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": field_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": field_text,
                }
            }
        }),
    );

    let foo_offset = field_text.find("FOO").expect("FOO access");
    let foo_position = utf16_position(field_text, foo_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": field_uri.as_str() },
                "position": { "line": foo_position.line, "character": foo_position.character }
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let location: Location = serde_json::from_value(resp["result"].clone()).expect("location");

    assert_eq!(location.uri.as_str(), expected_custom_uri);
    assert_eq!(location.range, expected_foo_range);

    // 4) shutdown
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
