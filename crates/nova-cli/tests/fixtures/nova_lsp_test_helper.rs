use serde_json::json;
use std::io::{BufRead, BufReader, Write};

fn main() -> std::io::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();

    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("nova-cli-test-lsp 0.0.0");
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprintln!(
            "nova-cli-test-lsp 0.0.0\n\nUsage:\n  nova-cli-test-lsp [--stdio]\n"
        );
        return Ok(());
    }

    // Accept (and ignore) `--stdio` for compatibility with `nova lsp`'s default behaviour.
    // All other args are ignored.

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    let mut saw_shutdown = false;

    while let Some(message) = read_jsonrpc_message(&mut reader)? {
        let method = message.get("method").and_then(|m| m.as_str());
        match method {
            Some("initialize") => {
                let id = message.get("id").cloned().unwrap_or(json!(null));
                write_jsonrpc_message(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "capabilities": {} }
                    }),
                )?;
            }
            Some("shutdown") => {
                saw_shutdown = true;
                let id = message.get("id").cloned().unwrap_or(json!(null));
                write_jsonrpc_message(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null
                    }),
                )?;
            }
            Some("exit") => {
                std::process::exit(if saw_shutdown { 0 } else { 1 });
            }
            _ => {
                // Ignore all other messages. This helper is intentionally minimal.
            }
        }
    }

    // EOF: follow the LSP convention of exiting non-zero if the process is terminated without a
    // proper shutdown.
    std::process::exit(if saw_shutdown { 0 } else { 1 });
}

fn write_jsonrpc_message(writer: &mut impl Write, message: &serde_json::Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_jsonrpc_message(
    reader: &mut impl BufRead,
) -> std::io::Result<Option<serde_json::Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(None);
        }

        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }

        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let len = match content_length {
        Some(len) => len,
        None => return Ok(None),
    };

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    let value = serde_json::from_slice(&buf)?;
    Ok(Some(value))
}
