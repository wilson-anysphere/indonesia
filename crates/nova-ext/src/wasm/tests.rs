use super::{WasmCapabilities, WasmHostDb, WasmLoadError, WasmPlugin, WasmPluginConfig};
use crate::traits::{
    CodeActionParams, CodeActionProvider, CompletionParams, CompletionProvider, DiagnosticParams,
    DiagnosticProvider, InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider,
};
use crate::types::{CodeAction, InlayHint, NavigationTarget};
use crate::{ExtensionContext, ExtensionRegistry, RegisterError};
use nova_config::NovaConfig;
use nova_core::FileId;
use nova_core::ProjectId;
use nova_scheduler::CancellationToken;
use nova_types::{ClassId, CompletionItem, Diagnostic, Span};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct TestDb {
    text: String,
    path: Option<PathBuf>,
}

impl WasmHostDb for TestDb {
    fn file_text(&self, _file: FileId) -> &str {
        &self.text
    }

    fn file_path(&self, _file: FileId) -> Option<&Path> {
        self.path.as_deref()
    }
}

fn ctx(db: Arc<TestDb>) -> ExtensionContext<TestDb> {
    ExtensionContext::new(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
        CancellationToken::new(),
    )
}

fn ctx_with_config(db: Arc<TestDb>, config: NovaConfig) -> ExtensionContext<TestDb> {
    ExtensionContext::new(
        db,
        Arc::new(config),
        ProjectId::new(0),
        CancellationToken::new(),
    )
}

const WAT_DIAG_AND_COMPLETIONS: &str = r#"
(module
  (memory (export "memory") 1)

  (global $heap (mut i32) (i32.const 1024))

  (func $nova_ext_alloc (export "nova_ext_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr)
  )

  (func $nova_ext_free (export "nova_ext_free") (param i32 i32)
    nop
  )

  (func (export "nova_ext_abi_version") (result i32)
    (i32.const 1)
  )

  ;; diagnostics + completions
  (func (export "nova_ext_capabilities") (result i32)
    (i32.const 3)
  )

  ;; Static JSON payloads.
  (data (i32.const 0) "[{\"message\":\"found needle\",\"code\":\"my.plugin.code\"}]")
  (data (i32.const 64) "[]")
  (data (i32.const 128) "[{\"label\":\"from-wasm\"}]")

  (func $contains_needle (param $ptr i32) (param $len i32) (result i32)
    (local $i i32)
    (local $end i32)
    (if (i32.lt_u (local.get $len) (i32.const 6))
      (then (return (i32.const 0))))
    (local.set $end (i32.sub (local.get $len) (i32.const 6)))
    (local.set $i (i32.const 0))
    (block $break
      (loop $loop
        (br_if $break (i32.gt_u (local.get $i) (local.get $end)))
        (if
          (i32.and
            (i32.eq (i32.load8_u (i32.add (local.get $ptr) (local.get $i))) (i32.const 110))
            (i32.and
              (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 1)))) (i32.const 101))
              (i32.and
                (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 2)))) (i32.const 101))
                (i32.and
                  (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 3)))) (i32.const 100))
                  (i32.and
                    (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 4)))) (i32.const 108))
                    (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 5)))) (i32.const 101))
                  )
                )
              )
            )
          )
          (then (return (i32.const 1)))
        )
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $loop)
      )
    )
    (i32.const 0)
  )

  (func (export "nova_ext_diagnostics") (param $req_ptr i32) (param $req_len i32) (result i64)
    (local $out_ptr i32)
    (local $out_len i32)
    (if (call $contains_needle (local.get $req_ptr) (local.get $req_len))
      (then
        (local.set $out_len (i32.const 52))
        (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))
        (memory.copy (local.get $out_ptr) (i32.const 0) (local.get $out_len))
      )
      (else
        (local.set $out_len (i32.const 2))
        (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))
        (memory.copy (local.get $out_ptr) (i32.const 64) (local.get $out_len))
      )
    )
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out_len)) (i64.const 32))
      (i64.extend_i32_u (local.get $out_ptr))
    )
  )

  (func (export "nova_ext_completions") (param i32 i32) (result i64)
    (local $out_ptr i32)
    (local $out_len i32)
    (local.set $out_len (i32.const 23))
    (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))
    (memory.copy (local.get $out_ptr) (i32.const 128) (local.get $out_len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out_len)) (i64.const 32))
      (i64.extend_i32_u (local.get $out_ptr))
    )
  )
)
"#;

