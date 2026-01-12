use assert_cmd::Command;
use assert_fs::TempDir;
use predicates::prelude::*;

fn nova() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("nova"))
}

#[cfg(not(feature = "wasm-extensions"))]
#[test]
fn validate_reports_missing_wasm_support() {
    let temp = TempDir::new().unwrap();

    nova()
        .arg("extensions")
        .arg("validate")
        .arg("--root")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "nova built without wasm extension support",
        ));
}

#[cfg(not(feature = "wasm-extensions"))]
#[test]
fn list_works_without_wasm_support() {
    use assert_fs::prelude::*;

    let temp = TempDir::new().unwrap();
    temp.child("nova.toml")
        .write_str(
            r#"
[extensions]
wasm_paths = ["extensions"]
"#,
        )
        .unwrap();

    temp.child("extensions/example.good")
        .create_dir_all()
        .unwrap();
    temp.child("extensions/example.good/nova-ext.toml")
        .write_str(
            r#"
id = "example.good"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
        )
        .unwrap();
    temp.child("extensions/example.good/plugin.wasm")
        .write_binary(&[0_u8; 1])
        .unwrap();

    let output = nova()
        .arg("extensions")
        .arg("list")
        .arg("--root")
        .arg(temp.path())
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["extensions"][0]["id"].as_str().unwrap(), "example.good");
    assert!(json["errors"].as_array().unwrap().is_empty(), "{json:#}");
}

#[cfg(feature = "wasm-extensions")]
mod wasm {
    use super::*;
    use assert_fs::prelude::*;

    fn write_extension_bundle(root: &TempDir, id: &str, abi_version: i32) {
        let extensions_root = root.child("extensions");
        extensions_root.create_dir_all().unwrap();

        let ext_dir = extensions_root.child(id);
        ext_dir.create_dir_all().unwrap();

        ext_dir
            .child("nova-ext.toml")
            .write_str(&format!(
                r#"
id = "{id}"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#
            ))
            .unwrap();

        let wat = format!(
            r#"
(module
  (memory (export "memory") 1)
  (func (export "nova_ext_alloc") (param i32) (result i32)
    (i32.const 0)
  )
  (func (export "nova_ext_free") (param i32 i32)
    nop
  )
  (func (export "nova_ext_abi_version") (result i32)
    (i32.const {abi_version})
  )
  (func (export "nova_ext_capabilities") (result i32)
    (i32.const 1)
  )
  (func (export "nova_ext_diagnostics") (param i32 i32) (result i64)
    (i64.const 0)
  )
)
"#
        );
        let wasm_bytes = wat::parse_str(&wat).unwrap();
        ext_dir.child("plugin.wasm").write_binary(&wasm_bytes).unwrap();
    }

    fn write_workspace_config(root: &TempDir) {
        root.child("nova.toml")
            .write_str(
                r#"
[extensions]
wasm_paths = ["extensions"]
"#,
            )
            .unwrap();
    }

    #[test]
    fn list_and_validate_succeeds_for_valid_extension() {
        let temp = TempDir::new().unwrap();
        write_workspace_config(&temp);
        write_extension_bundle(&temp, "example.good", 1);

        let list_output = nova()
            .arg("extensions")
            .arg("list")
            .arg("--root")
            .arg(temp.path())
            .arg("--json")
            .output()
            .unwrap();

        assert!(
            list_output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&list_output.stderr)
        );

        let json: serde_json::Value = serde_json::from_slice(&list_output.stdout).unwrap();
        assert_eq!(
            json["extensions"][0]["id"].as_str().unwrap(),
            "example.good"
        );
        assert!(json["errors"].as_array().unwrap().is_empty(), "{json:#}");

        nova()
            .arg("extensions")
            .arg("validate")
            .arg("--root")
            .arg(temp.path())
            .assert()
            .success();
    }

    #[test]
    fn validate_exits_nonzero_for_invalid_wasm_abi() {
        let temp = TempDir::new().unwrap();
        write_workspace_config(&temp);
        write_extension_bundle(&temp, "example.bad", 2);

        nova()
            .arg("extensions")
            .arg("validate")
            .arg("--root")
            .arg(temp.path())
            .assert()
            .failure()
            .stderr(
                predicate::str::contains("example.bad")
                    .and(predicate::str::contains("unsupported nova-ext wasm ABI version")),
            );
    }
}
