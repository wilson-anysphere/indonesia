mod codec;

use codec::{read_json_message, write_json_message};
use serde_json::json;
use std::io::{BufReader, BufWriter};

fn main() -> std::io::Result<()> {
    // Accept `--stdio` for compatibility with editor templates. For now we only
    // support stdio transport.
    let _ = std::env::args().skip(1).collect::<Vec<_>>();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    let mut shutdown_requested = false;

    while let Some(message) = read_json_message::<_, serde_json::Value>(&mut reader)? {
        let Some(method) = message.get("method").and_then(|m| m.as_str()) else {
            // Response (from client) or malformed message. Ignore.
            continue;
        };

        let id = message.get("id").cloned();
        if id.is_none() {
            // Notification.
            if method == "exit" {
                break;
            }
            continue;
        }

        let id = id.unwrap_or(serde_json::Value::Null);
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let response = handle_request(method, id, params, &mut shutdown_requested);
        write_json_message(&mut writer, &response)?;
    }

    Ok(())
}

fn handle_request(
    method: &str,
    id: serde_json::Value,
    params: serde_json::Value,
    shutdown_requested: &mut bool,
) -> serde_json::Value {
    match method {
        "initialize" => {
            // Minimal initialize response. We intentionally advertise no standard
            // capabilities yet; editor integrations can still call custom `nova/*`
            // requests directly.
            let result = json!({
                "capabilities": {},
                "serverInfo": {
                    "name": "nova-lsp",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            });
            json!({ "jsonrpc": "2.0", "id": id, "result": result })
        }
        "shutdown" => {
            *shutdown_requested = true;
            json!({ "jsonrpc": "2.0", "id": id, "result": serde_json::Value::Null })
        }
        _ => {
            if *shutdown_requested {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32600,
                        "message": "Server is shutting down"
                    }
                });
            }

            if method.starts_with("nova/") {
                match nova_lsp::handle_custom_request(method, params) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(err) => {
                        let (code, message) = match err {
                            nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                            nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                        };
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                }
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: {method}")
                    }
                })
            }
        }
    }
}
