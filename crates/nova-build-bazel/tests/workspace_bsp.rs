#![cfg(feature = "bsp")]

mod fake_bsp;

use fake_bsp::{spawn_fake_bsp_server, FakeBspServerConfig};
use nova_build_bazel::bsp::{
    BuildTarget, BuildTargetIdentifier, CompileProvider, InitializeBuildResult, JavacOptionsItem,
    JavacProvider, ServerCapabilities,
};
use nova_build_bazel::{BazelWorkspace, BspWorkspace, CommandOutput, CommandRunner};
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
fn bazel_workspace_target_compile_info_prefers_bsp_javac_options() {
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
