use filetime::{set_file_mtime, FileTime};
use nova_apt::{AptBuildExecutor, AptManager, AptRunTarget, NoopProgressReporter};
use nova_build::{
    BuildManager, BuildResult, DefaultCommandRunner, GradleBuildTask, MavenBuildGoal,
};
use nova_config::NovaConfig;
use nova_index::ClassIndex;
use nova_project::{
    BuildSystem, JavaConfig, Module, ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Maven {
        module: Option<PathBuf>,
        goal: MavenBuildGoal,
    },
    #[allow(dead_code)]
    Gradle {
        project_path: Option<String>,
        task: GradleBuildTask,
    },
    #[allow(dead_code)]
    Bazel { target: String },
}

#[derive(Default)]
struct RecordingBuildExecutor {
    calls: Mutex<Vec<Call>>,
}

impl RecordingBuildExecutor {
    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

impl AptBuildExecutor for RecordingBuildExecutor {
    fn build_maven(
        &self,
        _project_root: &Path,
        module_relative: Option<&Path>,
        goal: MavenBuildGoal,
    ) -> nova_build::Result<BuildResult> {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(Call::Maven {
                module: module_relative.map(PathBuf::from),
                goal,
            });
        Ok(BuildResult {
            diagnostics: Vec::new(),
            ..Default::default()
        })
    }

    fn build_gradle(
        &self,
        _project_root: &Path,
        project_path: Option<&str>,
        task: GradleBuildTask,
    ) -> nova_build::Result<BuildResult> {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(Call::Gradle {
                project_path: project_path.map(str::to_string),
                task,
            });
        Ok(BuildResult {
            diagnostics: Vec::new(),
            ..Default::default()
        })
    }

    fn build_bazel(&self, _project_root: &Path, target: &str) -> nova_build::Result<BuildResult> {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(Call::Bazel {
                target: target.to_string(),
            });
        Ok(BuildResult {
            diagnostics: Vec::new(),
            ..Default::default()
        })
    }
}

fn write_java_file(path: &Path, mtime_secs: i64) {
    std::fs::create_dir_all(path.parent().expect("java file has parent")).unwrap();
    std::fs::write(path, "class X {}").unwrap();
    set_file_mtime(path, FileTime::from_unix_time(mtime_secs, 0)).unwrap();
}

fn maven_project_config(
    workspace_root: &Path,
    modules: Vec<Module>,
    roots: Vec<SourceRoot>,
) -> ProjectConfig {
    ProjectConfig {
        workspace_root: workspace_root.to_path_buf(),
        build_system: BuildSystem::Maven,
        java: JavaConfig::default(),
        modules,
        jpms_modules: Vec::new(),
        jpms_workspace: None,
        source_roots: roots,
        module_path: Vec::new(),
        classpath: Vec::new(),
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    }
}

#[test]
fn stale_main_generated_sources_triggers_main_compile_for_that_module_only() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let module_a_root = root.join("module-a");
    let module_b_root = root.join("module-b");

    write_java_file(
        &module_a_root.join("src/main/java/com/example/App.java"),
        200,
    );
    write_java_file(
        &module_a_root.join("target/generated-sources/annotations/com/example/Gen.java"),
        100,
    );

    write_java_file(
        &module_b_root.join("src/main/java/com/example/App.java"),
        100,
    );
    write_java_file(
        &module_b_root.join("target/generated-sources/annotations/com/example/Gen.java"),
        200,
    );

