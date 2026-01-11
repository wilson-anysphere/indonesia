use crate::{NovaLspError, Result};
use nova_build::BuildManager;
use nova_dap::hot_swap::{BuildSystem, CompileError, CompileOutput, CompiledClass, HotSwapEngine};
use nova_dap::jdwp::{JdwpClient, TcpJdwpClient};
use nova_ide::Project;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugConfigurationsParams {
    /// Workspace root on disk.
    ///
    /// Clients should prefer `projectRoot` (camelCase). `root` is accepted as an
    /// alias for consistency with other Nova extension endpoints.
    #[serde(alias = "root")]
    pub project_root: String,
}

pub fn handle_debug_configurations(params: serde_json::Value) -> Result<serde_json::Value> {
    let params: DebugConfigurationsParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

    let project = Project::load_from_dir(Path::new(&params.project_root))
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;
    let configs = project.discover_debug_configurations();
    serde_json::to_value(configs).map_err(|err| NovaLspError::Internal(err.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotSwapRequestParams {
    #[serde(alias = "root")]
    pub project_root: String,
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub host: Option<String>,
    pub port: u16,
}

pub fn handle_hot_swap(params: serde_json::Value) -> Result<serde_json::Value> {
    let params: HotSwapRequestParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;

    let project_root = PathBuf::from(&params.project_root);
    let project = nova_project::load_project_with_workspace_config(&project_root)
        .map_err(|err| NovaLspError::Internal(err.to_string()))?;

    let mut changed_files = Vec::new();
    for file in params.changed_files {
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
    let host = params.host.as_deref().unwrap_or("127.0.0.1");
    jdwp.connect(host, params.port)
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

impl ProjectHotSwapBuild {
    fn outputs_for_build(
        &self,
        files: &[PathBuf],
        build: nova_build::BuildResult,
    ) -> Vec<CompileOutput> {
        let mut error_map: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for diag in build.diagnostics {
            if diag.severity == nova_core::DiagnosticSeverity::Error {
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

            compiled.push(CompiledClass { class_name, bytecode });
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
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
