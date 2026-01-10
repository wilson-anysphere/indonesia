use crate::runner::detect_build_tool;
use crate::schema::{BuildTool, DebugConfiguration, TestDebugRequest, TestDebugResponse};
use crate::{Result, SCHEMA_VERSION};
use std::path::{Path, PathBuf};

/// Create a command-based debug configuration for running a specific test.
///
/// This is intended to be consumed by `nova-dap` (or editor integrations) which can
/// launch the provided command and attach a debugger to the JVM.
pub fn debug_configuration_for_test(
    project_root: impl AsRef<Path>,
    build_tool: BuildTool,
    test_id: &str,
) -> Result<DebugConfiguration> {
    let project_root = canonicalize_fallback(project_root.as_ref());
    let tool = match build_tool {
        BuildTool::Auto => detect_build_tool(&project_root)?,
        other => other,
    };

    let (command, args) = match tool {
        BuildTool::Maven => {
            let mvnw = project_root.join("mvnw");
            let executable = if mvnw.exists() { "./mvnw" } else { "mvn" };

            (
                executable.to_string(),
                vec![
                    "-Dmaven.surefire.debug".to_string(),
                    format!("-Dtest={test_id}"),
                    "test".to_string(),
                ],
            )
        }
        BuildTool::Gradle => {
            let gradlew = project_root.join("gradlew");
            let executable = if gradlew.exists() {
                "./gradlew"
            } else {
                "gradle"
            };
            let pattern = test_id.replace('#', ".");
            (
                executable.to_string(),
                vec![
                    "test".to_string(),
                    "--tests".to_string(),
                    pattern,
                    "--debug-jvm".to_string(),
                ],
            )
        }
        BuildTool::Auto => unreachable!("auto must be resolved before config construction"),
    };

    Ok(DebugConfiguration {
        schema_version: SCHEMA_VERSION,
        name: format!("Debug {test_id}"),
        cwd: project_root.display().to_string(),
        command,
        args,
        env: Default::default(),
    })
}

/// Construct a debug configuration for a test based on an LSP-style request payload.
pub fn debug_configuration_for_request(req: &TestDebugRequest) -> Result<TestDebugResponse> {
    if req.project_root.trim().is_empty() {
        return Err(crate::NovaTestingError::InvalidRequest(
            "`projectRoot` must not be empty".to_string(),
        ));
    }
    if req.test.trim().is_empty() {
        return Err(crate::NovaTestingError::InvalidRequest(
            "`test` must not be empty".to_string(),
        ));
    }

    let project_root = canonicalize_fallback(Path::new(&req.project_root));
    let tool = match req.build_tool {
        BuildTool::Auto => detect_build_tool(&project_root)?,
        other => other,
    };

    let configuration = debug_configuration_for_test(&project_root, tool, &req.test)?;

    Ok(TestDebugResponse {
        schema_version: SCHEMA_VERSION,
        tool,
        configuration,
    })
}

fn canonicalize_fallback(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