    let project = maven_project_config(
        root,
        vec![
            Module {
                name: "module-a".to_string(),
                root: module_a_root.clone(),
                annotation_processing: Default::default(),
            },
            Module {
                name: "module-b".to_string(),
                root: module_b_root.clone(),
                annotation_processing: Default::default(),
            },
        ],
        vec![
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Source,
                path: module_a_root.join("src/main/java"),
            },
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Generated,
                path: module_a_root.join("target/generated-sources/annotations"),
            },
            SourceRoot {
                kind: SourceRootKind::Test,
                origin: SourceRootOrigin::Generated,
                path: module_a_root.join("target/generated-test-sources/test-annotations"),
            },
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Source,
                path: module_b_root.join("src/main/java"),
            },
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Generated,
                path: module_b_root.join("target/generated-sources/annotations"),
            },
            SourceRoot {
                kind: SourceRootKind::Test,
                origin: SourceRootOrigin::Generated,
                path: module_b_root.join("target/generated-test-sources/test-annotations"),
            },
        ],
    );

    let apt = AptManager::new(project, NovaConfig::default());
    let executor = RecordingBuildExecutor::default();
    let mut progress = NoopProgressReporter;
    apt.run_annotation_processing_for_target(&executor, AptRunTarget::Workspace, &mut progress)
        .unwrap();

    assert_eq!(
        executor.calls(),
        vec![Call::Maven {
            module: Some(PathBuf::from("module-a")),
            goal: MavenBuildGoal::Compile
        }]
    );
}

#[test]
fn stale_test_generated_sources_triggers_test_compile_only() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let module_a_root = root.join("module-a");
    let module_b_root = root.join("module-b");

    write_java_file(
        &module_a_root.join("src/main/java/com/example/App.java"),
        100,
    );
    write_java_file(
        &module_a_root.join("target/generated-sources/annotations/com/example/Gen.java"),
        200,
    );

    write_java_file(
        &module_a_root.join("src/test/java/com/example/Test.java"),
        300,
    );
    write_java_file(
        &module_a_root
            .join("target/generated-test-sources/test-annotations/com/example/GenTest.java"),
        200,
    );

    // Another module in the workspace to ensure we don't invoke builds unnecessarily.
    write_java_file(
        &module_b_root.join("src/main/java/com/example/App.java"),
        100,
    );
    write_java_file(
        &module_b_root.join("target/generated-sources/annotations/com/example/Gen.java"),
        200,
    );

    let project = maven_project_config(
        root,
        vec![
            Module {
                name: "module-a".to_string(),
                root: module_a_root.clone(),
                annotation_processing: Default::default(),
            },
            Module {
                name: "module-b".to_string(),
                root: module_b_root.clone(),
                annotation_processing: Default::default(),
            },
        ],
        vec![
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Source,
                path: module_a_root.join("src/main/java"),
            },
            SourceRoot {
                kind: SourceRootKind::Test,
                origin: SourceRootOrigin::Source,
                path: module_a_root.join("src/test/java"),
            },
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Generated,
                path: module_a_root.join("target/generated-sources/annotations"),
            },
            SourceRoot {
                kind: SourceRootKind::Test,
                origin: SourceRootOrigin::Generated,
                path: module_a_root.join("target/generated-test-sources/test-annotations"),
            },
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Source,
                path: module_b_root.join("src/main/java"),
            },
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Generated,
                path: module_b_root.join("target/generated-sources/annotations"),
            },
            SourceRoot {
                kind: SourceRootKind::Test,
                origin: SourceRootOrigin::Generated,
                path: module_b_root.join("target/generated-test-sources/test-annotations"),
            },
        ],
    );

    let apt = AptManager::new(project, NovaConfig::default());
    let executor = RecordingBuildExecutor::default();
    let mut progress = NoopProgressReporter;
    apt.run_annotation_processing_for_target(&executor, AptRunTarget::Workspace, &mut progress)
        .unwrap();

    assert_eq!(
        executor.calls(),
        vec![Call::Maven {
            module: Some(PathBuf::from("module-a")),
            goal: MavenBuildGoal::TestCompile
        }]
    );
}

