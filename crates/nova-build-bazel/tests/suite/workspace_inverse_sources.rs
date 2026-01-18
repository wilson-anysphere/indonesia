use crate::fake_bsp::{spawn_fake_bsp_server, FakeBspServerConfig};
use nova_build_bazel::bsp::{
    BuildTarget, BuildTargetIdentifier, CompileProvider, InitializeBuildResult, JavacProvider,
    ServerCapabilities,
};
use nova_build_bazel::{BazelWorkspace, BspWorkspace, CommandOutput, CommandRunner};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

use nova_build_bazel::test_support::EnvVarGuard;

#[derive(Clone, Debug, Default)]
struct RecordingRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl RecordingRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(args.iter().map(|s| s.to_string()).collect());

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
fn bazel_workspace_java_owning_targets_for_file_prefers_bsp_inverse_sources() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _use_bsp_guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some("1"));

    let root = tempdir().unwrap();
    // `java_owning_targets_for_file` requires the file to be contained in some Bazel package.
    std::fs::write(root.path().join("BUILD"), "# test\n").unwrap();
    let src_dir = root.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("My File.java");
    std::fs::write(&file_path, "class Hello {}").unwrap();

    let file_uri =
        nova_core::path_to_file_uri(&nova_core::AbsPathBuf::new(file_path.clone()).unwrap())
            .unwrap();

    let java_target = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://java".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: Some("//pkg:java".to_string()),
    };
    let java_target_without_label = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://java2".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["java".to_string()],
        display_name: Some("java2".to_string()),
    };
    let non_java_target = BuildTarget {
        id: BuildTargetIdentifier {
            uri: "test://scala".to_string(),
        },
        tags: Vec::new(),
        language_ids: vec!["scala".to_string()],
        display_name: Some("//pkg:scala".to_string()),
    };

    let mut inverse_sources = BTreeMap::new();
    inverse_sources.insert(
        file_uri.clone(),
        vec![
            java_target.id.clone(),
            java_target_without_label.id.clone(),
            non_java_target.id.clone(),
        ],
    );

    let config = FakeBspServerConfig {
        initialize: InitializeBuildResult {
            display_name: "fake-bsp".to_string(),
            version: "0.1.0".to_string(),
            bsp_version: "2.1.0".to_string(),
            capabilities: server_caps(),
        },
        targets: vec![
            java_target.clone(),
            java_target_without_label,
            non_java_target,
        ],
        inverse_sources,
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

    let targets = workspace.java_owning_targets_for_file(&file_path).unwrap();
    assert_eq!(targets, vec!["//pkg:java".to_string()]);

    // BSP path should avoid invoking `bazel query`.
    assert!(runner.calls().is_empty());

    let requests = server.requests();
    let inverse_request = requests
        .iter()
        .find(|msg| {
            msg.get("method").and_then(|v| v.as_str()) == Some("buildTarget/inverseSources")
        })
        .expect("missing buildTarget/inverseSources request");
    assert_eq!(
        inverse_request.get("params").unwrap(),
        &serde_json::json!({ "textDocument": { "uri": file_uri } })
    );

    drop(workspace);
    server.join();
}
