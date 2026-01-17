use crate::{NovaLspError, Result};
use nova_build::BuildManager;
use nova_dap::hot_swap::{BuildSystem, CompileError, CompileOutput, CompiledClass, HotSwapEngine};
use nova_dap::jdwp::{JdwpClient, TcpJdwpClient};
use nova_ide::Project;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use super::build::BuildStatusGuard;

pub fn handle_debug_configurations(params: serde_json::Value) -> Result<serde_json::Value> {
    let project_root = super::decode_project_root(params)?;
    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let project = Project::load_from_dir(Path::new(&project_root))
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;
    let configs = project.discover_debug_configurations();
    serde_json::to_value(configs).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_hot_swap(params: serde_json::Value) -> Result<serde_json::Value> {
    let obj = params
        .as_object()
        .ok_or_else(|| NovaLspError::InvalidParams("params must be an object".to_string()))?;
    let project_root = super::decode_project_root(Value::Object(obj.clone()))?;
    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let port_value = obj
        .get("port")
        .ok_or_else(|| NovaLspError::InvalidParams("missing required `port`".to_string()))?;
    let port_u64 = port_value.as_u64().ok_or_else(|| {
        NovaLspError::InvalidParams("`port` must be a non-negative integer".to_string())
    })?;
    let port = u16::try_from(port_u64).map_err(|_| {
        NovaLspError::InvalidParams(format!("`port` out of range (0-65535): {port_u64}"))
    })?;
    let host = obj.get("host").and_then(Value::as_str);
    let changed_files_raw = obj
        .get("changedFiles")
        .or_else(|| obj.get("changed_files"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            NovaLspError::InvalidParams("missing required `changedFiles`".to_string())
        })?;

    let project_root = PathBuf::from(&project_root);
    let project = nova_project::load_project_with_workspace_config(&project_root)
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;

    let mut changed_files: Vec<PathBuf> = Vec::new();
    for file in changed_files_raw {
        let file = file.as_str().ok_or_else(|| {
            NovaLspError::InvalidParams("`changedFiles` must be an array of strings".to_string())
        })?;
        let path = PathBuf::from(file);
        changed_files.push(if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        });
    }

    let build_manager = super::build_manager_for_root(&project_root, Duration::from_secs(120));
    let build = ProjectHotSwapBuild {
        project_root: project_root.clone(),
        project,
        build: build_manager,
    };

    let mut jdwp = TcpJdwpClient::new();
    let host = host.unwrap_or("127.0.0.1");
    jdwp.connect(host, port)
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;

    let mut engine = HotSwapEngine::new(build, jdwp);
    let result = engine.hot_swap(&changed_files);
    serde_json::to_value(result).map_err(|err| NovaLspError::Internal(err.to_string()))
}

struct ProjectHotSwapBuild {
    project_root: PathBuf,
    project: nova_project::ProjectConfig,
    build: BuildManager,
}

