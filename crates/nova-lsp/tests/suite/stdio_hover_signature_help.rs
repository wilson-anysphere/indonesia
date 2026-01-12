use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{read_response_with_id, write_jsonrpc_message};

fn lsp_position(text: &str, offset: usize) -> lsp_types::Position {
    let index = nova_core::LineIndex::new(text);
    let offset = nova_core::TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    let pos = index.position(text, offset);
    lsp_types::Position::new(pos.line, pos.character)
}

#[test]
fn stdio_server_supports_hover_and_signature_help() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
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
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let caps = initialize_resp
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .expect("initialize capabilities");
    assert_eq!(
        caps.get("hoverProvider").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        caps.get("signatureHelpProvider").is_some(),
        "expected signatureHelpProvider to be advertised"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Main.java";
    let text = concat!(
        "class Main {\n",
        "    static void add(int a, int b) {}\n",
        "    void test() {\n",
        "        Main foo = new Main();\n",
        "        foo.toString();\n",
        "        add(1, 2);\n",
        "    }\n",
        "}\n",
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": uri, "text": text } }
        }),
    );

    // Hover on `foo` in `foo.toString()`.
    let hover_offset = text.find("foo.toString").expect("hover target");
    let hover_pos = lsp_position(text, hover_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": uri },
                "position": hover_pos,
            }
        }),
    );
    let hover_resp = read_response_with_id(&mut stdout, 2);
    let hover_val = hover_resp.get("result").cloned().expect("hover result");
    assert!(!hover_val.is_null(), "expected non-null hover result");
    let hover: lsp_types::Hover = serde_json::from_value(hover_val).expect("decode Hover");
    let hover_str = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        lsp_types::HoverContents::Scalar(marked) => match marked {
            lsp_types::MarkedString::String(s) => s,
            lsp_types::MarkedString::LanguageString(ls) => ls.value,
        },
        lsp_types::HoverContents::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    lsp_types::MarkedString::String(s) => {
                        out.push_str(&s);
                        out.push('\n');
                    }
                    lsp_types::MarkedString::LanguageString(ls) => {
                        out.push_str(&ls.value);
                        out.push('\n');
                    }
                }
            }
            out
        }
    };
    assert!(
        hover_str.contains("foo"),
        "expected hover to mention identifier; got {hover_str:?}"
    );

    // Signature help inside `add(1, 2)`.
    let sig_call_offset = text.find("add(1, 2)").expect("signature help call") + "add(".len();
    let sig_pos = lsp_position(text, sig_call_offset);
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/signatureHelp",
            "params": {
                "textDocument": { "uri": uri },
                "position": sig_pos,
            }
        }),
    );
    let sig_resp = read_response_with_id(&mut stdout, 3);
    let sig_val = sig_resp
        .get("result")
        .cloned()
        .expect("signatureHelp result");
    assert!(!sig_val.is_null(), "expected non-null signatureHelp result");
    let sig: lsp_types::SignatureHelp =
        serde_json::from_value(sig_val).expect("decode SignatureHelp");
    assert!(
        !sig.signatures.is_empty(),
        "expected at least one signature"
    );

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
