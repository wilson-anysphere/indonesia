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
    let hang_initialize = std::env::args().any(|arg| arg == "--hang-initialize");

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

        // Best-effort: respond to any request with "method not found" so clients can exit.
        if let (Some(method), Some(id)) = (method, id) {
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