const WAT_ABI_MISMATCH: &str = r#"
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
  (func (export "nova_ext_abi_version") (result i32) (i32.const 2))
  (func (export "nova_ext_capabilities") (result i32) (i32.const 0))
)
"#;

const WAT_CAPABILITY_MISSING_EXPORT: &str = r#"
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
  ;; Claims diagnostics, but does not export `nova_ext_diagnostics`.
  (func (export "nova_ext_capabilities") (result i32) (i32.const 1))
)
"#;

const WAT_BUSY_LOOP: &str = r#"
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

const WAT_MEMORY_GROW: &str = r#"
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

const WAT_ALL_PROVIDERS: &str = r#"
(module
  (memory (export "memory") 1)

  (global $heap (mut i32) (i32.const 1024))

  (func $nova_ext_alloc (export "nova_ext_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr)
  )

  (func $nova_ext_free (export "nova_ext_free") (param i32 i32)
    nop
  )

  (func (export "nova_ext_abi_version") (result i32)
    (i32.const 1)
  )

  ;; all 5 providers
  (func (export "nova_ext_capabilities") (result i32)
    (i32.const 31)
  )

  (data (i32.const 0) "[{\"message\":\"diag\"}]")
  (data (i32.const 64) "[{\"label\":\"comp\"}]")
  (data (i32.const 128) "[{\"title\":\"action\",\"kind\":\"quickfix\"}]")
  (data (i32.const 256) "[{\"label\":\"hint\",\"span\":{\"start\":0,\"end\":1}}]")
  (data (i32.const 384) "[{\"fileId\":1,\"span\":{\"start\":0,\"end\":1},\"label\":\"file-target\"}]")
  (data (i32.const 512) "[{\"fileId\":2,\"span\":{\"start\":2,\"end\":3},\"label\":\"class-target\"}]")

  (func $contains_class (param $ptr i32) (param $len i32) (result i32)
    (local $i i32)
    (local $end i32)
    (if (i32.lt_u (local.get $len) (i32.const 5))
      (then (return (i32.const 0))))
    (local.set $end (i32.sub (local.get $len) (i32.const 5)))
    (local.set $i (i32.const 0))
    (block $break
      (loop $loop
        (br_if $break (i32.gt_u (local.get $i) (local.get $end)))
        (if
          (i32.and
            (i32.eq (i32.load8_u (i32.add (local.get $ptr) (local.get $i))) (i32.const 99)) ;; 'c'
            (i32.and
              (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 1)))) (i32.const 108)) ;; 'l'
              (i32.and
                (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 2)))) (i32.const 97)) ;; 'a'
                (i32.and
                  (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 3)))) (i32.const 115)) ;; 's'
                  (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 4)))) (i32.const 115)) ;; 's'
                )
              )
            )
          )
          (then (return (i32.const 1)))
        )
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $loop)
      )
    )
    (i32.const 0)
  )

  (func $return_static (param $src i32) (param $len i32) (result i64)
    (local $out_ptr i32)
    (local.set $out_ptr (call $nova_ext_alloc (local.get $len)))
    (memory.copy (local.get $out_ptr) (local.get $src) (local.get $len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $len)) (i64.const 32))
      (i64.extend_i32_u (local.get $out_ptr))
    )
  )

  (func (export "nova_ext_diagnostics") (param i32 i32) (result i64)
    (call $return_static (i32.const 0) (i32.const 20))
  )

  (func (export "nova_ext_completions") (param i32 i32) (result i64)
    (call $return_static (i32.const 64) (i32.const 18))
  )

  (func (export "nova_ext_code_actions") (param i32 i32) (result i64)
    (call $return_static (i32.const 128) (i32.const 38))
  )

  (func (export "nova_ext_inlay_hints") (param i32 i32) (result i64)
    (call $return_static (i32.const 256) (i32.const 45))
  )

  (func (export "nova_ext_navigation") (param $req_ptr i32) (param $req_len i32) (result i64)
    (if (call $contains_class (local.get $req_ptr) (local.get $req_len))
      (then (return (call $return_static (i32.const 512) (i32.const 64))))
    )
    (call $return_static (i32.const 384) (i32.const 63))
  )
)
"#;

