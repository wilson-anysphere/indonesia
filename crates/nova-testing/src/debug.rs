use crate::runner::{detect_build_tool, gradle_executable, maven_executable};
use crate::schema::{BuildTool, DebugConfiguration, TestDebugRequest, TestDebugResponse};
use crate::test_id::parse_qualified_test_id;
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

    let parsed = parse_qualified_test_id(test_id);
    let module = parsed.module;
    let stripped_test_id = parsed.test;
    let module_rel_path = module.as_deref();

    let (command, args) = match tool {
        BuildTool::Maven => {
            let executable = maven_executable(&project_root);

            let mut args = vec!["-Dmaven.surefire.debug".to_string()];
            if let Some(module_rel_path) = module_rel_path {
                // `"."` is the canonical encoding for the workspace root module. Avoid passing it
                // to `-pl` because Maven does not guarantee that `-pl .` is valid syntax.
                if module_rel_path != "." {
                    args.push("-pl".to_string());
                    args.push(module_rel_path.to_string());
                    args.push("-am".to_string());
                }
            }
            args.push(format!("-Dtest={stripped_test_id}"));
            args.push("test".to_string());

            (executable.to_string(), args)
        }
        BuildTool::Gradle => {
            let executable = gradle_executable(&project_root);
            let task = match module_rel_path {
                Some(".") => ":test".to_string(),
                Some(path) => format!(":{}:test", path.replace('/', ":")),
                None => "test".to_string(),
            };
            let pattern = stripped_test_id.replace('#', ".");
            (
                executable.to_string(),
                vec![
                    task,
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
    match path.canonicalize() {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(err) => {
            tracing::debug!(
                target = "nova.testing",
                path = %path.display(),
                error = %err,
                "failed to canonicalize test project root"
            );
            path.to_path_buf()
        }
    }
}
