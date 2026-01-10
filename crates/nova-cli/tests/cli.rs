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
            .and(predicate::str::contains("cache"))
            .and(predicate::str::contains("perf"))
            .and(predicate::str::contains("parse")),
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
