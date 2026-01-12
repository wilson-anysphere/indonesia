#![cfg(feature = "wasm-extensions")]

use nova_config::NovaConfig;
use nova_core::{FileId, ProjectId};
use nova_ext::wasm::WasmHostDb;
use nova_ext::{
    DiagnosticParams, ExtensionContext, ExtensionManager, ExtensionRegistry, LoadError,
    ProviderLastError,
};
use nova_scheduler::CancellationToken;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TODO_WAT: &str = include_str!("../examples/abi_v1_todo_diagnostics.wat");

const BUSY_LOOP_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))

  (func $nova_ext_alloc (export "nova_ext_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr)
  )
  (func $nova_ext_free (export "nova_ext_free") (param i32 i32) nop)
  (func (export "nova_ext_abi_version") (result i32) (i32.const 1))
  (func (export "nova_ext_capabilities") (result i32) (i32.const 1))

  (func (export "nova_ext_diagnostics") (param i32 i32) (result i64)
    (loop $loop
      br $loop
    )
    (i64.const 0)
  )
)
"#;

const MEMORY_GROW_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))

  (func $nova_ext_alloc (export "nova_ext_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr)
  )
  (func $nova_ext_free (export "nova_ext_free") (param i32 i32) nop)
  (func (export "nova_ext_abi_version") (result i32) (i32.const 1))
  (func (export "nova_ext_capabilities") (result i32) (i32.const 1))

  (data (i32.const 0) "[{\"message\":\"grow_failed\"}]")
  (data (i32.const 64) "[{\"message\":\"grow_ok\"}]")

  (func (export "nova_ext_diagnostics") (param i32 i32) (result i64)
    (local $grow_res i32)
    (local $src i32)
    (local $out_ptr i32)
    (local $out_len i32)

    (local.set $grow_res (memory.grow (i32.const 1)))
    (if (i32.eq (local.get $grow_res) (i32.const -1))
      (then
        (local.set $src (i32.const 0))
        (local.set $out_len (i32.const 27))
      )
      (else
        (local.set $src (i32.const 64))
        (local.set $out_len (i32.const 23))
      )
    )

    (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))
    (memory.copy (local.get $out_ptr) (local.get $src) (local.get $out_len))

    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out_len)) (i64.const 32))
      (i64.extend_i32_u (local.get $out_ptr))
    )
  )
)
"#;

#[derive(Debug)]
struct TestDb {
    text: String,
}

impl WasmHostDb for TestDb {
    fn file_text(&self, _file: FileId) -> &str {
        &self.text
    }
}

fn write_extension_bundle(root: &Path, id: &str, wat_src: &str) -> PathBuf {
    let ext_dir = root.join(id);
    fs::create_dir_all(&ext_dir).unwrap();

    fs::write(
        ext_dir.join("plugin.wasm"),
        wat::parse_str(wat_src).unwrap(),
    )
    .unwrap();
    fs::write(
        ext_dir.join(nova_ext::MANIFEST_FILE_NAME),
        format!(
            r#"
id = "{id}"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#
        ),
    )
    .unwrap();

    ext_dir
}

fn ctx(db: Arc<TestDb>, config: NovaConfig) -> ExtensionContext<TestDb> {
    ExtensionContext::new(
        db,
        Arc::new(config),
        ProjectId::new(0),
        CancellationToken::new(),
    )
}

#[test]
fn loads_bundle_and_produces_diagnostics() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write_extension_bundle(root, "todo", TODO_WAT);

    let (loaded, errors) = ExtensionManager::load_all_filtered(&[root.to_path_buf()], None, &[]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(loaded.len(), 1);

    let mut registry = ExtensionRegistry::<TestDb>::default();
    let report = ExtensionManager::register_all_best_effort(&mut registry, &loaded);
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.registered.len(), 1);

    let db = Arc::new(TestDb {
        text: "TODO".to_string(),
    });
    let diags = registry.diagnostics(
        ctx(db, NovaConfig::default()),
        DiagnosticParams {
            file: FileId::from_raw(1),
        },
    );
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].message, "TODO found");
    assert_eq!(diags[0].code, "my.plugin.todo");
    assert!(diags[0].span.is_some());
}

#[test]
fn timeout_and_memory_limits_are_enforced_via_config() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    write_extension_bundle(root, "busy", BUSY_LOOP_WAT);
    write_extension_bundle(root, "mem", MEMORY_GROW_WAT);

    let (loaded, errors) = ExtensionManager::load_all_filtered(&[root.to_path_buf()], None, &[]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(loaded.len(), 2);

    let busy = loaded
        .iter()
        .find(|ext| ext.id() == "busy")
        .unwrap()
        .clone();
    let mem = loaded.iter().find(|ext| ext.id() == "mem").unwrap().clone();

    // Busy-loop should be interrupted by the sandbox timeout and record an error.
    let mut registry = ExtensionRegistry::<TestDb>::default();
    // Ensure the registry-level watchdog does not fire before the WASM sandbox timeout.
    registry.options_mut().diagnostic_timeout = Duration::from_millis(500);
    let report = ExtensionManager::register_all_best_effort(&mut registry, &[busy]);
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.registered.len(), 1);

    let mut config = NovaConfig::default();
    config.extensions.wasm_timeout_ms = Some(10);

    let start = Instant::now();
    let diags = registry.diagnostics(
        ctx(
            Arc::new(TestDb {
                text: "TODO".to_string(),
            }),
            config,
        ),
        DiagnosticParams {
            file: FileId::from_raw(1),
        },
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "timeout should interrupt quickly (elapsed={elapsed:?})"
    );
    assert!(diags.is_empty());

    let stats = registry.stats();
    let busy_stats = stats
        .diagnostic
        .get("busy")
        .expect("stats for busy provider");
    assert_eq!(busy_stats.calls_total, 1);
    assert_eq!(busy_stats.timeouts_total, 1);
    assert_eq!(busy_stats.last_error, Some(ProviderLastError::Timeout));

    // Memory growth should be prevented by the sandbox upper bound.
    let mut registry = ExtensionRegistry::<TestDb>::default();
    let report = ExtensionManager::register_all_best_effort(&mut registry, &[mem]);
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.registered.len(), 1);

    let mut config = NovaConfig::default();
    config.extensions.wasm_memory_limit_bytes = Some(64 * 1024);

    let diags = registry.diagnostics(
        ctx(
            Arc::new(TestDb {
                text: "hello".to_string(),
            }),
            config,
        ),
        DiagnosticParams {
            file: FileId::from_raw(1),
        },
    );
    let messages: Vec<_> = diags.into_iter().map(|d| d.message).collect();
    assert!(
        messages.contains(&"grow_failed".to_string()),
        "expected memory.grow to fail under the configured limit (got {messages:?})"
    );
}

#[test]
fn allow_and_deny_lists_filter_extensions() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write_extension_bundle(root, "a", TODO_WAT);
    write_extension_bundle(root, "b", TODO_WAT);

    let (loaded, errors) =
        ExtensionManager::load_all_filtered(&[root.to_path_buf()], Some(&["a".to_string()]), &[]);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id(), "a");
    assert_eq!(errors.len(), 1);
    assert!(matches!(
        errors[0],
        LoadError::NotAllowedByConfig { ref id, .. } if id == "b"
    ));

    let (loaded, errors) =
        ExtensionManager::load_all_filtered(&[root.to_path_buf()], None, &["b".to_string()]);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id(), "a");
    assert_eq!(errors.len(), 1);
    assert!(matches!(
        errors[0],
        LoadError::DeniedByConfig { ref id, .. } if id == "b"
    ));
}
