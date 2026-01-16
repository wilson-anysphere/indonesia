use lsp_types::{
    CodeActionContext, CodeActionOrCommand, CodeActionParams, ExecuteCommandParams,
    PartialResultParams, Position, Range, TextDocumentIdentifier, WorkDoneProgressParams,
    WorkspaceEdit,
};
use nova_test_utils::extract_range;
use pretty_assertions::assert_eq;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    exit_notification, file_uri, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
};

#[test]
fn stdio_server_supports_extract_method_code_action_and_execute_command() {
    let _lock = stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Main.java");

    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    fs::write(&file_path, &source).expect("write file");

    let uri = file_uri(&file_path);
    let range = Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) request code actions
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range,
                context: CodeActionContext::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions: Vec<CodeActionOrCommand> =
        serde_json::from_value(code_action_resp.get("result").cloned().unwrap_or_default())
            .expect("code actions array");
    let args = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) => action.command.as_ref(),
            CodeActionOrCommand::Command(cmd) => Some(cmd),
        })
        .filter(|cmd| cmd.command == "nova.extractMethod")
        .and_then(|cmd| cmd.arguments.as_ref())
        .and_then(|args| args.first())
        .cloned()
        .expect("extract method action args");

    // 3) execute the command
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            ExecuteCommandParams {
                command: "nova.extractMethod".to_string(),
                arguments: vec![args],
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );

    let exec_resp = read_response_with_id(&mut stdout, 3);
    let result = exec_resp.get("result").cloned().expect("workspace edit");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_edits(&source, edits);
    let expected = r#"
class C {
    void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;
    assert_eq!(actual, expected);

    // 4) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0;

    for ch in text.chars() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
        idx += ch.len_utf8();
    }

    Position {
        line,
        character: col_utf16,
    }
}

fn position_to_offset(text: &str, pos: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0;

    for ch in text.chars() {
        if line == pos.line && col_utf16 == pos.character {
            return Some(idx);
        }

        if ch == '\n' {
            if line == pos.line {
                if col_utf16 == pos.character {
                    return Some(idx);
                }
                return None;
            }
            line += 1;
            col_utf16 = 0;
            idx += 1;
            continue;
        }

        if line == pos.line {
            col_utf16 += ch.len_utf16() as u32;
            if col_utf16 > pos.character {
                return None;
            }
        }
        idx += ch.len_utf8();
    }

    if line == pos.line && col_utf16 == pos.character {
        Some(idx)
    } else {
        None
    }
}

fn apply_lsp_edits(source: &str, edits: &[lsp_types::TextEdit]) -> String {
    let mut edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let start = position_to_offset(source, e.range.start).expect("start offset");
            let end = position_to_offset(source, e.range.end).expect("end offset");
            (start, end, e.new_text.as_str())
        })
        .collect();

    // Apply from the end so offsets remain stable.
    edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    // Overlap check.
    let mut last_start = source.len();
    for (start, end, _) in &edits {
        assert!(*start <= *end);
        assert!(*end <= source.len());
        assert!(*end <= last_start, "overlapping edits");
        last_start = *start;
    }

    let mut out = source.to_string();
    for (start, end, text) in edits {
        out.replace_range(start..end, text);
    }

    out
}
