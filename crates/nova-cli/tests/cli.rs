use assert_cmd::Command;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use predicates::prelude::*;

fn nova() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("nova"))
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
            .and(predicate::str::contains("bugreport")),
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
    assert!(
        status["indexes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|idx| idx["name"] == "symbols" && idx["bytes"].as_u64().unwrap_or(0) > 0),
        "expected non-empty symbols index, got: {status:#}"
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
            sym["name"] == "Main"
                && sym["locations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|loc| loc["file"] == "src/Main.java")
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
