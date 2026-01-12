use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use nova_project::ProjectConfig;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProjectDiscoveryError {
    #[error(transparent)]
    Project(#[from] nova_project::ProjectError),
    #[error("failed to read directory `{path}`: {source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read file `{path}`: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A Java project loaded for (mostly) semantic queries.
#[derive(Debug, Clone)]
pub struct Project {
    root: Option<PathBuf>,
    files: Vec<JavaSourceFile>,
}

#[derive(Debug, Clone)]
struct JavaSourceFile {
    path: PathBuf,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaClassInfo {
    pub path: PathBuf,
    pub package: Option<String>,
    pub simple_name: String,
    pub qualified_name: String,
    pub has_main: bool,
    pub is_test: bool,
    pub is_spring_boot_app: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DebugConfiguration {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: DebugConfigurationKind,
    pub request: DebugConfigurationRequest,
    pub main_class: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vm_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub spring_boot: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum DebugConfigurationRequest {
    Launch,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum DebugConfigurationKind {
    Java,
}

impl Project {
    /// Builds a `Project` from already-loaded sources.
    pub fn new(files: Vec<(PathBuf, String)>) -> Self {
        let files = files
            .into_iter()
            .map(|(path, text)| JavaSourceFile { path, text })
            .collect();
        Self { root: None, files }
    }

    /// Loads a project by recursively collecting `.java` files under `root`.
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self, ProjectDiscoveryError> {
        let root = root.as_ref().to_path_buf();
        let (workspace_root, source_roots) =
            match nova_project::load_project_with_workspace_config(&root) {
                Ok(ProjectConfig {
                    workspace_root,
                    source_roots,
                    ..
                }) => (workspace_root, source_roots),
                Err(_) => {
                    // Keep debug configuration discovery working even for ad-hoc folders or when
                    // project loading fails (for example, due to a broken build file in an
                    // ancestor directory).
                    (root.clone(), Vec::new())
                }
            };

        let mut java_files = Vec::new();
        if source_roots.is_empty() {
            collect_java_files(&workspace_root, &mut java_files)?;
        } else {
            for root in source_roots {
                collect_java_files(&root.path, &mut java_files)?;
            }
        }
        java_files.sort();
        java_files.dedup();

        let mut files = Vec::new();
        for path in java_files {
            let text =
                fs::read_to_string(&path).map_err(|source| ProjectDiscoveryError::ReadFile {
                    path: path.clone(),
                    source,
                })?;
            files.push(JavaSourceFile { path, text });
        }

        Ok(Self {
            root: Some(workspace_root),
            files,
        })
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Performs debug configuration discovery for this project.
    ///
    /// This is intentionally "pure" with respect to Nova's semantic database:
    /// it consumes the sources in memory and uses conservative heuristics to
    /// extract runnable entry points.
    pub fn discover_debug_configurations(&self) -> Vec<DebugConfiguration> {
        let project_name = self
            .root
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|p| p.to_str())
            .map(|s| s.to_string());

        let classes = self.discover_classes();

        let mut configs = Vec::new();
        let mut seen = BTreeSet::<(
            DebugConfigurationKind,
            DebugConfigurationRequest,
            String,
            Vec<String>,
            bool,
        )>::new();

        for class in &classes {
            if class.has_main {
                let config = DebugConfiguration {
                    name: format!("Run {}", class.simple_name),
                    kind: DebugConfigurationKind::Java,
                    request: DebugConfigurationRequest::Launch,
                    main_class: class.qualified_name.clone(),
                    args: Vec::new(),
                    vm_args: Vec::new(),
                    project_name: project_name.clone(),
                    spring_boot: false,
                };
                if seen.insert((
                    config.kind,
                    config.request,
                    config.main_class.clone(),
                    config.args.clone(),
                    config.spring_boot,
                )) {
                    configs.push(config);
                }
            }

            if class.is_test {
                let config = DebugConfiguration {
                    name: format!("Debug Tests: {}", class.simple_name),
                    kind: DebugConfigurationKind::Java,
                    request: DebugConfigurationRequest::Launch,
                    main_class: "org.junit.platform.console.ConsoleLauncher".into(),
                    args: vec!["--select-class".into(), class.qualified_name.clone()],
                    vm_args: Vec::new(),
                    project_name: project_name.clone(),
                    spring_boot: false,
                };
                if seen.insert((
                    config.kind,
                    config.request,
                    config.main_class.clone(),
                    config.args.clone(),
                    config.spring_boot,
                )) {
                    configs.push(config);
                }
            }

            if class.is_spring_boot_app {
                let config = DebugConfiguration {
                    name: format!("Spring Boot: {}", class.simple_name),
                    kind: DebugConfigurationKind::Java,
                    request: DebugConfigurationRequest::Launch,
                    main_class: class.qualified_name.clone(),
                    args: Vec::new(),
                    vm_args: Vec::new(),
                    project_name: project_name.clone(),
                    spring_boot: true,
                };
                if seen.insert((
                    config.kind,
                    config.request,
                    config.main_class.clone(),
                    config.args.clone(),
                    config.spring_boot,
                )) {
                    configs.push(config);
                }
            }
        }

        configs
    }

    pub fn discover_classes(&self) -> Vec<JavaClassInfo> {
        self.files
            .iter()
            .filter_map(|file| discover_class_from_source(&file.path, &file.text))
            .collect()
    }
}

fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), ProjectDiscoveryError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            // Some project models include conventional generated source roots even when the
            // corresponding directories have not been created yet. Treat missing directories as
            // empty rather than failing discovery (used by e.g. debug configuration scanning).
            return Ok(());
        }
        Err(source) => {
            return Err(ProjectDiscoveryError::ReadDir {
                path: dir.to_path_buf(),
                source,
            });
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ProjectDiscoveryError::ReadDir {
                    path: dir.to_path_buf(),
                    source,
                });
            }
        };
        let path = entry.path();
        if path.is_dir() {
            collect_java_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(path);
        }
    }

    Ok(())
}