impl BuildSystem for ProjectHotSwapBuild {
    fn compile_files(&mut self, files: &[PathBuf]) -> Vec<CompileOutput> {
        let mut status_guard = BuildStatusGuard::new(&self.project_root);
        let build_result = match self.project.build_system {
            nova_project::BuildSystem::Maven => self.build.build_maven(&self.project_root, None),
            nova_project::BuildSystem::Gradle => self.build.build_gradle(&self.project_root, None),
            nova_project::BuildSystem::Bazel => Err(nova_build::BuildError::Unsupported(
                "bazel hot-swap is not supported".into(),
            )),
            nova_project::BuildSystem::Simple => Err(nova_build::BuildError::Unsupported(
                "simple project hot-swap is not supported".into(),
            )),
        };

        match &build_result {
            Ok(result) => {
                let has_errors = result
                    .diagnostics
                    .iter()
                    .any(|diag| diag.severity == nova_core::BuildDiagnosticSeverity::Error);
                let exit_code = match result.exit_code {
                    Some(exit_code) => exit_code,
                    None => {
                        tracing::debug!(
                            target = "nova.lsp",
                            project_root = %self.project_root.display(),
                            "hot-swap build result missing exit code; defaulting to 0"
                        );
                        0
                    }
                };
                let failed = exit_code != 0 || has_errors;
                if failed {
                    let first_error = result
                        .diagnostics
                        .iter()
                        .find(|diag| diag.severity == nova_core::BuildDiagnosticSeverity::Error)
                        .map(|diag| diag.message.clone());
                    let message = first_error.or_else(|| {
                        if exit_code != 0 {
                            Some(format!("hot-swap build failed with exit code {exit_code}"))
                        } else {
                            Some("hot-swap build failed".to_string())
                        }
                    });
                    status_guard.mark_failure(message);
                } else {
                    status_guard.mark_success();
                }
            }
            Err(err) => status_guard.mark_failure(Some(err.to_string())),
        }

        match build_result {
            Ok(result) => self.outputs_for_build(files, result),
            Err(err) => files
                .iter()
                .map(|file| CompileOutput {
                    file: file.clone(),
                    result: Err(CompileError::new(err.to_string())),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[derive(Debug)]
    struct BlockingErrorRunner {
        started_tx: Mutex<Option<mpsc::Sender<()>>>,
        release_rx: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl nova_build::CommandRunner for BlockingErrorRunner {
        fn run(
            &self,
            _cwd: &Path,
            _program: &Path,
            _args: &[String],
        ) -> std::io::Result<nova_build::CommandOutput> {
            if let Some(tx) =
                crate::poison::lock(&self.started_tx, "BlockingErrorRunner::run/started_tx").take()
            {
                let _ = tx.send(());
            }

            if let Some(rx) =
                crate::poison::lock(&self.release_rx, "BlockingErrorRunner::run/release_rx").take()
            {
                let _ = rx.recv_timeout(Duration::from_secs(2));
            }

            Err(std::io::Error::new(std::io::ErrorKind::Other, "boom"))
        }
    }

    #[test]
    fn hot_swap_build_marks_build_status_building_then_failed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("maven-project");
        std::fs::create_dir_all(&root).unwrap();

        std::fs::write(
            root.join("pom.xml"),
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
        )
        .unwrap();

        // Ensure `nova-build` prefers wrapper scripts over requiring a system `mvn`.
        std::fs::write(root.join("mvnw"), "").unwrap();
        std::fs::write(root.join("mvnw.cmd"), "").unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let runner = BlockingErrorRunner {
            started_tx: Mutex::new(Some(started_tx)),
            release_rx: Mutex::new(Some(release_rx)),
        };

        let cache_dir = root.join(".nova").join("build-cache");
        let build = BuildManager::with_runner(cache_dir, Arc::new(runner));

        let project = nova_project::ProjectConfig {
            workspace_root: root.clone(),
            build_system: nova_project::BuildSystem::Maven,
            java: nova_project::JavaConfig::default(),
            modules: Vec::new(),
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        };

        let mut build_system = ProjectHotSwapBuild {
            project_root: root.clone(),
            project,
            build,
        };

        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            build_system.compile_files(&[root_for_thread.join("Foo.java")]);
        });

        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected build tool invocation to start");

        let params = serde_json::Value::Object({
            let mut obj = serde_json::Map::new();
            obj.insert(
                "projectRoot".to_string(),
                serde_json::Value::String(root.to_string_lossy().to_string()),
            );
            obj
        });
        let status = super::super::build::handle_build_status(params).unwrap();
        assert_eq!(
            status.get("status").and_then(|v| v.as_str()),
            Some("building")
        );

        // Allow the runner to return an error so the build finishes.
        let _ = release_tx.send(());
        handle.join().unwrap();

        let params = serde_json::Value::Object({
            let mut obj = serde_json::Map::new();
            obj.insert(
                "projectRoot".to_string(),
                serde_json::Value::String(root.to_string_lossy().to_string()),
            );
            obj
        });
        let status = super::super::build::handle_build_status(params).unwrap();
        assert_eq!(
            status.get("status").and_then(|v| v.as_str()),
            Some("failed")
        );
        let last_error = status
            .get("lastError")
            .and_then(|v| v.as_str())
            .expect("expected lastError to be a string");
        assert!(
            last_error.contains("boom"),
            "expected lastError to include the runner error: {status:?}"
        );
    }
}

impl ProjectHotSwapBuild {
    fn outputs_for_build(
        &self,
        files: &[PathBuf],
        build: nova_build::BuildResult,
    ) -> Vec<CompileOutput> {
        let mut error_map: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for diag in build.diagnostics {
            if diag.severity == nova_core::BuildDiagnosticSeverity::Error {
                let key = canonicalize_fallback(&diag.file);
                error_map.entry(key).or_default().push(diag.message);
            }
        }

        files
            .iter()
            .map(|file| {
                let canonical = canonicalize_fallback(file);
                if let Some(msgs) = error_map.get(&canonical) {
                    return CompileOutput {
                        file: file.clone(),
                        result: Err(CompileError::new(msgs.join("\n"))),
                    };
                }

                match self.compiled_classes_for_source(file) {
                    Ok(classes) => CompileOutput {
                        file: file.clone(),
                        result: Ok(classes),
                    },
                    Err(msg) => CompileOutput {
                        file: file.clone(),
                        result: Err(CompileError::new(msg)),
                    },
                }
            })
            .collect()
    }

    fn compiled_classes_for_source(
        &self,
        source_file: &Path,
    ) -> std::result::Result<Vec<CompiledClass>, String> {
        let output_dir = self
            .output_dir_for_source(source_file)
            .ok_or_else(|| "unable to determine output directory for source file".to_string())?;

        let source_root = self
            .source_root_for_file(source_file)
            .ok_or_else(|| "unable to determine source root for file".to_string())?;

        let rel = source_file
            .strip_prefix(&source_root.path)
            .map_err(|_| "source file is not under its detected source root".to_string())?;
        let mut rel_class = rel.to_path_buf();
        rel_class.set_extension("class");

        let class_file = output_dir.join(&rel_class);
        let class_dir = class_file
            .parent()
            .ok_or_else(|| "unable to determine class output directory".to_string())?;

        let stem = source_file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| "invalid source file name".to_string())?
            .to_string();

        let package = rel
            .parent()
            .and_then(|p| {
                let mut pkg = String::new();
                for (idx, seg) in p.iter().enumerate() {
                    let seg = seg.to_str()?;
                    if idx > 0 {
                        pkg.push('.');
                    }
                    pkg.push_str(seg);
                }
                Some(pkg)
            })
            .filter(|pkg| !pkg.is_empty());

        let mut compiled = Vec::<CompiledClass>::new();
        for entry in std::fs::read_dir(class_dir)
            .map_err(|err| format!("failed to read class output dir {class_dir:?}: {err}"))?
        {
            let entry = entry.map_err(|err| err.to_string())?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("class") {
                continue;
            }

            let file_stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| format!("invalid class file name: {path:?}"))?;
            if file_stem != stem && !file_stem.starts_with(&format!("{stem}$")) {
                continue;
            }

            let bytecode =
                std::fs::read(&path).map_err(|err| format!("failed to read {path:?}: {err}"))?;
            let class_name = match package.as_deref() {
                Some(pkg) => format!("{pkg}.{file_stem}"),
                None => file_stem.to_string(),
            };

            compiled.push(CompiledClass {
                class_name,
                bytecode,
            });
        }

        if compiled.is_empty() {
            return Err(format!(
                "no compiled class files found under {class_dir:?} for {source_file:?}"
            ));
        }

        compiled.sort_by(|a, b| a.class_name.cmp(&b.class_name));
        Ok(compiled)
    }

    fn source_root_for_file(&self, file: &Path) -> Option<&nova_project::SourceRoot> {
        self.project
            .source_roots
            .iter()
            .filter(|root| file.starts_with(&root.path))
            .max_by_key(|root| root.path.as_os_str().len())
    }

    fn output_dir_for_source(&self, file: &Path) -> Option<PathBuf> {
        let source_root = self.source_root_for_file(file)?;
        let kind = match source_root.kind {
            nova_project::SourceRootKind::Main => nova_project::OutputDirKind::Main,
            nova_project::SourceRootKind::Test => nova_project::OutputDirKind::Test,
        };

        let module_root = self
            .project
            .modules
            .iter()
            .filter(|m| file.starts_with(&m.root))
            .max_by_key(|m| m.root.as_os_str().len())
            .map(|m| m.root.clone())
            .unwrap_or_else(|| self.project.workspace_root.clone());

        self.project
            .output_dirs
            .iter()
            .find(|out| out.kind == kind && out.path.starts_with(&module_root))
            .map(|out| out.path.clone())
    }
}

fn canonicalize_fallback(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                path = %path.display(),
                err = %err,
                "failed to canonicalize path; using raw path"
            );
            path.to_path_buf()
        }
    }
}
