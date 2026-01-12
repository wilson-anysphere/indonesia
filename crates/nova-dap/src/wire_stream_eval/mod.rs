//! Compile+inject stream-evaluation helpers for the wire-level debugger.
//!
//! This module is the canonical home for Nova's "stream eval" pipeline (generate helper
//! source → compile → inject via JDWP `DefineClass` → invoke helper methods). Keeping the
//! implementation centralized avoids drift between multiple ad-hoc evaluator
//! implementations.

pub mod bindings;
pub mod java_gen;
pub mod java_types;
pub mod javac_config;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use nova_jdwp::wire::inspect::{self, Inspector};
use nova_jdwp::wire::types::{
    FrameId, Location, MethodId, ReferenceTypeId, INVOKE_SINGLE_THREADED,
};
use nova_jdwp::wire::{JdwpClient, JdwpError, JdwpValue, ObjectId, ThreadId};
use nova_scheduler::CancellationToken;
use nova_stream_debug::StreamSample;
use thiserror::Error;

use crate::javac::{apply_stream_eval_defaults, compile_java_for_hot_swap, HotSwapJavacConfig};
use crate::wire_stream_debug::TemporaryObjectPin;

static STREAM_EVAL_CLASS_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum StreamEvalError {
    #[error(transparent)]
    Jdwp(#[from] JdwpError),
    #[error(transparent)]
    Compile(#[from] crate::hot_swap::CompileError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("javac did not produce expected helper class `{0}`")]
    MissingHelperClass(String),
    #[error("injected class did not expose required method `{0}`")]
    MissingHelperMethod(String),
    #[error("invalid stage index {stage} (have {len})")]
    InvalidStage { stage: usize, len: usize },
    #[error("terminal method was not generated")]
    MissingTerminalMethod,
    #[error("helper invocation ({method}) threw {thrown}")]
    InvocationException { method: String, thrown: String },
    #[error("expected object value, got {0:?}")]
    ExpectedObject(JdwpValue),
}

/// A compiled+defined stream-eval helper class bound to a particular paused frame.
#[derive(Debug, Clone)]
pub struct StreamEvalHelper {
    pub class_name: String,
    pub class_id: ReferenceTypeId,
    pub thread: ThreadId,
    pub args: Vec<JdwpValue>,
    pub stage_method_ids: Vec<MethodId>,
    pub terminal_method_id: Option<MethodId>,
}

impl StreamEvalHelper {
    pub async fn invoke_stage(
        &self,
        jdwp: &JdwpClient,
        stage: usize,
    ) -> Result<JdwpValue, StreamEvalError> {
        let Some(method_id) = self.stage_method_ids.get(stage).copied() else {
            return Err(StreamEvalError::InvalidStage {
                stage,
                len: self.stage_method_ids.len(),
            });
        };
        invoke_helper_method(
            jdwp,
            self.class_id,
            self.thread,
            method_id,
            &self.args,
            &format!("stage{stage}"),
        )
        .await
    }

    pub async fn invoke_terminal(&self, jdwp: &JdwpClient) -> Result<JdwpValue, StreamEvalError> {
        let Some(method_id) = self.terminal_method_id else {
            return Err(StreamEvalError::MissingTerminalMethod);
        };
        invoke_helper_method(
            jdwp,
            self.class_id,
            self.thread,
            method_id,
            &self.args,
            "terminal",
        )
        .await
    }
}

/// Compile, inject, and resolve method IDs for a stream-eval helper class.
///
/// This is the full compile+inject pipeline used by the wire-level stream debugger.
pub(crate) async fn compile_and_inject_helper(
    cancel: &CancellationToken,
    jdwp: &JdwpClient,
    javac: &HotSwapJavacConfig,
    thread: ThreadId,
    frame_id: FrameId,
    location: Location,
    imports: &[String],
    stages: &[String],
    terminal: Option<&str>,
    max_sample_size: usize,
) -> Result<StreamEvalHelper, StreamEvalError> {
    // --- Bindings -----------------------------------------------------------
    let bindings = bindings::build_frame_bindings(jdwp, thread, frame_id, location).await?;
    let args = bindings.args_for_helper();

    // --- Source generation -------------------------------------------------
    let class_sig = jdwp.reference_type_signature(location.class_id).await?;
    let package = package_from_signature(&class_sig).unwrap_or_default();
    let nonce = STREAM_EVAL_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let simple_name = format!("__NovaStreamEvalHelper_{}_{}", std::process::id(), nonce);
    let fqcn = if package.is_empty() {
        simple_name.clone()
    } else {
        format!("{package}.{simple_name}")
    };

    let locals_for_java_gen = bindings.locals_for_java_gen();
    let fields_for_java_gen = bindings.fields_for_java_gen();
    let static_fields_for_java_gen = bindings.static_fields_for_java_gen();
    let src = java_gen::generate_stream_eval_helper_java_source(
        &package,
        &simple_name,
        imports,
        &locals_for_java_gen,
        &fields_for_java_gen,
        &static_fields_for_java_gen,
        stages,
        terminal,
        max_sample_size,
    );

    let temp_dir = crate::javac::hot_swap_temp_dir().map_err(StreamEvalError::Io)?;
    let src_path = temp_dir.path().join(format!("{simple_name}.java"));
    std::fs::write(&src_path, &src)?;

    // --- Compilation -------------------------------------------------------
    let javac = apply_stream_eval_defaults(javac);
    let compiled = compile_java_for_hot_swap(cancel, &javac, &src_path).await?;

    // --- Injection ---------------------------------------------------------
    let loader = jdwp.reference_type_class_loader(location.class_id).await?;
    if loader == 0 {
        return Err(StreamEvalError::Jdwp(JdwpError::Protocol(
            "unable to resolve classloader for paused frame; cannot define eval class".to_string(),
        )));
    }

    let mut helper_class_id: Option<ReferenceTypeId> = None;
    for class in &compiled {
        let id = jdwp
            .class_loader_define_class(loader, &class.class_name, &class.bytecode)
            .await?;
        if class.class_name == fqcn {
            helper_class_id = Some(id);
        }
    }

    let class_id =
        helper_class_id.ok_or_else(|| StreamEvalError::MissingHelperClass(fqcn.clone()))?;

    // --- Method resolution -------------------------------------------------
    let methods = jdwp.reference_type_methods(class_id).await?;
    let mut by_name: HashMap<String, MethodId> = HashMap::new();
    for method in methods {
        by_name.insert(method.name, method.method_id);
    }

    let mut stage_method_ids = Vec::with_capacity(stages.len());
    for idx in 0..stages.len() {
        let name = format!("stage{idx}");
        let id = by_name
            .get(&name)
            .copied()
            .ok_or_else(|| StreamEvalError::MissingHelperMethod(name.clone()))?;
        stage_method_ids.push(id);
    }

    let terminal_method_id = match terminal {
        Some(expr) if !expr.trim().is_empty() => Some(
            by_name
                .get("terminal")
                .copied()
                .ok_or_else(|| StreamEvalError::MissingHelperMethod("terminal".to_string()))?,
        ),
        _ => None,
    };

    Ok(StreamEvalHelper {
        class_name: fqcn,
        class_id,
        thread,
        args,
        stage_method_ids,
        terminal_method_id,
    })
}

async fn invoke_helper_method(
    jdwp: &JdwpClient,
    class_id: ReferenceTypeId,
    thread: ThreadId,
    method_id: MethodId,
    args: &[JdwpValue],
    method_label: &str,
) -> Result<JdwpValue, StreamEvalError> {
    let (value, exception) = jdwp
        .class_type_invoke_method(class_id, thread, method_id, args, INVOKE_SINGLE_THREADED)
        .await?;
    if exception != 0 {
        let thrown = format_thrown_exception_best_effort(jdwp, exception).await;
        return Err(StreamEvalError::InvocationException {
            method: method_label.to_string(),
            thrown,
        });
    }
    Ok(value)
}

async fn format_thrown_exception_best_effort(jdwp: &JdwpClient, exception: ObjectId) -> String {
    let mut inspector = Inspector::new(jdwp.clone());

    let runtime_type = inspector
        .runtime_type_name(exception)
        .await
        .unwrap_or_else(|_| "Exception".to_string());

    let message_id = inspector
        .object_children(exception)
        .await
        .ok()
        .and_then(|children| {
            children.into_iter().find_map(|child| {
                (child.name == "detailMessage")
                    .then_some(child.value)
                    .and_then(|value| match value {
                        JdwpValue::Object { id, .. } if id != 0 => Some(id),
                        _ => None,
                    })
            })
        });

    let message = match message_id {
        Some(id) => jdwp.string_reference_value(id).await.ok(),
        None => None,
    };

    match message {
        Some(msg) if !msg.is_empty() => format!("{runtime_type}: {msg}"),
        _ => runtime_type,
    }
}

fn package_from_signature(signature: &str) -> Option<String> {
    let internal = signature.strip_prefix('L')?.strip_suffix(';')?;
    let (pkg, _class) = internal.rsplit_once('/')?;
    Some(pkg.replace('/', "."))
}

/// Convert a `java.util.List` returned by an invoked helper method into a [`StreamSample`].
///
/// This is intended for stream-debug sampling stages that return `List` instances (e.g.
/// `sampleStream(stream, N)` which clamps the limit and handles primitive streams by boxing).
pub async fn list_to_stream_sample(
    jdwp: &JdwpClient,
    value: &JdwpValue,
) -> Result<StreamSample, StreamEvalError> {
    let object_id = match value {
        JdwpValue::Object { id, .. } => *id,
        other => return Err(StreamEvalError::ExpectedObject(other.clone())),
    };

    if object_id == 0 {
        return Ok(StreamSample {
            elements: Vec::new(),
            truncated: false,
            element_type: None,
            collection_type: None,
        });
    }

    let _pin = TemporaryObjectPin::new(jdwp, object_id).await;

    let mut inspector = inspect::Inspector::new(jdwp.clone());
    let collection_type = inspector.runtime_type_name(object_id).await.ok();

    let children = inspector.object_children(object_id).await?;
    let mut total_size: Option<usize> = None;
    let mut elements = Vec::new();
    let mut element_type: Option<String> = None;

    for child in children {
        match child.name.as_str() {
            "size" => {
                if let JdwpValue::Int(size) = child.value {
                    total_size = Some(size.max(0) as usize);
                }
            }
            "length" => {
                if let JdwpValue::Int(len) = child.value {
                    total_size = Some(len.max(0) as usize);
                }
            }
            _ => {
                if !child.name.starts_with('[') {
                    continue;
                }

                let (display, ty) = format_sample_value_wire(jdwp, &child.value).await;
                if element_type.is_none() {
                    element_type = ty;
                }
                elements.push(display);
            }
        }
    }

    let truncated = total_size
        .map(|size| elements.len() < size)
        .unwrap_or(false);

    Ok(StreamSample {
        elements,
        truncated,
        element_type,
        collection_type,
    })
}

#[async_recursion::async_recursion]
async fn format_sample_value_wire(
    jdwp: &JdwpClient,
    value: &JdwpValue,
) -> (String, Option<String>) {
    match value {
        JdwpValue::Void => ("void".to_string(), Some("void".to_string())),
        JdwpValue::Boolean(v) => (v.to_string(), Some("boolean".to_string())),
        JdwpValue::Byte(v) => (v.to_string(), Some("byte".to_string())),
        JdwpValue::Char(v) => (
            char::from_u32(u32::from(*v))
                .unwrap_or('\u{FFFD}')
                .to_string(),
            Some("char".to_string()),
        ),
        JdwpValue::Short(v) => (v.to_string(), Some("short".to_string())),
        JdwpValue::Int(v) => (v.to_string(), Some("int".to_string())),
        JdwpValue::Long(v) => (v.to_string(), Some("long".to_string())),
        JdwpValue::Float(v) => (v.to_string(), Some("float".to_string())),
        JdwpValue::Double(v) => (v.to_string(), Some("double".to_string())),
        JdwpValue::Object { id: 0, .. } => ("null".to_string(), None),
        JdwpValue::Object { id, .. } => match inspect::preview_object(jdwp, *id).await {
            Ok(preview) => match preview.kind {
                inspect::ObjectKindPreview::String { value } => (value, Some(preview.runtime_type)),
                inspect::ObjectKindPreview::PrimitiveWrapper { value } => {
                    format_sample_value_wire(jdwp, value.as_ref()).await
                }
                inspect::ObjectKindPreview::Optional { value } => {
                    let display = match value {
                        None => "Optional.empty".to_string(),
                        Some(v) => {
                            let (inner, _ty) = format_sample_value_wire(jdwp, v.as_ref()).await;
                            format!("Optional[{inner}]")
                        }
                    };
                    (display, Some(preview.runtime_type))
                }
                _ => (
                    format!("{}@0x{id:x}", preview.runtime_type),
                    Some(preview.runtime_type),
                ),
            },
            Err(_) => (format!("object@0x{id:x}"), None),
        },
    }
}

/// Define a class in the target VM, resolve a `stage0` method, and invoke it.
///
/// This is the JDWP-only portion of Nova's compile+inject evaluator used by stream-debug.
/// It is factored out so it can be exercised in CI using `MockJdwpServer` without requiring
/// a local JDK (no `javac`/`java`).
pub async fn define_class_and_invoke_stage0(
    jdwp: &JdwpClient,
    loader: ObjectId,
    thread: ThreadId,
    class_name: &str,
    bytecode: &[u8],
    args: &[JdwpValue],
) -> Result<JdwpValue, JdwpError> {
    let class_id = jdwp
        .class_loader_define_class(loader, class_name, bytecode)
        .await?;

    let methods = jdwp.reference_type_methods(class_id).await?;
    let stage0 = methods.iter().find(|m| m.name == "stage0").ok_or_else(|| {
        JdwpError::Protocol(format!(
            "expected injected class {class_id} to expose a stage0 method"
        ))
    })?;

    let (value, exception) = jdwp
        .class_type_invoke_method(
            class_id,
            thread,
            stage0.method_id,
            args,
            INVOKE_SINGLE_THREADED,
        )
        .await?;
    if exception != 0 {
        let thrown = format_thrown_exception_best_effort(jdwp, exception).await;
        return Err(JdwpError::Protocol(format!(
            "stage0 invocation threw {thrown}"
        )));
    }

    Ok(value)
}
