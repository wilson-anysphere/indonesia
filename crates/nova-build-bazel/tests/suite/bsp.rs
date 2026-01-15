use crate::fake_bsp::{spawn_fake_bsp_server, FakeBspServerConfig};
use nova_build_bazel::bsp::{
    BuildTarget, BuildTargetIdentifier, CompileProvider, Diagnostic, InitializeBuildResult,
    JavacOptionsItem, JavacProvider, Position, PublishDiagnosticsParams, Range, ServerCapabilities,
    TextDocumentIdentifier,
};
use nova_build_bazel::{test_support::EnvVarGuard, BspServerConfig, BspWorkspace, JavaCompileInfo};
use tempfile::tempdir;

#[test]
fn bsp_initialize_handles_interleaved_server_request_before_response() {
    let root = tempdir().unwrap();

    let config = FakeBspServerConfig {
        initialize: InitializeBuildResult {
            display_name: "fake-bsp".to_string(),
            version: "0.1.0".to_string(),
            bsp_version: "2.1.0".to_string(),
            capabilities: ServerCapabilities {
                compile_provider: Some(CompileProvider {
                    language_ids: vec!["java".to_string()],
                }),
                javac_provider: Some(JavacProvider {
                    language_ids: vec!["java".to_string()],
                }),
            },
        },
        targets: Vec::new(),
        inverse_sources: std::collections::BTreeMap::new(),
        javac_options: Vec::new(),
        compile_status_code: 0,
        diagnostics: Vec::new(),
        send_server_request_before_initialize_response: true,
    };

    let (client, server) = spawn_fake_bsp_server(config).unwrap();
    let workspace = BspWorkspace::from_client(root.path().to_path_buf(), client).unwrap();

    drop(workspace);
    server.join();
}

#[test]
fn bsp_workspace_connect_uses_dot_bsp_discovery_when_config_is_default() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _program_guard = EnvVarGuard::remove("NOVA_BSP_PROGRAM");
    let _args_guard = EnvVarGuard::remove("NOVA_BSP_ARGS");

    let root = tempdir().unwrap();
    let bsp_dir = root.path().join(".bsp");
    std::fs::create_dir_all(&bsp_dir).unwrap();

    let fake_server_path = env!("CARGO_BIN_EXE_fake_bsp_server");
    let json = serde_json::json!({
        "argv": [fake_server_path],
        "languages": ["java"],
    });
    std::fs::write(
        bsp_dir.join("server.json"),
        serde_json::to_string(&json).unwrap(),
    )
    .unwrap();

    let workspace = BspWorkspace::connect(root.path().to_path_buf(), BspServerConfig::default())
        .expect("connect should use .bsp discovery");
    assert_eq!(workspace.server_info().display_name, "fake-bsp");
}

#[test]
fn bsp_compile_collects_diagnostics_on_non_zero_status() {
    let root = tempdir().unwrap();
    let src_dir = root.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("My File.java");
    std::fs::write(&file_path, "class Hello {}").unwrap();

    let file_uri =
        nova_core::path_to_file_uri(&nova_core::AbsPathBuf::new(file_path.clone()).unwrap())
            .unwrap();

    let target = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://target1".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: Some("//java:hello".to_string()),
    };

    let diagnostics = PublishDiagnosticsParams {
        text_document: TextDocumentIdentifier { uri: file_uri },
        diagnostics: vec![Diagnostic {
            range: Range {
                start: Position {
                    line: 2,
                    character: 1,
                },
                end: Position {
                    line: 2,
                    character: 5,
                },
            },
            severity: Some(1),
            message: "boom".to_string(),
        }],
        reset: Some(true),
    };

    let config = FakeBspServerConfig {
        initialize: InitializeBuildResult {
            display_name: "fake-bsp".to_string(),
            version: "0.1.0".to_string(),
            bsp_version: "2.1.0".to_string(),
            capabilities: ServerCapabilities {
                compile_provider: Some(CompileProvider {
                    language_ids: vec!["java".to_string()],
                }),
                javac_provider: Some(JavacProvider {
                    language_ids: vec!["java".to_string()],
                }),
            },
        },
        targets: vec![target.clone()],
        inverse_sources: std::collections::BTreeMap::new(),
        javac_options: Vec::new(),
        compile_status_code: 2,
        diagnostics: vec![diagnostics],
        send_server_request_before_initialize_response: false,
    };

    let (client, server) = spawn_fake_bsp_server(config).unwrap();
    let mut workspace = BspWorkspace::from_client(root.path().to_path_buf(), client).unwrap();

    let id = workspace
        .resolve_build_target("//java:hello")
        .unwrap()
        .expect("target not resolved");
    let outcome = workspace.compile(&[id]).unwrap();

    assert_eq!(outcome.status_code, 2);
    assert_eq!(outcome.diagnostics.len(), 1);
    let diag = &outcome.diagnostics[0];
    assert_eq!(diag.file, file_path);
    assert_eq!(diag.message, "boom");
    assert_eq!(diag.severity, nova_core::BuildDiagnosticSeverity::Error);
    assert_eq!(diag.source.as_deref(), Some("fake-bsp"));
    assert_eq!(diag.range.start.line, 2);
    assert_eq!(diag.range.start.character, 1);

    drop(workspace);
    server.join();
}