#[test]
fn abi_version_mismatch_is_rejected() {
    let err = match WasmPlugin::from_wat(
        "abi-mismatch",
        WAT_ABI_MISMATCH,
        WasmPluginConfig::default(),
    ) {
        Ok(_) => panic!("module should be rejected"),
        Err(err) => err,
    };
    match err {
        WasmLoadError::AbiVersionMismatch { expected, found } => {
            assert_eq!(expected, 1);
            assert_eq!(found, 2);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn declared_capability_requires_export() {
    let err = match WasmPlugin::from_wat(
        "missing-export",
        WAT_CAPABILITY_MISSING_EXPORT,
        WasmPluginConfig::default(),
    ) {
        Ok(_) => panic!("module should be rejected"),
        Err(err) => err,
    };

    match err {
        WasmLoadError::MissingExport(name) => assert_eq!(name, "nova_ext_diagnostics"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn ptr_len_roundtrip_and_basic_call_works() {
    let plugin = WasmPlugin::from_wat(
        "roundtrip",
        WAT_DIAG_AND_COMPLETIONS,
        WasmPluginConfig::default(),
    )
    .expect("load plugin");

    let file = FileId::from_raw(1);
    let db = Arc::new(TestDb {
        text: "this file contains needle".to_string(),
        path: Some(PathBuf::from("/test/File.java")),
    });

    let diags = plugin.provide_diagnostics(ctx(Arc::clone(&db)), DiagnosticParams { file });
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].message, "found needle");
    assert_eq!(diags[0].code.as_ref(), "my.plugin.code");

    let completions = plugin.provide_completions(ctx(db), CompletionParams { file, offset: 0 });
    assert_eq!(
        completions,
        vec![CompletionItem {
            label: "from-wasm".to_string(),
            detail: None,
        }]
    );
}

#[test]
fn capabilities_drive_registry_registration() {
    let plugin = Arc::new(
        WasmPlugin::from_wat(
            "capabilities",
            WAT_DIAG_AND_COMPLETIONS,
            WasmPluginConfig::default(),
        )
        .expect("load plugin"),
    );

    assert!(plugin
        .capabilities()
        .contains(WasmCapabilities::DIAGNOSTICS));
    assert!(plugin
        .capabilities()
        .contains(WasmCapabilities::COMPLETIONS));
    assert!(!plugin
        .capabilities()
        .contains(WasmCapabilities::CODE_ACTIONS));
    assert!(!plugin.capabilities().contains(WasmCapabilities::NAVIGATION));
    assert!(!plugin
        .capabilities()
        .contains(WasmCapabilities::INLAY_HINTS));

    let mut registry = ExtensionRegistry::<TestDb>::default();
    plugin.register(&mut registry).unwrap();

    // These should now collide due to registration.
    assert_eq!(
        registry.register_diagnostic_provider(Arc::new(DummyDiagProvider {
            id: plugin.id().to_string()
        })),
        Err(RegisterError::DuplicateId {
            kind: "diagnostic",
            id: plugin.id().to_string()
        })
    );
    assert_eq!(
        registry.register_completion_provider(Arc::new(DummyCompletionProvider {
            id: plugin.id().to_string()
        })),
        Err(RegisterError::DuplicateId {
            kind: "completion",
            id: plugin.id().to_string()
        })
    );

    // But non-capabilities should still accept the same ID.
    registry
        .register_code_action_provider(Arc::new(DummyCodeActionProvider {
            id: plugin.id().to_string(),
        }))
        .unwrap();
    registry
        .register_navigation_provider(Arc::new(DummyNavigationProvider {
            id: plugin.id().to_string(),
        }))
        .unwrap();
    registry
        .register_inlay_hint_provider(Arc::new(DummyInlayHintProvider {
            id: plugin.id().to_string(),
        }))
        .unwrap();
}

#[test]
fn busy_loop_is_interrupted_by_timeout() {
    let mut config = WasmPluginConfig::default();
    config.timeout = Duration::from_millis(10);
    let plugin =
        WasmPlugin::from_wat("busy", WAT_BUSY_LOOP, config).expect("load busy-loop plugin");

    let file = FileId::from_raw(1);
    let db = Arc::new(TestDb {
        text: "needle".to_string(),
        path: None,
    });
    let start = Instant::now();
    let diags = plugin.provide_diagnostics(ctx(db), DiagnosticParams { file });
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(200),
        "call should be interrupted quickly (elapsed={elapsed:?})"
    );
    assert!(
        diags.is_empty(),
        "timeout should be treated as provider failure"
    );
}

#[test]
fn memory_limit_prevents_unbounded_growth() {
    let mut config = WasmPluginConfig::default();
    config.max_memory_bytes = 64 * 1024; // 1 page
    let plugin = WasmPlugin::from_wat("memlimit", WAT_MEMORY_GROW, config).expect("load module");

    let file = FileId::from_raw(1);
    let db = Arc::new(TestDb {
        text: "hello".to_string(),
        path: None,
    });
    let diags = plugin.provide_diagnostics(ctx(db), DiagnosticParams { file });
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].message, "grow_failed");
}

#[test]
fn nova_config_overrides_wasm_timeout_and_memory_limits() {
    // Timeout override.
    let mut plugin_config = WasmPluginConfig::default();
    plugin_config.timeout = Duration::from_millis(500);
    let plugin =
        WasmPlugin::from_wat("busy-config", WAT_BUSY_LOOP, plugin_config).expect("load module");

    let file = FileId::from_raw(1);
    let db = Arc::new(TestDb {
        text: "needle".to_string(),
        path: None,
    });

    let mut config = NovaConfig::default();
    config.extensions.wasm_timeout_ms = Some(10);

    let start = Instant::now();
    let diags = plugin.provide_diagnostics(
        ctx_with_config(Arc::clone(&db), config),
        DiagnosticParams { file },
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "call should be interrupted quickly by NovaConfig override (elapsed={elapsed:?})"
    );
    assert!(diags.is_empty());

    // Memory override.
    let mut plugin_config = WasmPluginConfig::default();
    plugin_config.max_memory_bytes = 512 * 1024; // allow growth
    let plugin =
        WasmPlugin::from_wat("mem-config", WAT_MEMORY_GROW, plugin_config).expect("load module");

    let mut config = NovaConfig::default();
    config.extensions.wasm_memory_limit_bytes = Some(64 * 1024); // disallow growth

    let diags = plugin.provide_diagnostics(ctx_with_config(db, config), DiagnosticParams { file });
    assert_eq!(diags.len(), 1);
    assert_eq!(
        diags[0].message, "grow_failed",
        "NovaConfig memory limit should override the plugin config"
    );
}

#[test]
fn all_provider_kinds_roundtrip() {
    let plugin = Arc::new(
        WasmPlugin::from_wat("all", WAT_ALL_PROVIDERS, WasmPluginConfig::default())
            .expect("load module"),
    );
    assert_eq!(plugin.capabilities().bits(), 31);

    let mut registry = ExtensionRegistry::<TestDb>::default();
    plugin.register(&mut registry).unwrap();

    let id = plugin.id().to_string();
    assert_eq!(
        registry.register_diagnostic_provider(Arc::new(DummyDiagProvider { id: id.clone() })),
        Err(RegisterError::DuplicateId {
            kind: "diagnostic",
            id: id.clone()
        })
    );
    assert_eq!(
        registry.register_completion_provider(Arc::new(DummyCompletionProvider { id: id.clone() })),
        Err(RegisterError::DuplicateId {
            kind: "completion",
            id: id.clone()
        })
    );
    assert_eq!(
        registry
            .register_code_action_provider(Arc::new(DummyCodeActionProvider { id: id.clone() })),
        Err(RegisterError::DuplicateId {
            kind: "code_action",
            id: id.clone()
        })
    );
    assert_eq!(
        registry.register_navigation_provider(Arc::new(DummyNavigationProvider { id: id.clone() })),
        Err(RegisterError::DuplicateId {
            kind: "navigation",
            id: id.clone()
        })
    );
    assert_eq!(
        registry.register_inlay_hint_provider(Arc::new(DummyInlayHintProvider { id: id.clone() })),
        Err(RegisterError::DuplicateId {
            kind: "inlay_hint",
            id
        })
    );

    let file = FileId::from_raw(1);
    let db = Arc::new(TestDb {
        text: "hello".to_string(),
        path: None,
    });

    let actions = plugin.provide_code_actions(
        ctx(Arc::clone(&db)),
        CodeActionParams {
            file,
            span: Some(Span::new(0, 1)),
        },
    );
    assert_eq!(
        actions,
        vec![CodeAction {
            title: "action".to_string(),
            kind: Some("quickfix".to_string()),
        }]
    );

    let hints = plugin.provide_inlay_hints(ctx(Arc::clone(&db)), InlayHintParams { file });
    assert_eq!(
        hints,
        vec![InlayHint {
            span: Some(Span::new(0, 1)),
            label: "hint".to_string(),
        }]
    );

    let file_targets = plugin.provide_navigation(
        ctx(Arc::clone(&db)),
        NavigationParams {
            symbol: crate::types::Symbol::File(file),
        },
    );
    assert_eq!(
        file_targets,
        vec![NavigationTarget {
            file,
            span: Some(Span::new(0, 1)),
            label: "file-target".to_string(),
        }]
    );

    let class_targets = plugin.provide_navigation(
        ctx(db),
        NavigationParams {
            symbol: crate::types::Symbol::Class(ClassId::from_raw(7)),
        },
    );
    assert_eq!(
        class_targets,
        vec![NavigationTarget {
            file: FileId::from_raw(2),
            span: Some(Span::new(2, 3)),
            label: "class-target".to_string(),
        }]
    );
}

#[derive(Clone)]
struct DummyDiagProvider {
    id: String,
}

impl DiagnosticProvider<TestDb> for DummyDiagProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_diagnostics(
        &self,
        _ctx: ExtensionContext<TestDb>,
        _params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        Vec::new()
    }
}

