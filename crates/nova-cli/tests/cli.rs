use assert_cmd::Command;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use predicates::prelude::*;

fn nova() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("nova"))
}

#[test]
fn help_mentions_core_commands() {
    nova()
        .arg("--help")
        .assert()
        .success()
        .stdout(
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
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["summary"]["errors"].as_u64().unwrap(), 0);
    assert!(v["diagnostics"].as_array().unwrap().is_empty());
}