#[test]
fn bsp_javac_options_multi_target_conversion() {
    let root = tempdir().unwrap();

    let target1 = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://t1".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: Some("//pkg:t1".to_string()),
    };
    let target2 = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://t2".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: Some("//pkg:t2".to_string()),
    };

    let javac1 = JavacOptionsItem {
        target: target1.id.clone(),
        classpath: vec!["a.jar".to_string()],
        class_directory: "out/classes".to_string(),
        options: vec![
            "--module-path".to_string(),
            "mods:moremods".to_string(),
            "--release".to_string(),
            "21".to_string(),
            "--enable-preview".to_string(),
        ],
    };

    // Intentionally omit the classpath field and provide `-classpath` in options to
    // ensure the converter is resilient.
    let javac2 = JavacOptionsItem {
        target: target2.id.clone(),
        classpath: Vec::new(),
        class_directory: "out/test".to_string(),
        options: vec![
            "-classpath".to_string(),
            "b.jar:c.jar".to_string(),
            "--source".to_string(),
            "17".to_string(),
            "--target".to_string(),
            "17".to_string(),
        ],
    };

    let config = FakeBspServerConfig {
        initialize: InitializeBuildResult {
            display_name: "fake-bsp".to_string(),
            version: "0.1.0".to_string(),
            bsp_version: "2.1.0".to_string(),
            capabilities: ServerCapabilities {
                compile_provider: Some(CompileProvider {
                    language_ids: vec!["java".to_string()],
                }),
                javac_provider: Some(JavacProvider {
                    language_ids: vec!["java".to_string()],
                }),
            },
        },
        targets: vec![target1.clone(), target2.clone()],
        inverse_sources: std::collections::BTreeMap::new(),
        javac_options: vec![javac1, javac2],
        compile_status_code: 0,
        diagnostics: Vec::new(),
        send_server_request_before_initialize_response: false,
    };

    let (client, server) = spawn_fake_bsp_server(config).unwrap();
    let mut workspace = BspWorkspace::from_client(root.path().to_path_buf(), client).unwrap();

    let items = workspace
        .javac_options(&[target1.id.clone(), target2.id.clone()])
        .unwrap();

    assert_eq!(items.len(), 2);

    let mut info_by_uri = std::collections::HashMap::<String, JavaCompileInfo>::new();
    for (id, info) in items {
        info_by_uri.insert(id.uri, info);
    }

    let info1 = info_by_uri.get("test://t1").unwrap();
    assert_eq!(info1.classpath, vec!["a.jar".to_string()]);
    assert_eq!(
        info1.module_path,
        vec!["mods".to_string(), "moremods".to_string()]
    );
    assert_eq!(info1.output_dir.as_deref(), Some("out/classes"));
    assert_eq!(info1.release.as_deref(), Some("21"));
    assert!(info1.preview);

    let info2 = info_by_uri.get("test://t2").unwrap();
    assert_eq!(
        info2.classpath,
        vec!["b.jar".to_string(), "c.jar".to_string()]
    );
    assert_eq!(info2.output_dir.as_deref(), Some("out/test"));
    assert_eq!(info2.source.as_deref(), Some("17"));
    assert_eq!(info2.target.as_deref(), Some("17"));

    drop(workspace);
    server.join();
}
