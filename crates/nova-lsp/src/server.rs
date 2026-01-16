use std::path::{Path, PathBuf};

use anyhow::Result;
use nova_dap::hot_swap::{BuildSystem, HotSwapEngine, HotSwapResult, JdwpRedefiner};
use nova_ide::DebugConfiguration;
use nova_workspace::Workspace;

#[derive(Debug)]
pub struct NovaLspServer {
    workspace: Workspace,
}

impl NovaLspServer {
    pub fn from_workspace(workspace: Workspace) -> Self {
        Self { workspace }
    }

    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::from_workspace(Workspace::open(root)?))
    }

    /// Custom LSP method: `nova/debug/configurations`.
    pub fn debug_configurations(&self) -> Vec<DebugConfiguration> {
        self.workspace.debug_configurations()
    }

    /// Custom LSP method: `nova/debug/hotSwap`.
    pub fn hot_swap<B: BuildSystem, J: JdwpRedefiner>(
        &self,
        service: &mut HotSwapService<B, J>,
        changed_files: &[PathBuf],
    ) -> HotSwapResult {
        service.engine.hot_swap(changed_files)
    }
}

/// A convenience wrapper for providing hot-swap support to the LSP layer.
///
/// In a full implementation this would likely be backed by the active debug
/// session, tying compilation to the project's build system and bytecode
/// redefinition to the session's JDWP connection.
#[derive(Debug)]
pub struct HotSwapService<B, J> {
    engine: HotSwapEngine<B, J>,
}

impl<B, J> HotSwapService<B, J> {
    pub fn new(build: B, jdwp: J) -> Self {
        Self {
            engine: HotSwapEngine::new(build, jdwp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_dap::hot_swap::{
        CompileError, CompileOutput, CompiledClass, JdwpError, JdwpRedefiner,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn lsp_debug_configurations_discovers_fixture_project() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let main_dir = root.join("src/main/java/com/example");
        let test_dir = root.join("src/test/java/com/example");
        fs::create_dir_all(&main_dir).unwrap();
        fs::create_dir_all(&test_dir).unwrap();

        fs::write(
            main_dir.join("Main.java"),
            r#"
                package com.example;

                public class Main {
                    public static void main(String[] args) {}
                }
            "#,
        )
        .unwrap();

        fs::write(
            test_dir.join("MainTest.java"),
            r#"
                package com.example;

                import org.junit.jupiter.api.Test;

                public class MainTest {
                    @Test void ok() {}
                }
            "#,
        )
        .unwrap();

        fs::write(
            main_dir.join("Application.java"),
            r#"
                package com.example;

                import org.springframework.boot.autoconfigure.SpringBootApplication;

                @SpringBootApplication
                public class Application {
                    public static void main(String[] args) {}
                }
            "#,
        )
        .unwrap();

        // Sanity check: ensure the underlying project discovery works for simple layouts.
        let direct_configs = nova_ide::Project::load_from_dir(root)
            .unwrap()
            .discover_debug_configurations();
        assert!(
            !direct_configs.is_empty(),
            "direct config discovery returned no configs"
        );

        let server = NovaLspServer::load_from_dir(root).unwrap();
        let configs = server.debug_configurations();

        let names: std::collections::BTreeSet<_> =
            configs.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains("Run Main"), "configs: {names:?}");
        assert!(names.contains("Run Application"), "configs: {names:?}");
        assert!(
            names.contains("Spring Boot: Application"),
            "configs: {names:?}"
        );
        assert!(
            names.contains("Debug Tests: MainTest"),
            "configs: {names:?}"
        );

        let spring = configs
            .iter()
            .find(|c| c.name == "Spring Boot: Application")
            .unwrap();
        assert!(spring.spring_boot);
    }

    #[derive(Debug, Default)]
    struct MockBuild {
        outputs: BTreeMap<PathBuf, CompileOutput>,
    }

    impl BuildSystem for MockBuild {
        fn compile_files(&mut self, files: &[PathBuf]) -> Vec<CompileOutput> {
            files
                .iter()
                .map(|file| {
                    self.outputs
                        .get(file)
                        .cloned()
                        .unwrap_or_else(|| CompileOutput {
                            file: file.clone(),
                            result: Err(CompileError::new("no output configured")),
                        })
                })
                .collect()
        }
    }

    #[derive(Debug, Default)]
    struct MockJdwp {
        results: BTreeMap<String, Result<(), JdwpError>>,
    }

    impl JdwpRedefiner for MockJdwp {
        fn redefine_class(&mut self, class_name: &str, _bytecode: &[u8]) -> Result<(), JdwpError> {
            self.results
                .get(class_name)
                .cloned()
                .unwrap_or_else(|| Ok(()))
        }
    }

    #[test]
    fn lsp_hot_swap_delegates_to_engine() {
        let server = NovaLspServer::from_workspace(Workspace::new_in_memory());

        let file = PathBuf::from("src/main/java/com/example/A.java");
        let mut build = MockBuild::default();
        build.outputs.insert(
            file.clone(),
            CompileOutput {
                file: file.clone(),
                result: Ok(vec![CompiledClass {
                    class_name: "com.example.A".into(),
                    bytecode: vec![1, 2, 3],
                }]),
            },
        );

        let jdwp = MockJdwp::default();
        let mut service = HotSwapService::new(build, jdwp);

        let result = server.hot_swap(&mut service, &[file.clone()]);

        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].file, file);
        assert_eq!(
            result.results[0].status,
            nova_dap::hot_swap::HotSwapStatus::Success
        );
    }
}