#[test]
fn no_stale_generated_roots_does_not_invoke_build() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let module_root = root.join("module-a");
    write_java_file(&module_root.join("src/main/java/com/example/App.java"), 100);
    write_java_file(
        &module_root.join("target/generated-sources/annotations/com/example/Gen.java"),
        200,
    );

    let project = maven_project_config(
        root,
        vec![Module {
            name: "module-a".to_string(),
            root: module_root.clone(),
            annotation_processing: Default::default(),
        }],
        vec![
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Source,
                path: module_root.join("src/main/java"),
            },
            SourceRoot {
                kind: SourceRootKind::Main,
                origin: SourceRootOrigin::Generated,
                path: module_root.join("target/generated-sources/annotations"),
            },
            SourceRoot {
                kind: SourceRootKind::Test,
                origin: SourceRootOrigin::Generated,
                path: module_root.join("target/generated-test-sources/test-annotations"),
            },
        ],
    );

    let apt = AptManager::new(project, NovaConfig::default());
    let executor = RecordingBuildExecutor::default();
    let mut progress = NoopProgressReporter;
    apt.run_annotation_processing_for_target(&executor, AptRunTarget::Workspace, &mut progress)
        .unwrap();

    assert!(executor.calls().is_empty());
}

#[test]
fn run_writes_generated_roots_snapshot_used_by_project_loader() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Create a simple workspace with a generated output directory that is *not* one of the
    // conventional `target/generated-sources/annotations` defaults.
    let src_root = root.join("src/main/java");
    std::fs::create_dir_all(&src_root).unwrap();
    std::fs::create_dir_all(src_root.join("com/example/app")).unwrap();
    std::fs::write(
        src_root.join("com/example/app/App.java"),
        r#"
 package com.example.app;
 import com.example.generated.GeneratedHello;
class App {}
"#,
    )
    .unwrap();

    let custom_generated_root = root.join("custom-generated");
    std::fs::create_dir_all(custom_generated_root.join("com/example/generated")).unwrap();
    std::fs::write(
        custom_generated_root.join("com/example/generated/GeneratedHello.java"),
        r#"
package com.example.generated;
public class GeneratedHello {
  public static String hello() { return "hi"; }
}
"#,
    )
    .unwrap();

    let project = ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![Module {
            name: "root".to_string(),
            root: root.to_path_buf(),
            annotation_processing: nova_project::AnnotationProcessing {
                main: Some(nova_project::AnnotationProcessingConfig {
                    enabled: true,
                    generated_sources_dir: Some(custom_generated_root.clone()),
                    ..Default::default()
                }),
                test: None,
            },
        }],
        jpms_modules: Vec::new(),
        jpms_workspace: None,
        source_roots: vec![SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: src_root,
        }],
        module_path: Vec::new(),
        classpath: Vec::new(),
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    };

    // Before running APT, the Nova project model should not pick up the custom root.
    let mut options = nova_project::LoadOptions::default();
    options.nova_config = NovaConfig::default();
    let loaded_before = nova_project::load_project_with_options(root, &options).unwrap();
    let index_before = ClassIndex::build(&loaded_before.source_roots).unwrap();
    assert!(
        !index_before.contains("com.example.generated.GeneratedHello"),
        "expected generated class to be absent before snapshot is written"
    );

    let mut apt = AptManager::new(project, NovaConfig::default());
    let build = BuildManager::with_runner(
        root.join(".nova").join("build-cache"),
        Arc::new(DefaultCommandRunner {
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        }),
    );
    let mut progress = NoopProgressReporter;
    let result = apt
        .run(&build, AptRunTarget::Workspace, None, &mut progress)
        .unwrap();
    assert!(
        matches!(result.status, nova_apt::AptRunStatus::UpToDate),
        "expected no build tool invocations for simple project"
    );

    // After writing the snapshot, reloading the project should include the custom root and make
    // generated classes discoverable.
    let loaded_after = nova_project::load_project_with_options(root, &options).unwrap();
    let index_after = ClassIndex::build(&loaded_after.source_roots).unwrap();
    assert!(index_after.contains("com.example.generated.GeneratedHello"));
}
