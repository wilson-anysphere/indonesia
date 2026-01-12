use crate::fake_bsp::{spawn_fake_bsp_server, FakeBspServerConfig};
use nova_build_bazel::bsp::{
    BuildTarget, BuildTargetIdentifier, CompileProvider, InitializeBuildResult, JavacOptionsItem,
    JavacProvider, ServerCapabilities,
};
use nova_build_bazel::test_support::EnvVarGuard;
use nova_build_bazel::{BazelWorkspace, BspWorkspace, CommandOutput, CommandRunner};
use std::{collections::BTreeMap, path::Path};
use tempfile::tempdir;

#[derive(Clone, Debug, Default)]
struct NoopRunner;

impl CommandRunner for NoopRunner {
    fn run(&self, _cwd: &Path, _program: &str, _args: &[&str]) -> anyhow::Result<CommandOutput> {
        anyhow::bail!("unexpected bazel invocation")
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
fn compile_info_for_file_prefers_bsp_inverse_sources_and_javac_options_without_bazel() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _use_bsp_guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some("1"));

    let root = tempdir().unwrap();

    // `compile_info_for_file` requires the file to be contained in some Bazel package.
    std::fs::write(root.path().join("BUILD"), "# test\n").unwrap();
    let src_dir = root.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("Hello.java");
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
        display_name: Some("//:java".to_string()),
    };

    let mut inverse_sources = BTreeMap::new();
    inverse_sources.insert(file_uri.clone(), vec![java_target.id.clone()]);

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
        inverse_sources,
        javac_options: vec![javac],
        compile_status_code: 0,
        diagnostics: Vec::new(),
        send_server_request_before_initialize_response: false,
    };

    let (client, server) = spawn_fake_bsp_server(config).unwrap();
    let bsp_workspace = BspWorkspace::from_client(root.path().to_path_buf(), client).unwrap();

    // The BSP-based path should not invoke `bazel` at all.
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), NoopRunner)
        .unwrap()
        .with_bsp_workspace(bsp_workspace);

    let info1 = workspace
        .compile_info_for_file(&file_path)
        .unwrap()
        .unwrap();
    assert_eq!(info1.classpath, vec!["a.jar".to_string()]);
    assert_eq!(info1.output_dir.as_deref(), Some("out/classes"));
    assert_eq!(info1.release.as_deref(), Some("17"));

    // Second call should hit both the owning-target cache and compile-info cache (no additional BSP
    // requests).
    let info2 = workspace
        .compile_info_for_file(Path::new("src/Hello.java"))
        .unwrap()
        .unwrap();
    assert_eq!(info2, info1);

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

    let javac_request = requests
        .iter()
        .find(|msg| msg.get("method").and_then(|v| v.as_str()) == Some("buildTarget/javacOptions"))
        .expect("missing buildTarget/javacOptions request");
    assert_eq!(
        javac_request.get("params").unwrap(),
        &serde_json::json!({ "targets": [ { "uri": "test://java" } ] })
    );

    assert_eq!(
        requests
            .iter()
            .filter(|msg| {
                msg.get("method").and_then(|v| v.as_str()) == Some("buildTarget/inverseSources")
            })
            .count(),
        1,
        "expected inverseSources to be cached"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|msg| {
                msg.get("method").and_then(|v| v.as_str()) == Some("buildTarget/javacOptions")
            })
            .count(),
        1,
        "expected javacOptions to be cached"
    );

    drop(workspace);
    server.join();
}
