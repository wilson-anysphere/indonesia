use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    exit_notification, initialize_request_empty, initialized_notification, read_jsonrpc_message,
    shutdown_request, stdio_server_lock, write_jsonrpc_message,
};

#[test]
fn stdio_server_loads_config_from_flag_and_initializes() {
    let _lock = stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(&config_path, "[logging]\nlevel = \"debug\"\n").expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's legacy AI env-var wiring can't override the config file and make
        // this test flaky.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let initialize_resp = read_jsonrpc_message(&mut stdout);
    assert_eq!(initialize_resp.get("id").and_then(|v| v.as_i64()), Some(1));
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(&mut stdin, &shutdown_request(2));
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
