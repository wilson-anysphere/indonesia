use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

use nova_jdwp::{JdwpError as NovaJdwpError, TcpJdwpClient};
use nova_jdwp::wire::{JdwpClient as WireJdwpClient, JdwpError as WireJdwpError};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledClass {
    pub class_name: String,
    pub bytecode: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileOutput {
    pub file: PathBuf,
    pub result: Result<CompiledClass, CompileError>,
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
    #[error("jdwp redefine failed: {0}")]
    Other(String),
}

/// Minimal build-system integration required for hot swapping.
pub trait BuildSystem {
    fn compile_files(&mut self, files: &[PathBuf]) -> Vec<CompileOutput>;
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
    fn redefine_class(&mut self, class_name: &str, bytecode: &[u8]) -> impl Future<Output = Result<(), JdwpError>> + Send + '_;
}

impl AsyncJdwpRedefiner for WireJdwpClient {
    fn redefine_class(&mut self, class_name: &str, bytecode: &[u8]) -> impl Future<Output = Result<(), JdwpError>> + Send + '_ {
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
        other => JdwpError::Other(other.to_string()),
    }
}

fn map_wire_jdwp_error(err: WireJdwpError) -> JdwpError {
    match err {
        WireJdwpError::VmError(error_code) if is_schema_change(error_code) => JdwpError::SchemaChange(format!(
            "HotSwap rejected by JVM (JDWP error {error_code})"
        )),
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
                    Ok(compiled) => match self
                        .jdwp
                        .redefine_class(&compiled.class_name, &compiled.bytecode)
                    {
                        Ok(()) => results.push(HotSwapFileResult {
                            file: file.clone(),
                            status: HotSwapStatus::Success,
                            message: None,
                        }),
                        Err(JdwpError::SchemaChange(msg)) => results.push(HotSwapFileResult {
                            file: file.clone(),
                            status: HotSwapStatus::SchemaChange,
                            message: Some(msg),
                        }),
                        Err(err) => results.push(HotSwapFileResult {
                            file: file.clone(),
                            status: HotSwapStatus::RedefinitionError,
                            message: Some(err.to_string()),
                        }),
                    },
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
                    Ok(compiled) => match self
                        .jdwp
                        .redefine_class(&compiled.class_name, &compiled.bytecode)
                        .await
                    {
                        Ok(()) => results.push(HotSwapFileResult {
                            file: file.clone(),
                            status: HotSwapStatus::Success,
                            message: None,
                        }),
                        Err(JdwpError::SchemaChange(msg)) => results.push(HotSwapFileResult {
                            file: file.clone(),
                            status: HotSwapStatus::SchemaChange,
                            message: Some(msg),
                        }),
                        Err(err) => results.push(HotSwapFileResult {
                            file: file.clone(),
                            status: HotSwapStatus::RedefinitionError,
                            message: Some(err.to_string()),
                        }),
                    },
                },
            }
        }

        HotSwapResult { results }
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
                result: Ok(CompiledClass {
                    class_name: "com.example.A".into(),
                    bytecode: vec![0xCA, 0xFE],
                }),
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
                result: Ok(CompiledClass {
                    class_name: "com.example.C".into(),
                    bytecode: vec![0xBE, 0xEF],
                }),
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
                    message: Some("Class structure changed. Restart required.".into())
                },
            ]
        );

        assert_eq!(
            engine.jdwp.calls,
            vec!["com.example.A".to_string(), "com.example.C".to_string()]
        );
    }
}
