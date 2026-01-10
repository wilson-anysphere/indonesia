use nova_build::{BuildManager, BuildResult};
use nova_config::NovaConfig;
use nova_core::fs;
use nova_project::{BuildSystem, ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Discover generated Java source roots produced by common annotation processor setups.
///
/// This helper exists for components that only know the workspace root on disk
/// (e.g. lightweight navigation/analysis in fixture tests). When a full
/// [`ProjectConfig`] is available, prefer using its generated [`SourceRoot`]s
/// (origin = `Generated`).
pub fn discover_generated_source_roots(project_root: &Path) -> Vec<PathBuf> {
    let candidates = [
        // Maven
        "target/generated-sources/annotations",
        "target/generated-sources/annotationProcessor/java/main",
        "target/generated-sources/annotationProcessor/java/test",
        // Gradle
        "build/generated/sources/annotationProcessor/java/main",
        "build/generated/sources/annotationProcessor/java/test",
        "build/generated/sources/annotationProcessor/java/integrationTest",
    ];

    candidates
        .into_iter()
        .map(|rel| project_root.join(rel))
        .filter(|path| path.is_dir())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeneratedSourcesFreshness {
    Missing,
    Stale,
    Fresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedSourceRootStatus {
    pub root: SourceRoot,
    pub freshness: GeneratedSourcesFreshness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleGeneratedSourcesStatus {
    pub module_name: String,
    pub module_root: std::path::PathBuf,
    pub roots: Vec<GeneratedSourceRootStatus>,
}

#[derive(Clone, Debug)]
pub struct GeneratedSourcesStatus {
    pub enabled: bool,
    pub modules: Vec<ModuleGeneratedSourcesStatus>,
}

pub trait ProgressReporter {
    fn begin(&mut self, _title: &str) {}
    fn report(&mut self, _message: &str) {}
    fn end(&mut self) {}
}

pub struct NoopProgressReporter;

impl ProgressReporter for NoopProgressReporter {}

pub struct AptManager {
    project: ProjectConfig,
    config: NovaConfig,
}

impl AptManager {
    pub fn new(project: ProjectConfig, config: NovaConfig) -> Self {
        Self { project, config }
    }

    pub fn project(&self) -> &ProjectConfig {
        &self.project
    }

    pub fn config(&self) -> &NovaConfig {
        &self.config
    }

    pub fn status(&self) -> io::Result<GeneratedSourcesStatus> {
        let enabled = self.config.generated_sources.enabled;
        let mut modules = Vec::new();

        for module in &self.project.modules {
            let roots = self.generated_roots_for_module(&module.root)?;
            let roots = roots
                .into_iter()
                .map(|root| {
                    let freshness = self.freshness_for_root(&module.root, &root)?;
                    Ok(GeneratedSourceRootStatus { root, freshness })
                })
                .collect::<io::Result<Vec<_>>>()?;

            modules.push(ModuleGeneratedSourcesStatus {
                module_name: module.name.clone(),
                module_root: module.root.clone(),
                roots,
            });
        }

        Ok(GeneratedSourcesStatus { enabled, modules })
    }

    pub fn run_annotation_processing(
        &self,
        build: &BuildManager,
        progress: &mut dyn ProgressReporter,
    ) -> nova_build::Result<BuildResult> {
        progress.begin("Running annotation processing");
        progress.report("Invoking build tool");
        let result = match self.project.build_system {
            BuildSystem::Maven => build.build_maven(&self.project.workspace_root, None)?,
            BuildSystem::Gradle => build.build_gradle(&self.project.workspace_root, None)?,
            BuildSystem::Simple => BuildResult {
                diagnostics: Vec::new(),
            },
        };
        progress.report("Build finished");
        progress.end();
        Ok(result)
    }

    fn freshness_for_root(
        &self,
        module_root: &Path,
        generated_root: &SourceRoot,
    ) -> io::Result<GeneratedSourcesFreshness> {
        if generated_root.origin != SourceRootOrigin::Generated {
            return Ok(GeneratedSourcesFreshness::Fresh);
        }

        if !generated_root.path.is_dir() {
            return Ok(GeneratedSourcesFreshness::Missing);
        }

        let output_mtime = max_java_mtime(&generated_root.path)?;
        let Some(output_mtime) = output_mtime else {
            return Ok(GeneratedSourcesFreshness::Missing);
        };

        let input_mtime = self.max_input_mtime(module_root, generated_root.kind)?;
        let Some(input_mtime) = input_mtime else {
            // No inputs means nothing can be stale.
            return Ok(GeneratedSourcesFreshness::Fresh);
        };

        if input_mtime > output_mtime {
            Ok(GeneratedSourcesFreshness::Stale)
        } else {
            Ok(GeneratedSourcesFreshness::Fresh)
        }
    }

    fn max_input_mtime(&self, module_root: &Path, kind: SourceRootKind) -> io::Result<Option<SystemTime>> {
        let mut max_time = None;

        for root in self
            .project
            .source_roots
            .iter()
            .filter(|root| root.origin == SourceRootOrigin::Source)
            .filter(|root| root.kind == kind)
            .filter(|root| root.path.starts_with(module_root))
        {
            let root_time = max_java_mtime(&root.path)?;
            max_time = Some(match (max_time, root_time) {
                (Some(existing), Some(candidate)) => {
                    if existing >= candidate {
                        existing
                    } else {
                        candidate
                    }
                }
                (Some(existing), None) => existing,
                (None, Some(candidate)) => candidate,
                (None, None) => continue,
            });
        }

        Ok(max_time)
    }

    fn generated_roots_for_module(&self, module_root: &Path) -> io::Result<Vec<SourceRoot>> {
        let mut candidates: Vec<(SourceRootKind, std::path::PathBuf)> = Vec::new();

        if let Some(override_roots) = &self.config.generated_sources.override_roots {
            for root in override_roots {
                let path = if root.is_absolute() {
                    root.clone()
                } else {
                    module_root.join(root)
                };
                candidates.push((SourceRootKind::Main, path));
            }
        } else {
            // Maven defaults.
            candidates.push((
                SourceRootKind::Main,
                module_root.join("target/generated-sources/annotations"),
            ));
            candidates.push((
                SourceRootKind::Test,
                module_root.join("target/generated-test-sources/test-annotations"),
            ));

            // Gradle defaults.
            candidates.push((
                SourceRootKind::Main,
                module_root.join("build/generated/sources/annotationProcessor/java/main"),
            ));
            candidates.push((
                SourceRootKind::Test,
                module_root.join("build/generated/sources/annotationProcessor/java/test"),
            ));

            for root in &self.config.generated_sources.additional_roots {
                let path = if root.is_absolute() {
                    root.clone()
                } else {
                    module_root.join(root)
                };
                candidates.push((SourceRootKind::Main, path));
            }
        }

        let mut seen = HashSet::new();
        let mut roots = Vec::new();

        for (kind, path) in candidates {
            if !path.is_dir() {
                continue;
            }
            if !seen.insert(path.clone()) {
                continue;
            }

            roots.push(SourceRoot {
                kind,
                origin: SourceRootOrigin::Generated,
                path,
            });
        }

        Ok(roots)
    }
}

fn max_java_mtime(root: &Path) -> io::Result<Option<SystemTime>> {
    let files = fs::collect_java_files(root)?;
    fs::max_modified_time(files)
}

#[cfg(test)]
mod tests {
    use nova_config::NovaConfig;
    use nova_core::{Name, PackageName, QualifiedName};
    use nova_hir::{CompilationUnit, ImportDecl};
    use nova_index::ClassIndex;
    use nova_jdk::JdkIndex;
    use nova_project::{load_project_with_options, LoadOptions, SourceRootOrigin};
    use nova_resolve::Resolver;
    use std::path::PathBuf;

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/maven_simple")
    }

    #[test]
    fn resolves_generated_type_when_generated_roots_enabled() {
        let project_root = fixture_root();

        let config = NovaConfig::default();
        let mut options = LoadOptions::default();
        options.nova_config = config.clone();
        let project = load_project_with_options(&project_root, &options).unwrap();

        assert!(project
            .source_roots
            .iter()
            .any(|sr| sr.origin == SourceRootOrigin::Generated));

        let index = ClassIndex::build(&project.source_roots).unwrap();
        assert!(index.contains("com.example.generated.GeneratedHello"));

        let mut unit = CompilationUnit::new(Some(PackageName::from_dotted("com.example.app")));
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("com.example.generated.GeneratedHello"),
            alias: None,
        });

        let jdk = JdkIndex::new();
        let resolver = Resolver::new(&jdk).with_classpath(&index);
        let resolved = resolver.resolve_import(&unit, &Name::from("GeneratedHello"));

        assert!(resolved.is_some());
    }

    #[test]
    fn does_not_resolve_generated_type_when_generated_roots_excluded() {
        let project_root = fixture_root();

        let mut config = NovaConfig::default();
        config.generated_sources.enabled = false;
        let mut options = LoadOptions::default();
        options.nova_config = config;
        let project = load_project_with_options(&project_root, &options).unwrap();

        assert!(!project
            .source_roots
            .iter()
            .any(|sr| sr.origin == SourceRootOrigin::Generated));

        let index = ClassIndex::build(&project.source_roots).unwrap();
        assert!(!index.contains("com.example.generated.GeneratedHello"));

        let mut unit = CompilationUnit::new(Some(PackageName::from_dotted("com.example.app")));
        unit.imports.push(ImportDecl::TypeSingle {
            ty: QualifiedName::from_dotted("com.example.generated.GeneratedHello"),
            alias: None,
        });

        let jdk = JdkIndex::new();
        let resolver = Resolver::new(&jdk).with_classpath(&index);
        let resolved = resolver.resolve_import(&unit, &Name::from("GeneratedHello"));

        assert!(resolved.is_none());
    }
}
