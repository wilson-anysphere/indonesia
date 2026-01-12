use crate::fake_bsp::{spawn_fake_bsp_server, FakeBspServerConfig};
use nova_build_bazel::bsp::{
    BuildTarget, BuildTargetIdentifier, CompileProvider, InitializeBuildResult, JavacOptionsItem,
    JavacProvider, ServerCapabilities,
};
use nova_build_bazel::test_support::EnvVarGuard;
use nova_build_bazel::{
    BazelWorkspace, BspServerConfig, BspWorkspace, CommandOutput, CommandRunner,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

#[derive(Clone, Debug, Default)]
struct RecordingRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl RecordingRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[derive(Clone, Debug)]
struct QueryingRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    query_stdout: String,
}

impl QueryingRunner {
    fn new(query_stdout: impl Into<String>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            query_stdout: query_stdout.into(),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for QueryingRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        let stdout = if args.first() == Some(&"query") {
            self.query_stdout.clone()
        } else {
            String::new()
        };

        Ok(CommandOutput {
            stdout,
            stderr: String::new(),
        })
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        // Return empty stdout for any query; this is sufficient for file-digest collection
        // in BazelWorkspace.
        Ok(CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

fn server_caps() -> ServerCapabilities {
    ServerCapabilities {
        compile_provider: Some(CompileProvider {
            language_ids: vec!["java".to_string()],
        }),
        javac_provider: Some(JavacProvider {
            language_ids: vec!["java".to_string()],
        }),
    }
}

#[test]
fn bazel_workspace_java_targets_prefers_bsp_buildtargets() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _use_bsp_guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some("1"));

    let root = tempdir().unwrap();

    let java_target = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://t1".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: Some("//pkg:t1".to_string()),
    };
    let non_java_target = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://t2".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["scala".to_string()],
        display_name: Some("//pkg:t2".to_string()),
    };

    let config = FakeBspServerConfig {
        initialize: InitializeBuildResult {
            display_name: "fake-bsp".to_string(),
            version: "0.1.0".to_string(),
            bsp_version: "2.1.0".to_string(),
            capabilities: server_caps(),
        },
        targets: vec![java_target.clone(), non_java_target],
        inverse_sources: std::collections::BTreeMap::new(),
        javac_options: Vec::new(),
        compile_status_code: 0,
        diagnostics: Vec::new(),
        send_server_request_before_initialize_response: false,
    };

    let (client, server) = spawn_fake_bsp_server(config).unwrap();
    let bsp_workspace = BspWorkspace::from_client(root.path().to_path_buf(), client).unwrap();

    let runner = RecordingRunner::default();
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone())
        .unwrap()
        .with_bsp_workspace(bsp_workspace);

    let targets = workspace.java_targets().unwrap();
    assert_eq!(targets, vec!["//pkg:t1".to_string()]);

    // BSP path should avoid invoking `bazel query` for target discovery.
    assert!(runner.calls().is_empty());

    drop(workspace);
    server.join();
}

#[test]
fn bazel_workspace_java_targets_falls_back_to_bsp_target_id_uri() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _use_bsp_guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some("1"));

    let root = tempdir().unwrap();

    let java_target = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "//pkg:t1".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: None,
    };

    let config = FakeBspServerConfig {
        initialize: InitializeBuildResult {
            display_name: "fake-bsp".to_string(),
            version: "0.1.0".to_string(),
            bsp_version: "2.1.0".to_string(),
            capabilities: server_caps(),
        },
        targets: vec![java_target],
        inverse_sources: std::collections::BTreeMap::new(),
        javac_options: Vec::new(),
        compile_status_code: 0,
        diagnostics: Vec::new(),
        send_server_request_before_initialize_response: false,
    };

    let (client, server) = spawn_fake_bsp_server(config).unwrap();
    let bsp_workspace = BspWorkspace::from_client(root.path().to_path_buf(), client).unwrap();

    let runner = RecordingRunner::default();
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone())
        .unwrap()
        .with_bsp_workspace(bsp_workspace);

    let targets = workspace.java_targets().unwrap();
    assert_eq!(targets, vec!["//pkg:t1".to_string()]);

    // BSP path should avoid invoking `bazel query` for target discovery.
    assert!(runner.calls().is_empty());

    drop(workspace);
    server.join();
}

#[test]
fn bazel_workspace_java_targets_falls_back_to_query_when_bsp_buildtargets_times_out() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _use_bsp_guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some("1"));
    let _program_guard = EnvVarGuard::remove("NOVA_BSP_PROGRAM");
    let _args_guard = EnvVarGuard::remove("NOVA_BSP_ARGS");

    let root = tempdir().unwrap();
    // Create a minimal workspace marker to match real usage patterns.
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();

    let fake_server_path = env!("CARGO_BIN_EXE_fake_bsp_server");
    let bsp_workspace = {
        // Keep the default request timeout for the initialization handshake. We'll shorten it
        // after connect so the hanging `workspace/buildTargets` request times out quickly.
        let _unset_timeout = EnvVarGuard::set("NOVA_BSP_REQUEST_TIMEOUT_MS", None);

        BspWorkspace::connect(
            root.path().to_path_buf(),
            BspServerConfig {
                program: fake_server_path.to_string(),
                args: vec![
                    "--hang-method".to_string(),
                    "workspace/buildTargets".to_string(),
                ],
            },
        )
        .unwrap()
    };

    let _request_timeout_guard = EnvVarGuard::set("NOVA_BSP_REQUEST_TIMEOUT_MS", Some("50"));

    let runner = QueryingRunner::new("//pkg:from_query\n");
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone())
        .unwrap()
        .with_bsp_workspace(bsp_workspace);

    let targets = workspace.java_targets().unwrap();
    assert_eq!(targets, vec!["//pkg:from_query".to_string()]);

    // Ensure we actually fell back to `bazel query` rather than returning BSP results.
    assert!(runner
        .calls()
        .into_iter()
        .any(|args| args.first().map(String::as_str) == Some("query")));
}

