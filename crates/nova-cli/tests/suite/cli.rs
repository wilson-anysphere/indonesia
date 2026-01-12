use assert_cmd::Command;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use predicates::prelude::*;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Mutex;

// Serialize passthrough tests that spawn an LSP stdio server to keep timings and resource usage
// predictable under parallel `cargo test` execution.
static STDIO_SERVER_LOCK: Mutex<()> = Mutex::new(());

fn command_output_with_retry(
    mut make_command: impl FnMut() -> ProcessCommand,
    context: &str,
) -> std::process::Output {
    let mut backoff_ms = 5_u64;
    for attempt in 0..7 {
        match make_command().output() {
            Ok(output) => return output,
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 6 => {
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms *= 2;
            }
            Err(err) => panic!("{context}: {err}"),
        }
    }
    unreachable!("retry loop should have returned or panicked");
}

fn nova() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("nova"))
}

fn lsp_test_server() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_nova-cli-test-lsp"))
}

/// Returns a temporary directory containing the requested executable name (backed by the lightweight
/// LSP test server binary), plus a PATH value that prepends that directory.
fn path_with_test_executable(name: &str) -> (TempDir, std::ffi::OsString) {
    let temp = TempDir::new().expect("tempdir");
    let stub = lsp_test_server();

    let exe_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let dest = temp.path().join(exe_name);

    // Avoid hard-linking the stub binary: on some platforms/filesystems this can
    // intermittently fail with `ETXTBSY` ("text file busy") if Cargo is still
    // finalizing the original executable while tests start running.
    std::fs::copy(&stub, &dest).expect("copy nova CLI test server");

    let mut entries =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    entries.insert(0, temp.path().to_path_buf());
    let path = std::env::join_paths(entries).expect("join PATH");
    (temp, path)
}

fn path_with_test_nova_lsp() -> (TempDir, std::ffi::OsString) {
    path_with_test_executable("nova-lsp")
}

fn path_with_test_nova_dap() -> (TempDir, std::ffi::OsString) {
    path_with_test_executable("nova-dap")
}