fn discover_class_from_source(path: &Path, text: &str) -> Option<JavaClassInfo> {
    let package = parse_package(text);
    let simple_name = parse_primary_type(text)?;
    let qualified_name = match &package {
        Some(pkg) => format!("{pkg}.{simple_name}"),
        None => simple_name.clone(),
    };

    let has_main = has_main_method(text);
    let is_test = is_junit_test(text);
    let is_spring_boot_app = is_spring_boot_app(text);

    Some(JavaClassInfo {
        path: path.to_path_buf(),
        package,
        simple_name,
        qualified_name,
        has_main,
        is_test,
        is_spring_boot_app,
    })
}

fn parse_package(text: &str) -> Option<String> {
    static PACKAGE_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?m)^\s*package\s+([A-Za-z0-9_.]+)\s*;").expect("valid regex"));
    PACKAGE_RE
        .captures(text)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

fn parse_primary_type(text: &str) -> Option<String> {
    static TYPE_RE: Lazy<Regex> = Lazy::new(|| {
        // Matches top-level `class Foo`, `interface Foo`, `enum Foo`, `record Foo`.
        Regex::new(r"(?m)^\s*(?:public\s+)?(?:abstract\s+|final\s+)?(?:class|interface|enum|record)\s+([A-Za-z_][A-Za-z0-9_]*)\b")
            .expect("valid regex")
    });
    TYPE_RE
        .captures(text)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

fn has_main_method(text: &str) -> bool {
    static MAIN_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?m)^\s*public\s+static\s+void\s+main\s*\(\s*(?:final\s+)?String\s*(?:\[\s*\]|\.\.\.)\s*[A-Za-z_][A-Za-z0-9_]*\s*\)",
        )
        .expect("valid regex")
    });
    MAIN_RE.is_match(text)
}

fn is_junit_test(text: &str) -> bool {
    static TEST_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"@(?:ParameterizedTest|RepeatedTest|Test)\b").expect("valid regex")
    });
    TEST_RE.is_match(text)
}

fn is_spring_boot_app(text: &str) -> bool {
    static SPRING_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"@SpringBootApplication\b").expect("valid regex"));
    SPRING_RE.is_match(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_main_test_and_spring_configs() {
        let project = Project::new(vec![
            (
                PathBuf::from("src/main/java/com/example/Main.java"),
                r#"
                    package com.example;

                    public class Main {
                        public static void main(String[] args) {
                            System.out.println("hi");
                        }
                    }
                "#
                .to_string(),
            ),
            (
                PathBuf::from("src/test/java/com/example/MainTest.java"),
                r#"
                    package com.example;

                    import org.junit.jupiter.api.Test;

                    public class MainTest {
                        @Test
                        void testIt() {}
                    }
                "#
                .to_string(),
            ),
            (
                PathBuf::from("src/main/java/com/example/Application.java"),
                r#"
                    package com.example;

                    import org.springframework.boot.autoconfigure.SpringBootApplication;

                    @SpringBootApplication
                    public class Application {
                        public static void main(String[] args) {}
                    }
                "#
                .to_string(),
            ),
        ]);

        let configs = project.discover_debug_configurations();

        let names: BTreeSet<_> = configs.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains("Run Main"));
        assert!(names.contains("Debug Tests: MainTest"));
        assert!(names.contains("Spring Boot: Application"));

        let spring = configs
            .iter()
            .find(|c| c.name == "Spring Boot: Application")
            .unwrap();
        assert!(spring.spring_boot);
    }
}
