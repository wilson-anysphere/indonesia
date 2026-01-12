use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::Duration;
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

#[tokio::test]
async fn tcp_server_listens_and_speaks_dap() {
    use nova_dap::dap_tokio::{DapReader, DapWriter};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    use tokio::net::TcpStream;

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_nova-dap"))
        .arg("--listen")
        .arg("127.0.0.1:0")
        .env_remove("NOVA_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nova-dap");

    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let mut stderr_lines = tokio::io::BufReader::new(stderr).lines();

    let addr: SocketAddr = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let Some(line) = stderr_lines
                .next_line()
                .await
                .expect("read stderr")
            else {
                panic!("nova-dap exited before reporting listening address");
            };

            if let Some(rest) = line.strip_prefix("listening on ") {
                return rest.parse::<SocketAddr>().expect("parse SocketAddr");
            }
        }
    })
    .await
    .expect("timeout waiting for listen address");

    let stream = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr))
        .await
        .expect("timeout connecting")
        .expect("connect");
    stream.set_nodelay(true).ok();
    let (reader, writer) = stream.into_split();
    let mut reader = DapReader::new(reader);
    let mut writer = DapWriter::new(writer);

    writer
        .write_value(&json!({
            "seq": 1,
            "type": "request",
            "command": "initialize",
            "arguments": {},
        }))
        .await
        .expect("write initialize");

    let mut got_initialize_response = None;
    let mut got_initialized_event = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, reader.read_value())
            .await
            .expect("timeout waiting for initialize response/event")
            .expect("read dap message")
            .expect("unexpected EOF");

        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(1)
        {
            got_initialize_response = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("initialized")
        {
            got_initialized_event = true;
        }

        if got_initialize_response.is_some() && got_initialized_event {
            break;
        }
    }

    let initialize_response = got_initialize_response.expect("initialize response");
    assert!(
        initialize_response
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "initialize was not successful: {initialize_response}"
    );
    assert!(got_initialized_event, "expected initialized event");

    writer
        .write_value(&json!({
            "seq": 2,
            "type": "request",
            "command": "disconnect",
            "arguments": {},
        }))
        .await
        .expect("write disconnect");

    let mut got_disconnect_response = None;
    let mut got_terminated_event = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, reader.read_value())
            .await
            .expect("timeout waiting for disconnect response/event")
            .expect("read dap message");

        let Some(msg) = msg else {
            break;
        };

        if msg.get("type").and_then(|v| v.as_str()) == Some("response")
            && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(2)
        {
            got_disconnect_response = Some(msg);
        } else if msg.get("type").and_then(|v| v.as_str()) == Some("event")
            && msg.get("event").and_then(|v| v.as_str()) == Some("terminated")
        {
            got_terminated_event = true;
        }

        if got_disconnect_response.is_some() && got_terminated_event {
            break;
        }
    }

    let disconnect_response = got_disconnect_response.expect("disconnect response");
    assert!(
        disconnect_response
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "disconnect was not successful: {disconnect_response}"
    );
    assert!(got_terminated_event, "expected terminated event after disconnect");

    drop(writer);
    drop(reader);

    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("timeout waiting for nova-dap to exit")
        .expect("wait");
    assert!(status.success());

    let mut stdout_buf = Vec::new();
    let mut stdout_reader = tokio::io::BufReader::new(stdout);
    stdout_reader
        .read_to_end(&mut stdout_buf)
        .await
        .expect("read stdout");
    assert!(
        stdout_buf.is_empty(),
        "expected nova-dap TCP mode to be silent on stdout, got: {}",
        String::from_utf8_lossy(&stdout_buf)
    );

    // Ensure only the "listening on ..." line was printed to stderr.
    let remaining: Vec<String> = tokio::time::timeout(Duration::from_secs(1), async {
        let mut lines = Vec::new();
        while let Some(line) = stderr_lines.next_line().await.expect("read stderr") {
            if !line.trim().is_empty() {
                lines.push(line);
            }
        }
        lines
    })
    .await
    .expect("timeout draining stderr");
    assert!(
        remaining.is_empty(),
        "unexpected stderr output in TCP mode: {remaining:?}"
    );
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