#[derive(Clone)]
struct DummyCompletionProvider {
    id: String,
}

impl CompletionProvider<TestDb> for DummyCompletionProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_completions(
        &self,
        _ctx: ExtensionContext<TestDb>,
        _params: CompletionParams,
    ) -> Vec<CompletionItem> {
        Vec::new()
    }
}

#[derive(Clone)]
struct DummyCodeActionProvider {
    id: String,
}

impl CodeActionProvider<TestDb> for DummyCodeActionProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_code_actions(
        &self,
        _ctx: ExtensionContext<TestDb>,
        _params: CodeActionParams,
    ) -> Vec<CodeAction> {
        Vec::new()
    }
}

#[derive(Clone)]
struct DummyNavigationProvider {
    id: String,
}

impl NavigationProvider<TestDb> for DummyNavigationProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_navigation(
        &self,
        _ctx: ExtensionContext<TestDb>,
        _params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        Vec::new()
    }
}

#[derive(Clone)]
struct DummyInlayHintProvider {
    id: String,
}

impl InlayHintProvider<TestDb> for DummyInlayHintProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_inlay_hints(
        &self,
        _ctx: ExtensionContext<TestDb>,
        _params: InlayHintParams,
    ) -> Vec<InlayHint> {
        Vec::new()
    }
}
