use serde_json::json;
use std::io::{BufRead, BufReader, Write};

fn main() -> std::io::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();

    fn parse_config_arg(args: &[String]) -> Option<String> {
        let mut i = 0usize;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--config" {
                return args.get(i + 1).cloned();
            }
            if let Some(value) = arg.strip_prefix("--config=") {
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
            i += 1;
        }
        None
    }

    // When requested by tests, validate that the launcher forwarded the global `--config <path>`
    // argument (without emitting anything on stdout).
    if let Ok(expected) = std::env::var("NOVA_CLI_TEST_EXPECT_CONFIG") {
        let actual = parse_config_arg(&args);
        if actual.as_deref() != Some(expected.as_str()) {
            eprintln!("expected --config {expected:?}, got args: {args:?}");
            std::process::exit(3);
        }
    }

    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("nova-cli-test-lsp 0.0.0");
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprintln!("nova-cli-test-lsp 0.0.0\n\nUsage:\n  nova-cli-test-lsp [--stdio]\n");
        return Ok(());
    }

    // When invoked as `nova-lsp` (via PATH), require `--stdio` so integration tests can validate
    // that the launcher injects the default argument.
    //
    // When invoked under any other name (e.g. directly as `nova-cli-test-lsp`), accept any args.
    let invoked_as_nova_lsp = std::env::args()
        .next()
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_stem())
        .and_then(|p| p.to_str())
        .is_some_and(|s| s == "nova-lsp");

    if invoked_as_nova_lsp && !args.iter().any(|arg| arg == "--stdio") {
        eprintln!("expected --stdio when invoked as nova-lsp");
        std::process::exit(2);
    }

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

fn write_jsonrpc_message(
    writer: &mut impl Write,
    message: &serde_json::Value,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_jsonrpc_message(reader: &mut impl BufRead) -> std::io::Result<Option<serde_json::Value>> {
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