#[test]
fn bazel_workspace_target_compile_info_prefers_bsp_javac_options() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _use_bsp_guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some("1"));

    let root = tempdir().unwrap();

    let java_target = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://t1".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: Some("//pkg:t1".to_string()),
    };

    let javac = JavacOptionsItem {
        target: java_target.id.clone(),
        classpath: vec!["a.jar".to_string()],
        class_directory: "out/classes".to_string(),
        options: vec!["--release".to_string(), "17".to_string()],
    };

    let config = FakeBspServerConfig {
        initialize: InitializeBuildResult {
            display_name: "fake-bsp".to_string(),
            version: "0.1.0".to_string(),
            bsp_version: "2.1.0".to_string(),
            capabilities: server_caps(),
        },
        targets: vec![java_target.clone()],
        inverse_sources: std::collections::BTreeMap::new(),
        javac_options: vec![javac],
        compile_status_code: 0,
        diagnostics: Vec::new(),
        send_server_request_before_initialize_response: false,
    };

    let (client, server) = spawn_fake_bsp_server(config).unwrap();
    let bsp_workspace = BspWorkspace::from_client(root.path().to_path_buf(), client).unwrap();

    let runner = RecordingRunner::default();
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone())
        .unwrap()
        .with_bsp_workspace(bsp_workspace);

    let info = workspace.target_compile_info("//pkg:t1").unwrap();
    assert_eq!(info.classpath, vec!["a.jar".to_string()]);
    assert_eq!(info.output_dir.as_deref(), Some("out/classes"));
    assert_eq!(info.release.as_deref(), Some("17"));
    assert_eq!(info.source.as_deref(), Some("17"));
    assert_eq!(info.target.as_deref(), Some("17"));

    // BSP path should not execute an `aquery`.
    let saw_aquery = runner
        .calls()
        .into_iter()
        .any(|args| args.first().map(String::as_str) == Some("aquery"));
    assert!(!saw_aquery);

    drop(workspace);
    server.join();
}

#[test]
fn bazel_workspace_target_compile_info_cache_is_invalidated_by_bazelrc_imports() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _use_bsp_guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some("1"));

    let root = tempdir().unwrap();

    // Create a `.bazelrc` that imports another file. Changes to the imported file can affect query
    // evaluation and should invalidate cached compile-info entries.
    std::fs::create_dir_all(root.path().join("tools")).unwrap();
    std::fs::write(root.path().join(".bazelrc"), "try-import tools/bazel.rc\n").unwrap();
    std::fs::write(root.path().join("tools/bazel.rc"), "common --color=no\n").unwrap();

    let java_target = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://t1".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: Some("//pkg:t1".to_string()),
    };

    let javac = JavacOptionsItem {
        target: java_target.id.clone(),
        classpath: vec!["a.jar".to_string()],
        class_directory: "out/classes".to_string(),
        options: vec!["--release".to_string(), "17".to_string()],
    };

    let config = FakeBspServerConfig {
        initialize: InitializeBuildResult {
            display_name: "fake-bsp".to_string(),
            version: "0.1.0".to_string(),
            bsp_version: "2.1.0".to_string(),
            capabilities: server_caps(),
        },
        targets: vec![java_target.clone()],
        inverse_sources: std::collections::BTreeMap::new(),
        javac_options: vec![javac],
        compile_status_code: 0,
        diagnostics: Vec::new(),
        send_server_request_before_initialize_response: false,
    };

    let (client, server) = spawn_fake_bsp_server(config).unwrap();
    let bsp_workspace = BspWorkspace::from_client(root.path().to_path_buf(), client).unwrap();

    let runner = RecordingRunner::default();
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone())
        .unwrap()
        .with_bsp_workspace(bsp_workspace);

    let _ = workspace.target_compile_info("//pkg:t1").unwrap();

    let count_javac_options = || {
        server
            .requests()
            .iter()
            .filter(|msg| {
                msg.get("method").and_then(|v| v.as_str()) == Some("buildTarget/javacOptions")
            })
            .count()
    };

    assert_eq!(count_javac_options(), 1);

    // Cache hit: no additional BSP requests.
    let _ = workspace.target_compile_info("//pkg:t1").unwrap();
    assert_eq!(count_javac_options(), 1);

    // Editing an imported bazelrc file should invalidate the cached entry and force a new BSP
    // request.
    std::fs::write(root.path().join("tools/bazel.rc"), "common --color=yes\n").unwrap();
    let _ = workspace.target_compile_info("//pkg:t1").unwrap();
    assert_eq!(count_javac_options(), 2);

    // BSP path should not invoke any `bazel` subprocesses.
    assert!(runner.calls().is_empty());

    drop(workspace);
    server.join();
}
