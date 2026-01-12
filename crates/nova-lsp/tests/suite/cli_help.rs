use std::process::Command;

#[test]
fn help_documents_distributed_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--help")
        .output()
        .expect("run `nova-lsp --help`");
    assert!(output.status.success());

    // Help currently prints to stderr (eprintln!), but accept stdout too.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{stdout}{stderr}");

    let mut usage_line: Option<String> = None;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim_end() == "Usage:" {
            usage_line = lines.next().map(|s| s.to_string());
            break;
        }
    }
    let usage_line = usage_line.unwrap_or_default();
    assert!(
        usage_line.contains("--distributed"),
        "expected Usage line to include `--distributed`, got:\n{text}"
    );
    assert!(
        usage_line.contains("--distributed-worker-command"),
        "expected Usage line to include `--distributed-worker-command`, got:\n{text}"
    );
    assert!(
        usage_line.contains("--config"),
        "expected Usage line to include `--config`, got:\n{text}"
    );
    assert!(
        usage_line.contains("--stdio"),
        "expected Usage line to include `--stdio`, got:\n{text}"
    );

    assert!(
        text.contains("--distributed"),
        "expected help output to mention `--distributed`, got:\n{text}"
    );
    assert!(
        text.contains("--distributed-worker-command"),
        "expected help output to mention `--distributed-worker-command`, got:\n{text}"
    );
    assert!(
        text.contains("nova-router") && text.contains("nova-worker"),
        "expected distributed mode description to mention nova-router + nova-worker, got:\n{text}"
    );
    assert!(
        text.contains("sibling nova-worker") && text.contains("PATH"),
        "expected default nova-worker lookup description, got:\n{text}"
    );
    assert!(
        text.contains("--config <path>"),
        "expected help output to mention `--config <path>`, got:\n{text}"
    );
    assert!(
        text.contains("If omitted, uses NOVA_CONFIG") && text.contains(".nova.toml"),
        "expected config description to mention default discovery via NOVA_CONFIG/NOVA_CONFIG_PATH and nova.toml/.nova.toml, got:\n{text}"
    );
}