fn write_jsonrpc_message(writer: &mut impl Write, message: &serde_json::Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

fn read_jsonrpc_message(reader: &mut impl BufRead) -> serde_json::Value {
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

fn read_response_with_id(reader: &mut impl BufRead, id: i64) -> serde_json::Value {
    loop {
        let msg = read_jsonrpc_message(reader);
        if msg.get("method").is_some() {
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return msg;
        }
    }
}

#[test]
fn help_mentions_core_commands() {
    nova().arg("--help").assert().success().stdout(
        predicate::str::contains("index")
            .and(predicate::str::contains("diagnostics"))
            .and(predicate::str::contains("symbols"))
            .and(predicate::str::contains("deps"))
            .and(predicate::str::contains("cache"))
            .and(predicate::str::contains("perf"))
            .and(predicate::str::contains("parse"))
            .and(predicate::str::contains("extensions"))
            .and(predicate::str::contains("lsp"))
            .and(predicate::str::contains("dap"))
            .and(predicate::str::contains("bugreport")),
    );
}

#[test]
fn lsp_help_mentions_passthrough_examples() {
    nova().args(["lsp", "--help"]).assert().success().stdout(
        predicate::str::contains("nova lsp -- --help")
            .and(predicate::str::contains("--distributed"))
            .and(predicate::str::contains("--distributed-worker-command")),
    );
}

#[test]
fn symbols_help_documents_distributed_worker_default() {
    nova().args(["symbols", "--help"]).assert().success().stdout(
        predicate::str::contains("--distributed-worker-command")
            .and(predicate::str::contains("adjacent to the running `nova` executable"))
            .and(predicate::str::contains("$PATH")),
    );
}

#[test]
fn lsp_version_passthrough_matches_nova_lsp() {
    let nova_lsp = lsp_test_server();

    let direct = command_output_with_retry(
        || {
            let mut cmd = ProcessCommand::new(&nova_lsp);
            cmd.arg("--version");
            cmd
        },
        "run nova-lsp --version",
    );
    assert!(
        direct.status.success(),
        "direct stderr: {}",
        String::from_utf8_lossy(&direct.stderr)
    );

    // Verify PATH lookup: `nova lsp --version` should behave like `nova-lsp --version`.
    let (_temp, path_with_nova_lsp) = path_with_test_nova_lsp();

    let via_nova = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("lsp")
        .arg("--version")
        .env("PATH", &path_with_nova_lsp)
        .output()
        .expect("run nova lsp --version");
    assert!(
        via_nova.status.success(),
        "via nova (PATH) stderr: {}",
        String::from_utf8_lossy(&via_nova.stderr)
    );
    assert_eq!(
        direct.stdout, via_nova.stdout,
        "expected identical stdout for `nova-lsp --version` and `nova lsp --version`"
    );

    // Verify explicit `--path` override.
    let via_nova_path = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("lsp")
        .arg("--path")
        .arg(&nova_lsp)
        .arg("--version")
        .output()
        .expect("run nova lsp --path ... --version");
    assert!(
        via_nova_path.status.success(),
        "via nova (--path) stderr: {}",
        String::from_utf8_lossy(&via_nova_path.stderr)
    );
    assert_eq!(
        direct.stdout, via_nova_path.stdout,
        "expected identical stdout for `nova-lsp --version` and `nova lsp --path ... --version`"
    );
}

#[test]
fn lsp_forwards_global_config_to_child() {
    let temp = TempDir::new().expect("tempdir");
    let config = temp.child("nova.toml");
    config.write_str("").expect("write config");

    let nova_lsp = lsp_test_server();

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(config.path())
        .arg("lsp")
        .arg("--nova-lsp")
        .arg(&nova_lsp)
        .arg("--version")
        .env("NOVA_CLI_TEST_EXPECT_CONFIG", config.path())
        .output()
        .expect("run nova lsp --config ... --version");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn lsp_passthrough_config_equals_overrides_global_config() {
    let temp = TempDir::new().expect("tempdir");
    let global_config = temp.child("global.toml");
    global_config.write_str("").expect("write global config");
    let child_config = temp.child("child.toml");
    child_config.write_str("").expect("write child config");

    let nova_lsp = lsp_test_server();

    // Child config is provided using `--config=<path>` so this test ensures the
    // launcher detects both `--config <path>` and `--config=<path>` forms.
    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(global_config.path())
        .arg("lsp")
        .arg("--nova-lsp")
        .arg(&nova_lsp)
        .arg("--")
        .arg(format!("--config={}", child_config.path().display()))
        .arg("--version")
        .env("NOVA_CLI_TEST_EXPECT_CONFIG", child_config.path())
        .output()
        .expect("run nova lsp --config ... lsp -- --config=... --version");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn lsp_stdio_initialize_shutdown_exit_passthrough() {
    let _guard = STDIO_SERVER_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let (_temp, path_with_nova_lsp) = path_with_test_nova_lsp();

    let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("PATH", path_with_nova_lsp)
        .spawn()
        .expect("spawn nova lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    assert_eq!(initialize_resp.get("id").and_then(|v| v.as_i64()), Some(1));
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 2);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "expected successful LSP lifecycle, got {status:?}"
    );
}

#[test]
fn lsp_exit_without_shutdown_propagates_failure_status() {
    let _guard = STDIO_SERVER_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let (_temp, path_with_nova_lsp) = path_with_test_nova_lsp();

    let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("PATH", path_with_nova_lsp)
        .spawn()
        .expect("spawn nova lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);

    // Per LSP spec, exiting without a prior shutdown request should return non-zero.
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert_eq!(
        status.code(),
        Some(1),
        "expected exit code 1, got {status:?}"
    );
}

#[test]
fn dap_version_passthrough_matches_nova_dap() {
    let (_temp, path_with_nova_dap) = path_with_test_nova_dap();

    let stub = lsp_test_server();
    let nova_dap_path = _temp
        .path()
        .join(format!("nova-dap{}", std::env::consts::EXE_SUFFIX));

    let direct = command_output_with_retry(
        || {
            let mut cmd = ProcessCommand::new(&nova_dap_path);
            cmd.arg("--version");
            cmd
        },
        "run nova-dap --version",
    );
    assert!(
        direct.status.success(),
        "direct stderr: {}",
        String::from_utf8_lossy(&direct.stderr)
    );

    let via_nova = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("dap")
        .arg("--version")
        .env("PATH", &path_with_nova_dap)
        .output()
        .expect("run nova dap --version");
    assert!(
        via_nova.status.success(),
        "via nova (PATH) stderr: {}",
        String::from_utf8_lossy(&via_nova.stderr)
    );
    assert_eq!(
        direct.stdout, via_nova.stdout,
        "expected identical stdout for `nova-dap --version` and `nova dap --version`"
    );

    let via_nova_path = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("dap")
        .arg("--path")
        .arg(&stub)
        .arg("--version")
        .output()
        .expect("run nova dap --path ... --version");
    assert!(
        via_nova_path.status.success(),
        "via nova (--path) stderr: {}",
        String::from_utf8_lossy(&via_nova_path.stderr)
    );
    assert_eq!(
        direct.stdout, via_nova_path.stdout,
        "expected identical stdout for `nova-dap --version` and `nova dap --path ... --version`"
    );
}

#[test]
fn dap_passthrough_config_equals_overrides_global_config() {
    let temp = TempDir::new().expect("tempdir");
    let global_config = temp.child("global.toml");
    global_config.write_str("").expect("write global config");
    let child_config = temp.child("child.toml");
    child_config.write_str("").expect("write child config");

    let stub = lsp_test_server();

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(global_config.path())
        .arg("dap")
        .arg("--nova-dap")
        .arg(&stub)
        .arg("--")
        .arg(format!("--config={}", child_config.path().display()))
        .arg("--version")
        .env("NOVA_CLI_TEST_EXPECT_CONFIG", child_config.path())
        .output()
        .expect("run nova dap --config ... dap -- --config=... --version");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn diagnostics_json_runs_on_tiny_project() {
    let temp = TempDir::new().unwrap();
    let cache_root = TempDir::new().unwrap();
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/Main.java")
        .write_str(
            r#"public class Main {
  public static void main(String[] args) {
    System.out.println("hello");
  }
}
"#,
        )
        .unwrap();

    let output = nova()
        .arg("diagnostics")
        .arg(temp.path())
        .arg("--json")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["summary"]["errors"].as_u64().unwrap(), 0);
    assert!(v["diagnostics"].as_array().unwrap().is_empty());
}

#[test]
fn diagnostics_exit_nonzero_on_parse_errors() {
    let temp = TempDir::new().unwrap();
    let cache_root = TempDir::new().unwrap();
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/Bad.java")
        .write_str("class Bad { int x = ; }")
        .unwrap();

    let output = nova()
        .arg("diagnostics")
        .arg(temp.path())
        .arg("--json")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1, got {:?} (stderr: {})",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(v["summary"]["errors"].as_u64().unwrap() > 0, "{v:#}");
    assert!(
        v["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "PARSE" && d["file"] == "src/Bad.java"),
        "expected PARSE diagnostic for src/Bad.java, got: {v:#}"
    );
}

#[test]
fn diagnostics_sarif_emits_results() {
    let temp = TempDir::new().unwrap();
    let cache_root = TempDir::new().unwrap();
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/Bad.java")
        .write_str("class Bad { int x = ; }")
        .unwrap();

    let output = nova()
        .arg("diagnostics")
        .arg(temp.path())
        .arg("--format")
        .arg("sarif")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1, got {:?} (stderr: {})",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = v["runs"][0]["results"].as_array().unwrap();
    assert!(!results.is_empty(), "expected SARIF results, got: {v:#}");
}

#[test]
fn diagnostics_github_emits_annotations() {
    let temp = TempDir::new().unwrap();
    let cache_root = TempDir::new().unwrap();
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/Bad.java")
        .write_str("class Bad { int x = ; }")
        .unwrap();

    let output = nova()
        .arg("diagnostics")
        .arg(temp.path())
        .arg("--format")
        .arg("github")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1, got {:?} (stderr: {})",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("::error file="),
        "expected GitHub annotation workflow command, got: {stdout}"
    );
}

#[test]
fn index_creates_persistent_cache_and_symbols_work() {
    let temp = TempDir::new().unwrap();
    let cache_root = TempDir::new().unwrap();
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/Main.java")
        .write_str(
            r#"public class Main {
  public static void main(String[] args) {
    System.out.println("hello");
  }
}
"#,
        )
        .unwrap();

    let output = nova()
        .arg("index")
        .arg(temp.path())
        .arg("--json")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let cache_root_path = v["cache_root"].as_str().unwrap();
    let metadata = std::path::Path::new(cache_root_path).join("metadata.json");
    assert!(metadata.exists(), "missing {}", metadata.display());

    // `cache status --json` should report metadata and index artifacts.
    let status_output = nova()
        .arg("cache")
        .arg("status")
        .arg("--path")
        .arg(temp.path())
        .arg("--json")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert!(
        status_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );

    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert!(status["metadata"].is_object());
    let indexes = status["indexes"].as_array().unwrap();
    let has_legacy_symbols = indexes
        .iter()
        .any(|idx| idx["name"] == "symbols" && idx["bytes"].as_u64().unwrap_or(0) > 0);
    let has_sharded_indexes = indexes
        .iter()
        .any(|idx| idx["name"] == "shards" && idx["bytes"].as_u64().unwrap_or(0) > 0);
    assert!(
        has_legacy_symbols || has_sharded_indexes,
        "expected non-empty symbol index artifacts, got: {status:#}"
    );

    // Workspace symbol search should find the Main type in the index.
    let symbols_output = nova()
        .arg("symbols")
        .arg("Main")
        .arg("--path")
        .arg(temp.path())
        .arg("--json")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert!(
        symbols_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&symbols_output.stderr)
    );

    let symbols: serde_json::Value = serde_json::from_slice(&symbols_output.stdout).unwrap();
    let symbols = symbols.as_array().unwrap();
    assert!(
        symbols.iter().any(|sym| {
            let has_file = sym
                .get("location")
                .and_then(|loc| loc.get("file"))
                .and_then(|v| v.as_str())
                == Some("src/Main.java")
                || sym
                    .get("locations")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .any(|loc| loc.get("file").and_then(|v| v.as_str()) == Some("src/Main.java"));
            sym.get("name").and_then(|v| v.as_str()) == Some("Main") && has_file
        }),
        "expected Main symbol in results, got: {symbols:#?}"
    );
}

#[test]
fn index_from_nested_path_detects_bazel_workspace_root() {
    let workspace = TempDir::new().unwrap();
    let cache_root = TempDir::new().unwrap();

    workspace.child("WORKSPACE").write_str("# bazel").unwrap();
    workspace.child("pkg").create_dir_all().unwrap();
    workspace.child("pkg/BUILD").write_str("# build").unwrap();
    workspace.child("pkg/src").create_dir_all().unwrap();
    workspace
        .child("pkg/src/Foo.java")
        .write_str("package demo; public class Foo {}")
        .unwrap();

    let output = nova()
        .arg("index")
        .arg(workspace.child("pkg/src/Foo.java").path())
        .arg("--json")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        v["root"].as_str().unwrap(),
        workspace.path().to_str().unwrap(),
        "expected Bazel workspace root, got: {v:#}"
    );
}

#[test]
fn parse_json_reports_errors_and_exits_nonzero() {
    let temp = TempDir::new().unwrap();
    temp.child("Bad.java")
        .write_str("class Bad { int x = ; }")
        .unwrap();

    let output = nova()
        .arg("parse")
        .arg(temp.child("Bad.java").path())
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        output.status.code() == Some(1),
        "expected exit code 1, got {:?} (stderr: {})",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(v["tree"].as_str().unwrap().contains("CompilationUnit"));
    assert!(
        v["errors"].as_array().unwrap().len() > 0,
        "expected at least one parse error, got: {v:#}"
    );
}

#[test]
fn bugreport_json_creates_bundle_files() {
    let temp = TempDir::new().unwrap();
    temp.child("config.toml")
        .write_str(
            r#"
[logging]
level = "debug"

[ai]
enabled = true
api_key = "SUPER-SECRET"
"#,
        )
        .unwrap();

    let out_dir = temp.child("bugreport-out");

    let output = nova()
        .arg("--config")
        .arg(temp.child("config.toml").path())
        .arg("bugreport")
        .arg("--out")
        .arg(out_dir.path())
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let bundle_path = std::path::Path::new(v["path"].as_str().unwrap());
    assert!(bundle_path.is_dir());

    for file in [
        "meta.json",
        "config.json",
        "logs.txt",
        "performance.json",
        "crashes.json",
    ] {
        let path = bundle_path.join(file);
        assert!(path.is_file(), "missing {}", path.display());
    }

    // Config file should be included but secrets redacted.
    let config_json = std::fs::read_to_string(bundle_path.join("config.json")).unwrap();
    assert!(config_json.contains("\"level\": \"debug\""));
    assert!(!config_json.contains("SUPER-SECRET"));
    assert!(config_json.contains("<redacted>"));

    // Logs should include at least the bugreport creation line.
    let logs = std::fs::read_to_string(bundle_path.join("logs.txt")).unwrap();
    assert!(
        logs.contains("creating bug report bundle"),
        "expected logs to mention bugreport creation, got:\n{logs}"
    );
}

#[test]
fn bugreport_json_creates_archive_when_requested() {
    let temp = TempDir::new().unwrap();
    temp.child("config.toml")
        .write_str(
            r#"
[logging]
level = "debug"

[ai]
enabled = true
api_key = "SUPER-SECRET"
"#,
        )
        .unwrap();

    let out_dir = temp.child("bugreport-out");

    let output = nova()
        .arg("--config")
        .arg(temp.child("config.toml").path())
        .arg("bugreport")
        .arg("--out")
        .arg(out_dir.path())
        .arg("--archive")
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let bundle_path = std::path::Path::new(v["path"].as_str().unwrap());
    let archive_path = std::path::Path::new(v["archive"].as_str().unwrap());
    assert!(bundle_path.is_dir());
    assert!(archive_path.is_file());
    assert_eq!(archive_path, &bundle_path.with_extension("zip"));
}

#[test]
fn cache_list_json_reports_project_caches_and_skips_deps() {
    let cache_root = TempDir::new().unwrap();

    cache_root.child("deps").create_dir_all().unwrap();
    cache_root
        .child("deps/keep.txt")
        .write_str("do not touch")
        .unwrap();

    for (name, last_updated) in [("proj-a", 10_u64), ("proj-b", 20_u64)] {
        let dir = cache_root.child(name);
        dir.create_dir_all().unwrap();
        dir.child("metadata.json")
            .write_str(&format!(
                r#"{{"schema_version":{},"nova_version":"test","last_updated_millis":{last_updated}}}"#,
                nova_cache::CACHE_METADATA_SCHEMA_VERSION
            ))
            .unwrap();
        dir.child("blob.bin").write_binary(&vec![0_u8; 8]).unwrap();
    }

    let output = nova()
        .arg("cache")
        .arg("list")
        .arg("--json")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let caches = v["caches"].as_array().unwrap();
    let names: Vec<String> = caches
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["proj-a", "proj-b"]);
}

#[test]
fn cache_gc_removes_project_cache_dirs_and_preserves_deps() {
    let cache_root = TempDir::new().unwrap();

    cache_root.child("deps").create_dir_all().unwrap();
    cache_root
        .child("deps/keep.txt")
        .write_str("do not touch")
        .unwrap();

    cache_root.child("proj-a").create_dir_all().unwrap();
    cache_root
        .child("proj-a/blob.bin")
        .write_binary(&vec![0_u8; 8])
        .unwrap();

    cache_root.child("proj-b").create_dir_all().unwrap();
    cache_root
        .child("proj-b/blob.bin")
        .write_binary(&vec![0_u8; 8])
        .unwrap();

    let output = nova()
        .arg("cache")
        .arg("gc")
        .arg("--max-total-bytes")
        .arg("0")
        .arg("--keep-latest-n")
        .arg("0")
        .arg("--json")
        .env("NOVA_CACHE_DIR", cache_root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !cache_root.child("proj-a").path().exists(),
        "proj-a should be removed"
    );
    assert!(
        !cache_root.child("proj-b").path().exists(),
        "proj-b should be removed"
    );
    assert!(
        cache_root.child("deps").path().exists(),
        "deps should not be removed"
    );
}
