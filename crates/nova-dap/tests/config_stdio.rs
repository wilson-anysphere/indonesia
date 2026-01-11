use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[test]
fn stdio_server_loads_config_from_flag_and_initializes() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(&config_path, "[logging]\nlevel = \"debug\"\n").expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-dap"))
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-dap");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_dap_message(
        &mut stdin,
        &json!({
            "seq": 1,
            "type": "request",
            "command": "initialize",
            "arguments": {}
        }),
    );
    let initialize_resp = read_dap_response(&mut stdout, 1);
    assert!(initialize_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    write_dap_message(
        &mut stdin,
        &json!({
            "seq": 2,
            "type": "request",
            "command": "disconnect",
            "arguments": {}
        }),
    );
    let disconnect_resp = read_dap_response(&mut stdout, 2);
    assert!(disconnect_resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success());
}

fn write_dap_message(writer: &mut impl Write, message: &serde_json::Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

fn read_dap_response(reader: &mut impl BufRead, request_seq: i64) -> serde_json::Value {
    loop {
        let msg = read_dap_message(reader);
        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(request_seq)
        {
            return msg;
        }
    }
}

fn read_dap_message(reader: &mut impl BufRead) -> serde_json::Value {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).expect("read header line");
        assert!(bytes_read > 0, "unexpected EOF while reading headers");

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).expect("read body");
    serde_json::from_slice(&buf).expect("parse json")
}

