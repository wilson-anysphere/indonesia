//! Test-only BSP server used by integration tests.
//!
//! This binary intentionally implements only enough BSP framing/JSON-RPC to simulate a
//! misbehaving server (e.g. one that accepts a connection but never responds to
//! `build/initialize`).

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    time::Duration,
};

fn main() -> Result<()> {
    // Modes:
    // - `--hang-initialize`: accept the connection but never respond to `build/initialize`.
    // - `--hang-method <METHOD>`: accept the request but never respond to that method.
    let mut hang_initialize = false;
    let mut hang_method: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hang-initialize" => hang_initialize = true,
            "--hang-method" => {
                if let Some(method) = args.next() {
                    hang_method = Some(method);
                }
            }
            _ => {}
        }
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    while let Ok(Some(msg)) = read_message(&mut reader) {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").and_then(Value::as_i64);

        if hang_initialize && method == Some("build/initialize") && id.is_some() {
            // Stall forever until the client kills us.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }

        if let (Some(method), Some(id)) = (method, id) {
            if hang_method.as_deref() == Some(method) {
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }

            // Respond to initialize with a minimal but valid result so the client can complete the
            // handshake.
            if method == "build/initialize" {
                let result = serde_json::json!({
                    "displayName": "fake-bsp",
                    "version": "0.1.0",
                    "bspVersion": "2.1.0",
                    "capabilities": {
                        "compileProvider": { "languageIds": ["java"] },
                        "javacProvider": { "languageIds": ["java"] },
                    }
                });
                let _ = write_message(
                    &mut writer,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    }),
                );
                continue;
            }

            // Respond to shutdown requests with null.
            if method == "build/shutdown" {
                let _ = write_message(
                    &mut writer,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": Value::Null,
                    }),
                );
                continue;
            }

            // Best-effort: respond to any other request with "method not found" so clients can exit.
            let _ = write_message(
                &mut writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("method not found: {method}"),
                    }
                }),
            );
        }
    }

    Ok(())
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }
    }

    let len = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .context("failed to read framed JSON-RPC message")?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

fn write_message(writer: &mut impl Write, msg: &Value) -> Result<()> {
    let json = serde_json::to_vec(msg)?;
    write!(writer, "Content-Length: {}\r\n\r\n", json.len())?;
    writer.write_all(&json)?;
    writer.flush()?;
    Ok(())
}
