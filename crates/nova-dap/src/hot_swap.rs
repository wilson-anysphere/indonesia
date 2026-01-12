use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

use nova_jdwp::wire::{JdwpClient as WireJdwpClient, JdwpError as WireJdwpError};
use nova_jdwp::{JdwpError as NovaJdwpError, TcpJdwpClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HotSwapStatus {
    Success,
    CompileError,
    SchemaChange,
    RedefinitionError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HotSwapFileResult {
    pub file: PathBuf,
    pub status: HotSwapStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HotSwapResult {
    pub results: Vec<HotSwapFileResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HotSwapClassResult {
    pub class_name: String,
    pub status: HotSwapStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HotSwapFileResultV2 {
    pub file: PathBuf,
    pub status: HotSwapStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub classes: Vec<HotSwapClassResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HotSwapResultV2 {
    pub results: Vec<HotSwapFileResultV2>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledClass {
    pub class_name: String,
    pub bytecode: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileOutput {
    pub file: PathBuf,
    pub result: Result<Vec<CompiledClass>, CompileError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileOutputMulti {
    pub file: PathBuf,
    pub result: Result<Vec<CompiledClass>, CompileError>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{message}")]
pub struct CompileError {
    message: String,
}

impl CompileError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum JdwpError {
    #[error("unsupported redefinition (schema change): {0}")]
    SchemaChange(String),
    #[error("class not loaded: {0}")]
    NotLoaded(String),
    #[error("jdwp redefine failed: {0}")]
    Other(String),
}

/// Minimal build-system integration required for hot swapping.
pub trait BuildSystem {
    /// Compile each file in `files` and return at most one [`CompileOutput`] per
    /// requested file.
    ///
    /// On success, [`CompileOutput::result`] may contain one or more compiled
    /// `.class` files (including nested / anonymous classes).
    fn compile_files(&mut self, files: &[PathBuf]) -> Vec<CompileOutput>;
}

/// Multi-class build-system integration required for hot swapping.
pub trait BuildSystemMulti {
    fn compile_files_multi(&mut self, files: &[PathBuf]) -> Vec<CompileOutputMulti>;
}

/// Minimal JDWP integration required for hot swapping.
pub trait JdwpRedefiner {
    fn redefine_class(&mut self, class_name: &str, bytecode: &[u8]) -> Result<(), JdwpError>;
}

impl JdwpRedefiner for TcpJdwpClient {
    fn redefine_class(&mut self, class_name: &str, bytecode: &[u8]) -> Result<(), JdwpError> {
        TcpJdwpClient::redefine_class(self, class_name, bytecode).map_err(map_tcp_jdwp_error)
    }
}

/// Minimal JDWP integration required for hot swapping (async).
pub trait AsyncJdwpRedefiner {
    fn redefine_class<'a>(
        &'a mut self,
        class_name: &'a str,
        bytecode: &'a [u8],
    ) -> impl Future<Output = Result<(), JdwpError>> + Send + 'a;
}

impl AsyncJdwpRedefiner for WireJdwpClient {
    fn redefine_class<'a>(
        &'a mut self,
        class_name: &'a str,
        bytecode: &'a [u8],
    ) -> impl Future<Output = Result<(), JdwpError>> + Send + 'a {
        async move {
            WireJdwpClient::redefine_class_by_name(self, class_name, bytecode)
                .await
                .map_err(map_wire_jdwp_error)
        }
    }
}

fn map_tcp_jdwp_error(err: NovaJdwpError) -> JdwpError {
    match err {
        NovaJdwpError::CommandFailed { error_code } if is_schema_change(error_code) => {
            JdwpError::SchemaChange(format!("HotSwap rejected by JVM (JDWP error {error_code})"))
        }
        NovaJdwpError::Protocol(msg) if msg.contains("is not loaded in target JVM") => {
            JdwpError::NotLoaded(msg)
        }
        other => JdwpError::Other(other.to_string()),
    }
}

fn map_wire_jdwp_error(err: WireJdwpError) -> JdwpError {
    match err {
        WireJdwpError::VmError(error_code) if is_schema_change(error_code) => {
            JdwpError::SchemaChange(format!("HotSwap rejected by JVM (JDWP error {error_code})"))
        }
        WireJdwpError::Protocol(msg) if msg.contains("is not loaded in target JVM") => {
            JdwpError::NotLoaded(msg)
        }
        other => JdwpError::Other(other.to_string()),
    }
}

fn is_schema_change(error_code: u16) -> bool {
    // See JDWP error codes (subset). These are commonly returned by
    // `VirtualMachine/RedefineClasses` when the change is not supported.
    matches!(error_code, 21 | 60 | 61 | 62 | 63 | 64 | 66 | 67 | 70 | 71)
}
/// Hot-swap coordinator combining compilation with JDWP `RedefineClasses`.
#[derive(Debug)]
pub struct HotSwapEngine<B, J> {
    build: B,
    jdwp: J,
}

impl<B, J> HotSwapEngine<B, J> {
    pub fn new(build: B, jdwp: J) -> Self {
        Self { build, jdwp }
    }

    pub fn build(&self) -> &B {
        &self.build
    }

    pub fn jdwp(&self) -> &J {
        &self.jdwp
    }
}

impl<B: BuildSystem, J: JdwpRedefiner> HotSwapEngine<B, J> {
    /// Compile + redefine each file in `changed_files`, returning a per-file
    /// result suitable for surfacing in an editor UI.
    pub fn hot_swap(&mut self, changed_files: &[PathBuf]) -> HotSwapResult {
        let compile_outputs = self.build.compile_files(changed_files);
        let outputs_by_file: HashMap<PathBuf, CompileOutput> = compile_outputs
            .into_iter()
            .map(|out| (out.file.clone(), out))
            .collect();

        let mut results = Vec::with_capacity(changed_files.len());

        for file in changed_files {
            let output = outputs_by_file.get(file);
            match output {
                None => results.push(HotSwapFileResult {
                    file: file.clone(),
                    status: HotSwapStatus::CompileError,
                    message: Some("file was not part of compile output".into()),
                }),
                Some(output) => match &output.result {
                    Err(err) => results.push(HotSwapFileResult {
                        file: file.clone(),
                        status: HotSwapStatus::CompileError,
                        message: Some(err.to_string()),
                    }),
                    Ok(classes) => {
                        if classes.is_empty() {
                            results.push(HotSwapFileResult {
                                file: file.clone(),
                                status: HotSwapStatus::CompileError,
                                message: Some("no compiled classes produced".into()),
                            });
                            continue;
                        }

                        let mut schema_change = false;
                        let mut redefinition_error = false;
                        let mut errors = Vec::new();

                        for compiled in classes {
                            match self
                                .jdwp
                                .redefine_class(&compiled.class_name, &compiled.bytecode)
                            {
                                Ok(()) => {}
                                Err(JdwpError::NotLoaded(_)) => {
                                    // Inner/anonymous classes may not be loaded yet.
                                }
                                Err(JdwpError::SchemaChange(msg)) => {
                                    schema_change = true;
                                    errors.push(format!("{}: {}", compiled.class_name, msg));
                                }
                                Err(JdwpError::Other(msg)) => {
                                    redefinition_error = true;
                                    errors.push(format!("{}: {}", compiled.class_name, msg));
                                }
                            }
                        }

                        let status = if schema_change {
                            HotSwapStatus::SchemaChange
                        } else if redefinition_error {
                            HotSwapStatus::RedefinitionError
                        } else {
                            HotSwapStatus::Success
                        };

                        results.push(HotSwapFileResult {
                            file: file.clone(),
                            status,
                            message: (!errors.is_empty()).then(|| errors.join("\n")),
                        });
                    }
                },
            }
        }

        HotSwapResult { results }
    }
}

impl<B: BuildSystem, J: AsyncJdwpRedefiner> HotSwapEngine<B, J> {
    /// Async variant of [`Self::hot_swap`] for JDWP clients that require `await`
    /// (e.g. the wire-based tokio client).
    pub async fn hot_swap_async(&mut self, changed_files: &[PathBuf]) -> HotSwapResult {
        let compile_outputs = self.build.compile_files(changed_files);
        let outputs_by_file: HashMap<PathBuf, CompileOutput> = compile_outputs
            .into_iter()
            .map(|out| (out.file.clone(), out))
            .collect();

        let mut results = Vec::with_capacity(changed_files.len());

        for file in changed_files {
            let output = outputs_by_file.get(file);
            match output {
                None => results.push(HotSwapFileResult {
                    file: file.clone(),
                    status: HotSwapStatus::CompileError,
                    message: Some("file was not part of compile output".into()),
                }),
                Some(output) => match &output.result {
                    Err(err) => results.push(HotSwapFileResult {
                        file: file.clone(),
                        status: HotSwapStatus::CompileError,
                        message: Some(err.to_string()),
                    }),
                    Ok(classes) => {
                        if classes.is_empty() {
                            results.push(HotSwapFileResult {
                                file: file.clone(),
                                status: HotSwapStatus::CompileError,
                                message: Some("no compiled classes produced".into()),
                            });
                            continue;
                        }

                        let mut schema_change = false;
                        let mut redefinition_error = false;
                        let mut errors = Vec::new();

                        for compiled in classes {
                            match self
                                .jdwp
                                .redefine_class(&compiled.class_name, &compiled.bytecode)
                                .await
                            {
                                Ok(()) => {}
                                Err(JdwpError::NotLoaded(_)) => {
                                    // Inner/anonymous classes may not be loaded yet.
                                }
                                Err(JdwpError::SchemaChange(msg)) => {
                                    schema_change = true;
                                    errors.push(format!("{}: {}", compiled.class_name, msg));
                                }
                                Err(JdwpError::Other(msg)) => {
                                    redefinition_error = true;
                                    errors.push(format!("{}: {}", compiled.class_name, msg));
                                }
                            }
                        }

                        let status = if schema_change {
                            HotSwapStatus::SchemaChange
                        } else if redefinition_error {
                            HotSwapStatus::RedefinitionError
                        } else {
                            HotSwapStatus::Success
                        };

                        results.push(HotSwapFileResult {
                            file: file.clone(),
                            status,
                            message: (!errors.is_empty()).then(|| errors.join("\n")),
                        });
                    }
                },
            }
        }

        HotSwapResult { results }
    }
}

fn summarize_multi_file_results(classes: &[HotSwapClassResult]) -> (HotSwapStatus, Option<String>) {
    // Deterministic precedence order:
    // 1) SchemaChange: redefine rejected due to unsupported change (restart required).
    // 2) RedefinitionError: any non-schema redefine error.
    // 3) Success: all classes successfully redefined.
    if classes
        .iter()
        .any(|class| class.status == HotSwapStatus::SchemaChange)
    {
        let message = classes
            .iter()
            .find(|class| class.status == HotSwapStatus::SchemaChange)
            .and_then(|class| class.message.clone());
        return (HotSwapStatus::SchemaChange, message);
    }

    if classes
        .iter()
        .any(|class| class.status == HotSwapStatus::RedefinitionError)
    {
        let message = classes
            .iter()
            .find(|class| class.status == HotSwapStatus::RedefinitionError)
            .and_then(|class| class.message.clone());
        return (HotSwapStatus::RedefinitionError, message);
    }

    (HotSwapStatus::Success, None)
}

impl<B: BuildSystemMulti, J: JdwpRedefiner> HotSwapEngine<B, J> {
    /// Multi-class variant of [`Self::hot_swap`]. A single source file may
    /// compile to multiple `.class` files (e.g. inner/anonymous classes).
    pub fn hot_swap_multi(&mut self, changed_files: &[PathBuf]) -> HotSwapResultV2 {
        let compile_outputs = self.build.compile_files_multi(changed_files);
        let outputs_by_file: HashMap<PathBuf, CompileOutputMulti> = compile_outputs
            .into_iter()
            .map(|out| (out.file.clone(), out))
            .collect();

        let mut results = Vec::with_capacity(changed_files.len());

        for file in changed_files {
            let output = outputs_by_file.get(file);
            match output {
                None => results.push(HotSwapFileResultV2 {
                    file: file.clone(),
                    status: HotSwapStatus::CompileError,
                    message: Some("file was not part of compile output".into()),
                    classes: Vec::new(),
                }),
                Some(output) => match &output.result {
                    Err(err) => results.push(HotSwapFileResultV2 {
                        file: file.clone(),
                        status: HotSwapStatus::CompileError,
                        message: Some(err.to_string()),
                        classes: Vec::new(),
                    }),
                    Ok(compiled_classes) if compiled_classes.is_empty() => {
                        results.push(HotSwapFileResultV2 {
                            file: file.clone(),
                            status: HotSwapStatus::CompileError,
                            message: Some("no classes produced".into()),
                            classes: Vec::new(),
                        })
                    }
                    Ok(compiled_classes) => {
                        let mut class_results = Vec::with_capacity(compiled_classes.len());

                        for compiled in compiled_classes {
                            let class_name = compiled.class_name.clone();
                            match self
                                .jdwp
                                .redefine_class(&compiled.class_name, &compiled.bytecode)
                            {
                                Ok(()) => class_results.push(HotSwapClassResult {
                                    class_name,
                                    status: HotSwapStatus::Success,
                                    message: None,
                                }),
                                Err(JdwpError::NotLoaded(_)) => class_results.push(HotSwapClassResult {
                                    class_name,
                                    status: HotSwapStatus::Success,
                                    message: None,
                                }),
                                Err(JdwpError::SchemaChange(message)) => {
                                    class_results.push(HotSwapClassResult {
                                        class_name,
                                        status: HotSwapStatus::SchemaChange,
                                        message: Some(message),
                                    });
                                }
                                Err(err) => class_results.push(HotSwapClassResult {
                                    class_name,
                                    status: HotSwapStatus::RedefinitionError,
                                    message: Some(err.to_string()),
                                }),
                            }
                        }

                        let (status, message) = summarize_multi_file_results(&class_results);
                        results.push(HotSwapFileResultV2 {
                            file: file.clone(),
                            status,
                            message,
                            classes: class_results,
                        });
                    }
                },
            }
        }

        HotSwapResultV2 { results }
    }
}

impl<B: BuildSystemMulti, J: AsyncJdwpRedefiner> HotSwapEngine<B, J> {
    /// Async variant of [`Self::hot_swap_multi`] for JDWP clients that require
    /// `await` (e.g. the wire-based tokio client).
    pub async fn hot_swap_multi_async(&mut self, changed_files: &[PathBuf]) -> HotSwapResultV2 {
        let compile_outputs = self.build.compile_files_multi(changed_files);
        let outputs_by_file: HashMap<PathBuf, CompileOutputMulti> = compile_outputs
            .into_iter()
            .map(|out| (out.file.clone(), out))
            .collect();

        let mut results = Vec::with_capacity(changed_files.len());

        for file in changed_files {
            let output = outputs_by_file.get(file);
            match output {
                None => results.push(HotSwapFileResultV2 {
                    file: file.clone(),
                    status: HotSwapStatus::CompileError,
                    message: Some("file was not part of compile output".into()),
                    classes: Vec::new(),
                }),
                Some(output) => match &output.result {
                    Err(err) => results.push(HotSwapFileResultV2 {
                        file: file.clone(),
                        status: HotSwapStatus::CompileError,
                        message: Some(err.to_string()),
                        classes: Vec::new(),
                    }),
                    Ok(compiled_classes) if compiled_classes.is_empty() => {
                        results.push(HotSwapFileResultV2 {
                            file: file.clone(),
                            status: HotSwapStatus::CompileError,
                            message: Some("no classes produced".into()),
                            classes: Vec::new(),
                        })
                    }
                    Ok(compiled_classes) => {
                        let mut class_results = Vec::with_capacity(compiled_classes.len());

                        for compiled in compiled_classes {
                            let class_name = compiled.class_name.clone();
                            match self
                                .jdwp
                                .redefine_class(&compiled.class_name, &compiled.bytecode)
                                .await
                            {
                                Ok(()) => class_results.push(HotSwapClassResult {
                                    class_name,
                                    status: HotSwapStatus::Success,
                                    message: None,
                                }),
                                Err(JdwpError::NotLoaded(_)) => class_results.push(HotSwapClassResult {
                                    class_name,
                                    status: HotSwapStatus::Success,
                                    message: None,
                                }),
                                Err(JdwpError::SchemaChange(message)) => {
                                    class_results.push(HotSwapClassResult {
                                        class_name,
                                        status: HotSwapStatus::SchemaChange,
                                        message: Some(message),
                                    });
                                }
                                Err(err) => class_results.push(HotSwapClassResult {
                                    class_name,
                                    status: HotSwapStatus::RedefinitionError,
                                    message: Some(err.to_string()),
                                }),
                            }
                        }

                        let (status, message) = summarize_multi_file_results(&class_results);
                        results.push(HotSwapFileResultV2 {
                            file: file.clone(),
                            status,
                            message,
                            classes: class_results,
                        });
                    }
                },
            }
        }

        HotSwapResultV2 { results }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Debug, Default)]
    struct MockBuild {
        outputs: BTreeMap<PathBuf, CompileOutput>,
        calls: Vec<Vec<PathBuf>>,
    }

    impl BuildSystem for MockBuild {
        fn compile_files(&mut self, files: &[PathBuf]) -> Vec<CompileOutput> {
            self.calls.push(files.to_vec());
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
    struct MockBuildMulti {
        outputs: BTreeMap<PathBuf, CompileOutputMulti>,
        calls: Vec<Vec<PathBuf>>,
    }

    impl BuildSystemMulti for MockBuildMulti {
        fn compile_files_multi(&mut self, files: &[PathBuf]) -> Vec<CompileOutputMulti> {
            self.calls.push(files.to_vec());
            files
                .iter()
                .map(|file| {
                    self.outputs
                        .get(file)
                        .cloned()
                        .unwrap_or_else(|| CompileOutputMulti {
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
        calls: Vec<String>,
    }

    impl JdwpRedefiner for MockJdwp {
        fn redefine_class(&mut self, class_name: &str, _bytecode: &[u8]) -> Result<(), JdwpError> {
            self.calls.push(class_name.to_string());
            self.results
                .get(class_name)
                .cloned()
                .unwrap_or_else(|| Ok(()))
        }
    }

    #[test]
    fn hot_swap_reports_compile_errors_and_schema_changes() {
        let a = PathBuf::from("src/main/java/com/example/A.java");
        let b = PathBuf::from("src/main/java/com/example/B.java");
        let c = PathBuf::from("src/main/java/com/example/C.java");

        let mut build = MockBuild::default();
        build.outputs.insert(
            a.clone(),
            CompileOutput {
                file: a.clone(),
                result: Ok(vec![CompiledClass {
                    class_name: "com.example.A".into(),
                    bytecode: vec![0xCA, 0xFE],
                }]),
            },
        );
        build.outputs.insert(
            b.clone(),
            CompileOutput {
                file: b.clone(),
                result: Err(CompileError::new("B.java:1: error: nope")),
            },
        );
        build.outputs.insert(
            c.clone(),
            CompileOutput {
                file: c.clone(),
                result: Ok(vec![CompiledClass {
                    class_name: "com.example.C".into(),
                    bytecode: vec![0xBE, 0xEF],
                }]),
            },
        );

        let mut jdwp = MockJdwp::default();
        jdwp.results.insert(
            "com.example.C".into(),
            Err(JdwpError::SchemaChange(
                "Class structure changed. Restart required.".into(),
            )),
        );

        let mut engine = HotSwapEngine::new(build, jdwp);
        let result = engine.hot_swap(&[a.clone(), b.clone(), c.clone()]);

        assert_eq!(
            result.results,
            vec![
                HotSwapFileResult {
                    file: a,
                    status: HotSwapStatus::Success,
                    message: None
                },
                HotSwapFileResult {
                    file: b,
                    status: HotSwapStatus::CompileError,
                    message: Some("B.java:1: error: nope".into())
                },
                HotSwapFileResult {
                    file: c,
                    status: HotSwapStatus::SchemaChange,
                    message: Some(
                        "com.example.C: Class structure changed. Restart required.".into()
                    )
                },
            ]
        );

        assert_eq!(
            engine.jdwp.calls,
            vec!["com.example.A".to_string(), "com.example.C".to_string()]
        );
    }

    #[test]
    fn hot_swap_redefines_all_classes_for_file() {
        let file = PathBuf::from("src/main/java/com/example/A.java");

        let mut build = MockBuild::default();
        build.outputs.insert(
            file.clone(),
            CompileOutput {
                file: file.clone(),
                result: Ok(vec![
                    CompiledClass {
                        class_name: "com.example.A".into(),
                        bytecode: vec![0xCA, 0xFE],
                    },
                    CompiledClass {
                        class_name: "com.example.A$Inner".into(),
                        bytecode: vec![0xBE, 0xEF],
                    },
                ]),
            },
        );

        let jdwp = MockJdwp::default();
        let mut engine = HotSwapEngine::new(build, jdwp);
        let result = engine.hot_swap(&[file.clone()]);

        assert_eq!(
            result.results,
            vec![HotSwapFileResult {
                file,
                status: HotSwapStatus::Success,
                message: None,
            }]
        );

        assert_eq!(
            engine.jdwp.calls,
            vec!["com.example.A".to_string(), "com.example.A$Inner".to_string()]
        );
    }

    #[test]
    fn hot_swap_aggregates_schema_change_across_classes() {
        let file = PathBuf::from("src/main/java/com/example/A.java");

        let mut build = MockBuild::default();
        build.outputs.insert(
            file.clone(),
            CompileOutput {
                file: file.clone(),
                result: Ok(vec![
                    CompiledClass {
                        class_name: "com.example.A".into(),
                        bytecode: vec![0xCA, 0xFE],
                    },
                    CompiledClass {
                        class_name: "com.example.A$Inner".into(),
                        bytecode: vec![0xBE, 0xEF],
                    },
                ]),
            },
        );

        let mut jdwp = MockJdwp::default();
        jdwp.results.insert(
            "com.example.A$Inner".into(),
            Err(JdwpError::SchemaChange("schema changed".into())),
        );

        let mut engine = HotSwapEngine::new(build, jdwp);
        let result = engine.hot_swap(&[file.clone()]);

        assert_eq!(
            result.results,
            vec![HotSwapFileResult {
                file,
                status: HotSwapStatus::SchemaChange,
                message: Some("com.example.A$Inner: schema changed".into()),
            }]
        );
    }

    #[test]
    fn hot_swap_treats_empty_class_list_as_compile_error() {
        let file = PathBuf::from("src/main/java/com/example/A.java");

        let mut build = MockBuild::default();
        build.outputs.insert(
            file.clone(),
            CompileOutput {
                file: file.clone(),
                result: Ok(Vec::new()),
            },
        );

        let jdwp = MockJdwp::default();
        let mut engine = HotSwapEngine::new(build, jdwp);
        let result = engine.hot_swap(&[file.clone()]);

        assert_eq!(
            result.results,
            vec![HotSwapFileResult {
                file,
                status: HotSwapStatus::CompileError,
                message: Some("no compiled classes produced".into()),
            }]
        );
        assert!(engine.jdwp.calls.is_empty());
    }

    #[test]
    fn hot_swap_multi_redefines_multiple_classes_per_file() {
        let a = PathBuf::from("src/main/java/com/example/A.java");

        let mut build = MockBuildMulti::default();
        build.outputs.insert(
            a.clone(),
            CompileOutputMulti {
                file: a.clone(),
                result: Ok(vec![
                    CompiledClass {
                        class_name: "com.example.A".into(),
                        bytecode: vec![0xCA, 0xFE],
                    },
                    CompiledClass {
                        class_name: "com.example.A$Inner".into(),
                        bytecode: vec![0xBE, 0xEF],
                    },
                ]),
            },
        );

        let jdwp = MockJdwp::default();
        let mut engine = HotSwapEngine::new(build, jdwp);
        let result = engine.hot_swap_multi(&[a.clone()]);

        assert_eq!(
            result,
            HotSwapResultV2 {
                results: vec![HotSwapFileResultV2 {
                    file: a,
                    status: HotSwapStatus::Success,
                    message: None,
                    classes: vec![
                        HotSwapClassResult {
                            class_name: "com.example.A".into(),
                            status: HotSwapStatus::Success,
                            message: None,
                        },
                        HotSwapClassResult {
                            class_name: "com.example.A$Inner".into(),
                            status: HotSwapStatus::Success,
                            message: None,
                        },
                    ],
                }]
            }
        );

        assert_eq!(
            engine.jdwp.calls,
            vec![
                "com.example.A".to_string(),
                "com.example.A$Inner".to_string()
            ]
        );
    }

    #[test]
    fn hot_swap_multi_reports_schema_changes_per_class() {
        let a = PathBuf::from("src/main/java/com/example/A.java");

        let mut build = MockBuildMulti::default();
        build.outputs.insert(
            a.clone(),
            CompileOutputMulti {
                file: a.clone(),
                result: Ok(vec![
                    CompiledClass {
                        class_name: "com.example.A".into(),
                        bytecode: vec![0xCA, 0xFE],
                    },
                    CompiledClass {
                        class_name: "com.example.A$Inner".into(),
                        bytecode: vec![0xBE, 0xEF],
                    },
                ]),
            },
        );

        let mut jdwp = MockJdwp::default();
        jdwp.results.insert(
            "com.example.A$Inner".into(),
            Err(JdwpError::SchemaChange(
                "Class structure changed. Restart required.".into(),
            )),
        );

        let mut engine = HotSwapEngine::new(build, jdwp);
        let result = engine.hot_swap_multi(&[a.clone()]);

        assert_eq!(
            result.results,
            vec![HotSwapFileResultV2 {
                file: a,
                status: HotSwapStatus::SchemaChange,
                message: Some("Class structure changed. Restart required.".into()),
                classes: vec![
                    HotSwapClassResult {
                        class_name: "com.example.A".into(),
                        status: HotSwapStatus::Success,
                        message: None,
                    },
                    HotSwapClassResult {
                        class_name: "com.example.A$Inner".into(),
                        status: HotSwapStatus::SchemaChange,
                        message: Some("Class structure changed. Restart required.".into()),
                    },
                ],
            }]
        );

        assert_eq!(
            engine.jdwp.calls,
            vec![
                "com.example.A".to_string(),
                "com.example.A$Inner".to_string()
            ]
        );
    }

    #[test]
    fn hot_swap_multi_reports_redefinition_errors_per_class() {
        let a = PathBuf::from("src/main/java/com/example/A.java");

        let mut build = MockBuildMulti::default();
        build.outputs.insert(
            a.clone(),
            CompileOutputMulti {
                file: a.clone(),
                result: Ok(vec![
                    CompiledClass {
                        class_name: "com.example.A".into(),
                        bytecode: vec![0xCA, 0xFE],
                    },
                    CompiledClass {
                        class_name: "com.example.A$Inner".into(),
                        bytecode: vec![0xBE, 0xEF],
                    },
                ]),
            },
        );

        let mut jdwp = MockJdwp::default();
        jdwp.results.insert(
            "com.example.A$Inner".into(),
            Err(JdwpError::Other("unexpected error".into())),
        );

        let mut engine = HotSwapEngine::new(build, jdwp);
        let result = engine.hot_swap_multi(&[a.clone()]);

        assert_eq!(
            result.results,
            vec![HotSwapFileResultV2 {
                file: a,
                status: HotSwapStatus::RedefinitionError,
                message: Some("jdwp redefine failed: unexpected error".into()),
                classes: vec![
                    HotSwapClassResult {
                        class_name: "com.example.A".into(),
                        status: HotSwapStatus::Success,
                        message: None,
                    },
                    HotSwapClassResult {
                        class_name: "com.example.A$Inner".into(),
                        status: HotSwapStatus::RedefinitionError,
                        message: Some("jdwp redefine failed: unexpected error".into()),
                    },
                ],
            }]
        );

        assert_eq!(
            engine.jdwp.calls,
            vec![
                "com.example.A".to_string(),
                "com.example.A$Inner".to_string()
            ]
        );
    }

    #[test]
    fn hot_swap_multi_treats_empty_class_list_as_compile_error() {
        let a = PathBuf::from("src/main/java/com/example/A.java");

        let mut build = MockBuildMulti::default();
        build.outputs.insert(
            a.clone(),
            CompileOutputMulti {
                file: a.clone(),
                result: Ok(Vec::new()),
            },
        );

        let jdwp = MockJdwp::default();
        let mut engine = HotSwapEngine::new(build, jdwp);
        let result = engine.hot_swap_multi(&[a.clone()]);

        assert_eq!(
            result.results,
            vec![HotSwapFileResultV2 {
                file: a,
                status: HotSwapStatus::CompileError,
                message: Some("no classes produced".into()),
                classes: Vec::new(),
            }]
        );

        assert!(engine.jdwp.calls.is_empty());
    }
}
