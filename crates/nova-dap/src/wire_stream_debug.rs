//! Wire-level stream debugging runtime.
//!
//! The legacy DAP adapter relies on JDI's built-in expression evaluator, while the
//! wire-level adapter must compile and inject helper bytecode into the debuggee
//! (typically via `javac` + `ClassLoaderReference.DefineClass`) before it can
//! evaluate stream pipeline stages.
//!
//! ## Timeout semantics
//!
//! `nova_stream_debug::StreamDebugConfig::max_total_time` is intended to budget the
//! *evaluation* phase (per-stage JDWP invocations). Helper compilation / injection
//! is treated as setup and is therefore **excluded** from `max_total_time`.
//!
//! To avoid hanging indefinitely in setup, we apply a separate, fixed setup
//! timeout (`SETUP_TIMEOUT`). The returned `StreamDebugResult.total_duration_ms`
//! includes setup time so clients can surface total latency, but setup time alone
//! will not cause a `max_total_time` timeout.

use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use nova_jdwp::wire::{
    inspect, JdwpClient, JdwpError, JdwpValue, MethodId, ObjectId, ReferenceTypeId, ThreadId,
};
use nova_scheduler::CancellationToken;
use nova_stream_debug::{analyze_stream_expression, StreamAnalysisError, StreamDebugConfig};
use nova_stream_debug::{StreamDebugResult, StreamSample};
use thiserror::Error;
use tokio::process::Command;

const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Temporarily pins a JDWP object id (best-effort) by issuing
/// `ObjectReference.DisableCollection` on creation and `EnableCollection` on drop.
///
/// This is primarily intended for *temporary* objects created as a byproduct of
/// evaluation (e.g. stream-debug sampling invoking `collect(toList())`), which
/// can otherwise be garbage collected immediately after the invoked method
/// returns.
///
/// ## Best-effort semantics
/// - If disable/enable fails (including `VmError(ERROR_INVALID_OBJECT)`), the
///   error is ignored.
/// - The guard tries to synchronously re-enable collection in `Drop` so we do
///   not leak pins across requests.
///
/// ## Important
/// Do **not** use this guard for long-lived/persistently pinned objects (e.g.
/// ones pinned via user interaction in the variables UI). JDWP collection
/// enable/disable is not ref-counted; enabling collection here could undo a
/// persistent pin.
pub(crate) struct TemporaryObjectPin {
    jdwp: JdwpClient,
    object_id: ObjectId,
}

impl TemporaryObjectPin {
    pub(crate) async fn new(jdwp: &JdwpClient, object_id: ObjectId) -> Self {
        if object_id != 0 {
            // Best-effort: the object may already be invalid/collected.
            let _ = jdwp.object_reference_disable_collection(object_id).await;
        }
        Self {
            jdwp: jdwp.clone(),
            object_id,
        }
    }
}

impl Drop for TemporaryObjectPin {
    fn drop(&mut self) {
        let object_id = self.object_id;
        if object_id == 0 {
            return;
        }

        let jdwp = self.jdwp.clone();
        let Some(handle) = tokio::runtime::Handle::try_current().ok() else {
            // No tokio runtime: best-effort.
            return;
        };

        // Prefer a synchronous (scoped) enable call so pins are always released
        // before returning from the higher-level request handler.
        let enable_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let handle = handle.clone();
            let jdwp = jdwp.clone();
            move || {
                tokio::task::block_in_place(|| {
                    handle.block_on(async move {
                        let _ = jdwp.object_reference_enable_collection(object_id).await;
                    });
                });
            }
        }));

        if enable_res.is_ok() {
            return;
        }

        // Fallback: if `block_in_place` is unavailable (e.g. current-thread
        // runtime), spawn and detach.
        let _ = handle.spawn(async move {
            let _ = jdwp.object_reference_enable_collection(object_id).await;
        });
    }
}

pub(crate) async fn inspect_object_children_temporarily_pinned(
    jdwp: &JdwpClient,
    object_id: ObjectId,
) -> Result<Vec<(String, JdwpValue, Option<String>)>, JdwpError> {
    if object_id == 0 {
        return Ok(Vec::new());
    }
    let _pin = TemporaryObjectPin::new(jdwp, object_id).await;
    inspect::object_children(jdwp, object_id).await
}

pub(crate) async fn inspect_object_preview_temporarily_pinned(
    jdwp: &JdwpClient,
    object_id: ObjectId,
) -> Result<inspect::ObjectPreview, JdwpError> {
    if object_id == 0 {
        return Err(JdwpError::Protocol(
            "expected non-null object id for preview".to_string(),
        ));
    }
    let _pin = TemporaryObjectPin::new(jdwp, object_id).await;
    inspect::preview_object(jdwp, object_id).await
}

/// Stream-debug sampling helper.
///
/// Invokes a helper method (via JDWP `ClassType.InvokeMethod`) that returns an
/// object ID, pins the returned object, then reads its children for display.
///
/// This mirrors the stream-debug sampling pattern of producing a fresh
/// `List` (e.g. from `stream.collect(toList())`) and immediately inspecting it.
pub async fn sample_object_children_via_invoke_method(
    jdwp: &JdwpClient,
    class_id: ReferenceTypeId,
    thread: ThreadId,
    method_id: MethodId,
    args: &[JdwpValue],
) -> Result<Vec<(String, JdwpValue, Option<String>)>, JdwpError> {
    let (value, exception) = jdwp
        .class_type_invoke_method(class_id, thread, method_id, args, 0)
        .await?;
    if exception != 0 {
        return Err(JdwpError::Protocol(format!(
            "exception thrown during invoke_method (id=0x{exception:x})"
        )));
    }

    let object_id = match value {
        JdwpValue::Object { id, .. } => id,
        _ => return Ok(Vec::new()),
    };
    if object_id == 0 {
        return Ok(Vec::new());
    }

    inspect_object_children_temporarily_pinned(jdwp, object_id).await
}

#[derive(Debug, Error)]
pub enum WireStreamDebugError {
    #[error(transparent)]
    Analysis(#[from] StreamAnalysisError),
    #[error(
        "refusing to run stream debug on `{stream_expr}` because it looks like an existing Stream value.\n\
Stream debug samples by evaluating `.limit(...).collect(...)`, which *consumes* streams.\n\
Rewrite the expression to recreate the stream (e.g. `collection.stream()` or `java.util.Arrays.stream(array)`)."
    )]
    UnsafeExistingStream { stream_expr: String },
    #[error("evaluation cancelled")]
    Cancelled,
    /// Setup (helper compilation / injection) exceeded the fixed setup timeout.
    #[error("setup exceeded time limit")]
    SetupTimeout,
    /// Evaluation exceeded the configured `max_total_time` budget.
    #[error("evaluation exceeded time limit")]
    Timeout,
    #[error(transparent)]
    Jdwp(#[from] JdwpError),
    #[error("no threads available in target VM")]
    NoThreads,
    #[error("no classes available in target VM")]
    NoClasses,
    #[error("helper class did not expose required method `{0}`")]
    MissingHelperMethod(&'static str),
    #[error("failed to compile helper class: {0}")]
    Compile(String),
}

#[derive(Debug, Clone)]
struct CompiledHelperClass {
    name: String,
    bytecode: Vec<u8>,
}

trait HelperClassCompiler {
    fn compile<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CompiledHelperClass, WireStreamDebugError>> + Send + 'a>>;
}

#[derive(Debug, Clone)]
struct JavacHelperCompiler {
    javac: String,
}

impl Default for JavacHelperCompiler {
    fn default() -> Self {
        Self {
            javac: "javac".to_string(),
        }
    }
}

impl HelperClassCompiler for JavacHelperCompiler {
    fn compile<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CompiledHelperClass, WireStreamDebugError>> + Send + 'a>>
    {
        Box::pin(async move { compile_helper_with_javac(cancel, &self.javac).await })
    }
}

async fn cancellable_jdwp<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = Result<T, JdwpError>>,
) -> Result<T, WireStreamDebugError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(WireStreamDebugError::Cancelled),
        res = fut => Ok(res?),
    }
}

async fn budgeted_jdwp<T>(
    cancel: &CancellationToken,
    budget_start: Instant,
    budget: Duration,
    fut: impl Future<Output = Result<T, JdwpError>>,
) -> Result<T, WireStreamDebugError> {
    if cancel.is_cancelled() {
        return Err(WireStreamDebugError::Cancelled);
    }

    let elapsed = budget_start.elapsed();
    let remaining = budget.checked_sub(elapsed).unwrap_or(Duration::ZERO);
    if remaining.is_zero() {
        return Err(WireStreamDebugError::Timeout);
    }

    tokio::select! {
        _ = cancel.cancelled() => Err(WireStreamDebugError::Cancelled),
        res = tokio::time::timeout(remaining, fut) => match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(err)) => Err(err.into()),
            Err(_elapsed) => Err(WireStreamDebugError::Timeout),
        }
    }
}

/// Evaluate a stream pipeline with wire-level JDWP.
///
/// NOTE: The wire-level stream debugger is still evolving. This entrypoint exists
/// primarily to codify timeout + cancellation semantics around helper compilation /
/// injection. The runtime currently performs a minimal helper invocation to
/// validate wiring.
pub async fn debug_stream_wire(
    jdwp: &JdwpClient,
    expression: &str,
    config: &StreamDebugConfig,
    cancel: &CancellationToken,
) -> Result<StreamDebugResult, WireStreamDebugError> {
    let chain = analyze_stream_expression(expression)?;
    debug_stream_wire_with_compiler(
        jdwp,
        &chain,
        config,
        cancel,
        &JavacHelperCompiler::default(),
    )
    .await
}

async fn debug_stream_wire_with_compiler(
    jdwp: &JdwpClient,
    chain: &nova_stream_debug::StreamChain,
    config: &StreamDebugConfig,
    cancel: &CancellationToken,
    compiler: &impl HelperClassCompiler,
) -> Result<StreamDebugResult, WireStreamDebugError> {
    let started = Instant::now();

    if let nova_stream_debug::StreamSource::ExistingStream { stream_expr } = &chain.source {
        let stream_expr = stream_expr.trim();
        if nova_stream_debug::is_pure_access_expr(stream_expr) {
            return Err(WireStreamDebugError::UnsafeExistingStream {
                stream_expr: stream_expr.to_string(),
            });
        }
    }

    // --- Setup phase (excluded from max_total_time) -------------------------
    let (thread, helper_class, helper_method) = match setup_helper(jdwp, cancel, compiler).await {
        Ok(res) => res,
        Err(WireStreamDebugError::Compile(msg)) => {
            return Err(WireStreamDebugError::Compile(format!(
                "while evaluating `{}`:\n{msg}",
                chain.expression
            )));
        }
        Err(other) => return Err(other),
    };

    // --- Evaluation phase (budgeted by max_total_time) ----------------------
    let eval_started = Instant::now();
    let (value, source_duration_ms) = timed_async(|| async {
        budgeted_jdwp(
            cancel,
            eval_started,
            config.max_total_time,
            jdwp.class_type_invoke_method(
                helper_class,
                thread,
                helper_method,
                &[JdwpValue::Int(1)],
                0,
            ),
        )
        .await
    })
    .await?;

    let source_sample = StreamSample {
        elements: vec![format_wire_value(&value.0)],
        truncated: false,
        element_type: None,
        collection_type: None,
    };

    Ok(StreamDebugResult {
        expression: chain.expression.clone(),
        source: chain.source.clone(),
        source_sample,
        source_duration_ms,
        steps: Vec::new(),
        terminal: None,
        total_duration_ms: started.elapsed().as_millis(),
    })
}

async fn setup_helper(
    jdwp: &JdwpClient,
    cancel: &CancellationToken,
    compiler: &impl HelperClassCompiler,
) -> Result<(ThreadId, ReferenceTypeId, MethodId), WireStreamDebugError> {
    let setup = async {
        let compiled = compiler.compile(cancel).await?;
        let threads = cancellable_jdwp(cancel, jdwp.all_threads()).await?;
        let thread = threads
            .into_iter()
            .next()
            .ok_or(WireStreamDebugError::NoThreads)?;

        let classes = cancellable_jdwp(cancel, jdwp.all_classes()).await?;
        let class_id = classes
            .into_iter()
            .next()
            .map(|c| c.type_id)
            .ok_or(WireStreamDebugError::NoClasses)?;

        let loader = cancellable_jdwp(cancel, jdwp.reference_type_class_loader(class_id)).await?;

        let helper_class = cancellable_jdwp(
            cancel,
            jdwp.class_loader_define_class(loader, &compiled.name, &compiled.bytecode),
        )
        .await?;

        let methods = cancellable_jdwp(cancel, jdwp.reference_type_methods(helper_class)).await?;
        let Some(method_id) = methods
            .into_iter()
            .find(|m| m.name == "ping")
            .map(|m| m.method_id)
        else {
            return Err(WireStreamDebugError::MissingHelperMethod("ping"));
        };

        Ok::<_, WireStreamDebugError>((thread, helper_class, method_id))
    };

    tokio::select! {
        _ = cancel.cancelled() => Err(WireStreamDebugError::Cancelled),
        res = tokio::time::timeout(SETUP_TIMEOUT, setup) => match res {
            Ok(res) => res,
            Err(_elapsed) => Err(WireStreamDebugError::SetupTimeout),
        }
    }
}

fn format_wire_value(value: &JdwpValue) -> String {
    match value {
        JdwpValue::Boolean(v) => v.to_string(),
        JdwpValue::Byte(v) => v.to_string(),
        JdwpValue::Char(v) => char::from_u32(*v as u32).unwrap_or('\u{FFFD}').to_string(),
        JdwpValue::Short(v) => v.to_string(),
        JdwpValue::Int(v) => v.to_string(),
        JdwpValue::Long(v) => v.to_string(),
        JdwpValue::Float(v) => v.to_string(),
        JdwpValue::Double(v) => v.to_string(),
        JdwpValue::Void => "void".to_string(),
        JdwpValue::Object { tag: _, id } => format!("object#{id}"),
    }
}

fn jdwp_primitive_type_name(value: &JdwpValue) -> Option<String> {
    Some(
        match value {
            JdwpValue::Void => "void",
            JdwpValue::Boolean(_) => "boolean",
            JdwpValue::Byte(_) => "byte",
            JdwpValue::Char(_) => "char",
            JdwpValue::Short(_) => "short",
            JdwpValue::Int(_) => "int",
            JdwpValue::Long(_) => "long",
            JdwpValue::Float(_) => "float",
            JdwpValue::Double(_) => "double",
            JdwpValue::Object { .. } => return None,
        }
        .to_string(),
    )
}

/// Stream-debug value formatter that mirrors `nova-stream-debug`'s legacy `format_sample_value`
/// semantics.
///
/// The wire adapter receives `JdwpValue`s that may be boxed primitives (`Integer`, `Long`, ...)
/// and should show them as the underlying primitive string rather than `java.lang.Integer#...`.
#[async_recursion::async_recursion]
pub(crate) async fn format_stream_sample_value(
    inspector: &mut inspect::Inspector,
    value: &JdwpValue,
) -> Result<(String, Option<String>), JdwpError> {
    match value {
        JdwpValue::Object { id: 0, .. } => Ok(("null".to_string(), None)),
        JdwpValue::Void => Ok(("void".to_string(), Some("void".to_string()))),
        JdwpValue::Boolean(v) => Ok((v.to_string(), jdwp_primitive_type_name(value))),
        JdwpValue::Byte(v) => Ok((v.to_string(), jdwp_primitive_type_name(value))),
        JdwpValue::Char(v) => Ok((
            char::from_u32(*v as u32).unwrap_or('\u{FFFD}').to_string(),
            jdwp_primitive_type_name(value),
        )),
        JdwpValue::Short(v) => Ok((v.to_string(), jdwp_primitive_type_name(value))),
        JdwpValue::Int(v) => Ok((v.to_string(), jdwp_primitive_type_name(value))),
        JdwpValue::Long(v) => Ok((v.to_string(), jdwp_primitive_type_name(value))),
        JdwpValue::Float(v) => Ok((v.to_string(), jdwp_primitive_type_name(value))),
        JdwpValue::Double(v) => Ok((v.to_string(), jdwp_primitive_type_name(value))),
        JdwpValue::Object { id, .. } => {
            let id = *id;
            let preview = match inspector.preview_object(id).await {
                Ok(preview) => preview,
                Err(_) => {
                    // Best-effort fallback: we may not be able to resolve the runtime type (e.g.
                    // the object was collected). Still return a stable string.
                    return Ok((format!("object#{id}"), None));
                }
            };

            match preview.kind {
                inspect::ObjectKindPreview::String { value } => {
                    Ok((value, Some(preview.runtime_type)))
                }
                inspect::ObjectKindPreview::PrimitiveWrapper { value } => {
                    format_stream_sample_value(inspector, &value).await
                }
                inspect::ObjectKindPreview::Optional { value } => {
                    let display = match value {
                        None => "Optional.empty".to_string(),
                        Some(inner) => {
                            let (inner_display, _ty) =
                                format_stream_sample_value(inspector, &inner).await?;
                            format!("Optional[{inner_display}]")
                        }
                    };
                    Ok((display, Some(preview.runtime_type)))
                }
                _ => Ok((
                    format!("{}#{id}", preview.runtime_type),
                    Some(preview.runtime_type),
                )),
            }
        }
    }
}

fn parse_indexed_child_name(name: &str) -> Option<usize> {
    let name = name.trim();
    if !name.starts_with('[') || !name.ends_with(']') {
        return None;
    }
    let inner = &name[1..name.len().saturating_sub(1)];
    if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    inner.parse::<usize>().ok()
}

/// Convert a list-like object (e.g. `java.util.ArrayList`) into a `StreamSample` using the same
/// child-expansion logic as the variables UI (`Inspector::object_children`).
///
/// This differs from the preview-based approach because `object_children` for lists/arrays
/// includes a metadata entry (e.g. `size` / `length`) followed by indexed elements (`[0]`, ...).
/// Stream debug samples should include only the indexed elements.
pub(crate) async fn stream_sample_from_list_object(
    jdwp: &JdwpClient,
    list_object_id: ObjectId,
) -> Result<StreamSample, JdwpError> {
    if list_object_id == 0 {
        return Ok(StreamSample {
            elements: Vec::new(),
            truncated: false,
            element_type: None,
            collection_type: None,
        });
    }

    // Keep the list pinned throughout preview + children inspection to avoid GC races for
    // temporary objects returned from InvokeMethod.
    let _pin = TemporaryObjectPin::new(jdwp, list_object_id).await;
    let mut inspector = inspect::Inspector::new(jdwp.clone());

    // Prefer the richer preview runtime type, but fall back to the simpler runtime-type lookup
    // (best-effort) so we can still surface samples even if the preview helpers fail.
    let collection_type = match inspector.preview_object(list_object_id).await {
        Ok(preview) => Some(preview.runtime_type),
        Err(_) => inspector.runtime_type_name(list_object_id).await.ok(),
    };

    let children = inspector.object_children(list_object_id).await?;

    let returned_size =
        children
            .iter()
            .find_map(|child| match (child.name.as_str(), &child.value) {
                ("size" | "length", JdwpValue::Int(v)) => Some((*v).max(0) as usize),
                _ => None,
            });

    let mut indexed_children: Vec<(usize, JdwpValue)> = children
        .into_iter()
        .filter_map(|child| parse_indexed_child_name(&child.name).map(|idx| (idx, child.value)))
        .collect();
    indexed_children.sort_by_key(|(idx, _)| *idx);

    let mut elements = Vec::with_capacity(indexed_children.len());
    let mut element_type: Option<String> = None;
    for (_idx, value) in indexed_children {
        let (display, ty) = format_stream_sample_value(&mut inspector, &value).await?;
        elements.push(display);
        if element_type.is_none() {
            element_type = ty;
        }
    }

    let truncated = returned_size
        .map(|size| size > elements.len())
        .unwrap_or(false);

    Ok(StreamSample {
        elements,
        truncated,
        element_type,
        collection_type,
    })
}

async fn timed_async<T, E, Fut>(f: impl FnOnce() -> Fut) -> Result<(T, u128), E>
where
    Fut: Future<Output = Result<T, E>>,
{
    let start = Instant::now();
    let value = f().await?;
    Ok((value, start.elapsed().as_millis()))
}

async fn compile_helper_with_javac(
    cancel: &CancellationToken,
    javac: &str,
) -> Result<CompiledHelperClass, WireStreamDebugError> {
    // The helper is intentionally tiny; its purpose is to validate class injection + method
    // invocation plumbing.
    const CLASS_NAME: &str = "NovaStreamDebugHelper";
    const SOURCE: &str = r#"
public class NovaStreamDebugHelper {
  public static Object ping(Object x) {
    return x;
  }
}
"#;

    let dir =
        stream_debug_temp_dir().map_err(|err| WireStreamDebugError::Compile(err.to_string()))?;
    let out_dir = dir.join("out");
    if let Err(err) = std::fs::create_dir(&out_dir) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(WireStreamDebugError::Compile(format!(
            "failed to create temp output dir: {err}"
        )));
    }

    let src_path = dir.join(format!("{CLASS_NAME}.java"));
    if let Err(err) = std::fs::write(&src_path, SOURCE) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(WireStreamDebugError::Compile(format!(
            "failed to write temp helper source: {err}"
        )));
    }

    // Prefer `--release 8` so the helper class can load on older debuggee JVMs when attaching.
    // If the host toolchain is JDK 8 (no `--release` support), retry with `-source/-target`.
    let release_attempt =
        run_javac_attempt(cancel, javac, &out_dir, &src_path, &["--release", "8"]).await;
    let status = match release_attempt {
        Ok(status) => status,
        Err(err) => {
            let WireStreamDebugError::Compile(msg) = &err else {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(err);
            };
            let lower = msg.to_lowercase();
            let is_release_flag_unsupported = lower.contains("invalid flag: --release")
                || lower.contains("unrecognized option: --release");
            if !is_release_flag_unsupported {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(err);
            }

            // Clean output dir before retrying to avoid stale classes.
            let _ = std::fs::remove_dir_all(&out_dir);
            if let Err(err) = std::fs::create_dir_all(&out_dir) {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(WireStreamDebugError::Compile(format!(
                    "failed to recreate temp output dir: {err}"
                )));
            }

            let retry_attempt = run_javac_attempt(
                cancel,
                javac,
                &out_dir,
                &src_path,
                &["-source", "1.8", "-target", "1.8"],
            )
            .await;
            match retry_attempt {
                Ok(status) => status,
                Err(err2) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(match err2 {
                        WireStreamDebugError::Compile(msg2) => WireStreamDebugError::Compile(
                            format!("{msg}\n\nretry without `--release` failed:\n{msg2}"),
                        ),
                        other => other,
                    });
                }
            }
        }
    };

    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(WireStreamDebugError::Compile(
            "javac reported failure status without diagnostics".to_string(),
        ));
    }

    let class_file = out_dir.join(format!("{CLASS_NAME}.class"));
    let bytecode = match std::fs::read(&class_file) {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(WireStreamDebugError::Compile(format!(
                "failed to read compiled helper class: {err}"
            )));
        }
    };

    let _ = std::fs::remove_dir_all(&dir);
    Ok(CompiledHelperClass {
        name: CLASS_NAME.to_string(),
        bytecode,
    })
}

async fn run_javac_attempt(
    cancel: &CancellationToken,
    javac: &str,
    out_dir: &Path,
    src_path: &Path,
    extra_args: &[&str],
) -> Result<std::process::ExitStatus, WireStreamDebugError> {
    let mut cmd = Command::new(javac);
    cmd.arg("-J-Xms16m");
    cmd.arg("-J-Xmx256m");
    cmd.arg("-J-XX:CompressedClassSpaceSize=64m");
    cmd.arg("-g");
    cmd.arg("-encoding");
    cmd.arg("UTF-8");
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.arg("-d");
    cmd.arg(out_dir);
    cmd.arg(src_path);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|err| WireStreamDebugError::Compile(format!("failed to spawn {javac}: {err}")))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| WireStreamDebugError::Compile("javac stdout unavailable".to_string()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| WireStreamDebugError::Compile("javac stderr unavailable".to_string()))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await;
        buf
    });

    let status = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(WireStreamDebugError::Cancelled);
        }
        res = tokio::time::timeout(SETUP_TIMEOUT, child.wait()) => match res {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => {
                stdout_task.abort();
                stderr_task.abort();
                return Err(WireStreamDebugError::Compile(format!("javac failed: {err}")));
            }
            Err(_elapsed) => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                stdout_task.abort();
                stderr_task.abort();
                return Err(WireStreamDebugError::SetupTimeout);
            }
        },
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    if !status.success() {
        let diagnostics = crate::javac::format_javac_failure(&stdout, &stderr);
        let message = if diagnostics.trim().is_empty() {
            format!(
                "stream debug helper compilation failed\nGenerated source: {}\n\njavac exited with {status} but produced no diagnostics",
                src_path.display()
            )
        } else {
            format_stream_debug_helper_compile_failure(src_path, &diagnostics)
        };
        return Err(WireStreamDebugError::Compile(message));
    }

    Ok(status)
}

fn stream_debug_temp_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir().join("nova-dap-stream-debug");
    std::fs::create_dir_all(&base)?;
    let id = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("compile-{id}-{}", std::process::id()));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

fn format_stream_debug_helper_compile_failure(source_path: &Path, diagnostics: &str) -> String {
    let mut out = String::new();
    out.push_str("stream debug helper compilation failed\n");
    out.push_str(&format!("Generated source: {}\n", source_path.display()));

    if let Some((line, col)) = crate::javac::parse_first_formatted_javac_location(diagnostics) {
        if let Ok(source) = std::fs::read_to_string(source_path) {
            if let Some(src_line) = source.lines().nth(line.saturating_sub(1)) {
                let mut preview = src_line.trim_end().to_string();
                const MAX_PREVIEW_LEN: usize = 200;
                if preview.len() > MAX_PREVIEW_LEN {
                    preview.truncate(MAX_PREVIEW_LEN);
                    preview.push('…');
                }
                out.push_str(&format!(
                    "Generated code (line {line}, col {col}): {preview}\n"
                ));
            }
        }
    }
    out.push('\n');
    out.push_str("Javac diagnostics:\n");
    out.push_str(diagnostics);

    for hint in stream_debug_compile_hints(diagnostics) {
        out.push_str("\n\nHint: ");
        out.push_str(&hint);
    }

    out
}

fn stream_debug_compile_hints(javac_output: &str) -> Vec<String> {
    let lower = javac_output.to_lowercase();
    let mut hints = Vec::new();

    // Missing Collectors import / symbol.
    if javac_output.contains("Collectors") && lower.contains("cannot find symbol") {
        hints.push(
            "`Collectors` was not found. Try using the fully-qualified name \
`java.util.stream.Collectors` (e.g. `java.util.stream.Collectors.toList()`), or ensure the adapter \
can locate your source file so it can copy your project's imports."
                .to_string(),
        );
    }

    // Private access failures (injected helper class is not an inner class).
    if lower.contains("has private access")
        || lower.contains("private access")
        || lower.contains("cannot access private")
    {
        hints.push(
            "The stream debugger compiles an injected helper class. It can only access public / \
protected / package-private members; private members are not accessible from the helper. Consider \
using a public accessor or rewriting the expression."
                .to_string(),
        );
    }

    // Type inference / raw types / lambda inference failures.
    if lower.contains("cannot infer type")
        || lower.contains("cannot infer type arguments")
        || lower.contains("inference variable")
        || lower.contains("bad return type in lambda expression")
    {
        hints.push(
            "Java type inference can fail in the injected helper context (especially with raw or \
erased generic types). Try adding explicit casts or types, e.g. \
`((java.util.List<Foo>) list).stream()` or assigning the stream to a typed local variable."
                .to_string(),
        );
    }

    // Unqualified method calls (compile+inject helpers are top-level classes).
    if lower.contains("cannot find symbol") {
        if let Some(name) = javac_output
            .lines()
            .find_map(|line| extract_missing_method_name(line))
        {
            hints.push(format!(
                "Javac could not resolve `{name}(...)`. The stream debugger evaluates expressions in an injected helper class, \
so unqualified method calls from your source file may need to be qualified. Try `this.{name}(...)` (instance) or \
`DeclaringClass.{name}(...)` (static)."
            ));
        }
    }

    hints
}

fn extract_missing_method_name(line: &str) -> Option<String> {
    // `javac` continuation line:
    // `symbol:   method helper(int)`
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("symbol:")?.trim_start();
    let rest = rest.strip_prefix("method")?.trim_start();

    let start = rest
        .char_indices()
        .find(|(_, ch)| is_ident_start(*ch))
        .map(|(idx, _)| idx)?;
    let after = &rest[start..];
    let end = after
        .char_indices()
        .find(|(_, ch)| !is_ident_part(*ch))
        .map(|(idx, _)| idx)
        .unwrap_or(after.len());
    let name = &after[..end];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_alphabetic()
}

fn is_ident_part(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;
    use std::{io::Write, path::PathBuf};

    use nova_jdwp::wire::mock::{DelayedReply, MockJdwpServer, MockJdwpServerConfig};

    #[test]
    fn stream_debug_compile_failure_formats_javac_diagnostics_with_context_and_hints() {
        let raw_javac_stderr = concat!(
            "/tmp/NovaStreamDebugHelper.java:10: error: cannot find symbol\n",
            "        return list.stream().collect(Collectors.toList());\n",
            "                                    ^\n",
            "  symbol:   variable Collectors\n",
            "  location: class NovaStreamDebugHelper\n",
            "1 error\n",
        );

        let diagnostics = crate::javac::format_javac_failure(&[], raw_javac_stderr.as_bytes());
        let message = format_stream_debug_helper_compile_failure(
            Path::new("/tmp/NovaStreamDebugHelper.java"),
            &diagnostics,
        );

        assert!(
            message.contains("stream debug helper compilation failed"),
            "missing context header:\n{message}"
        );
        assert!(
            message.contains("Generated source: /tmp/NovaStreamDebugHelper.java"),
            "missing source path:\n{message}"
        );
        assert!(
            message.contains("/tmp/NovaStreamDebugHelper.java:10:"),
            "expected formatted javac location:\n{message}"
        );
        assert!(
            message.contains("java.util.stream.Collectors"),
            "expected Collectors hint:\n{message}"
        );
    }

    #[test]
    fn stream_debug_compile_failure_includes_generated_source_snippet_when_available() {
        let mut file = tempfile::Builder::new()
            .prefix("NovaStreamDebugHelper-")
            .suffix(".java")
            .tempfile()
            .unwrap();

        // Make the failing statement land on line 10.
        writeln!(file, "").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, "").unwrap();
        writeln!(
            file,
            "        return list.stream().collect(Collectors.toList());"
        )
        .unwrap();

        let path: PathBuf = file.path().to_path_buf();

        let raw_javac_stderr = format!(
            "{}:10: error: cannot find symbol\n\
        return list.stream().collect(Collectors.toList());\n\
                                    ^\n\
  symbol:   variable Collectors\n\
  location: class NovaStreamDebugHelper\n\
1 error\n",
            path.display()
        );

        let diagnostics = crate::javac::format_javac_failure(&[], raw_javac_stderr.as_bytes());
        let message = format_stream_debug_helper_compile_failure(&path, &diagnostics);

        assert!(
            message.contains("Generated code (line 10"),
            "expected generated source snippet:\n{message}"
        );
        assert!(
            message.contains("return list.stream().collect(Collectors.toList());"),
            "expected snippet to contain the offending line:\n{message}"
        );
    }

    #[test]
    fn stream_debug_compile_failure_includes_hint_for_unqualified_method_calls() {
        let raw_javac_stderr = concat!(
            "/tmp/NovaStreamDebugHelper.java:10: error: cannot find symbol\n",
            "        return helper(1);\n",
            "               ^\n",
            "  symbol:   method helper(int)\n",
            "  location: class NovaStreamDebugHelper\n",
            "1 error\n",
        );

        let diagnostics = crate::javac::format_javac_failure(&[], raw_javac_stderr.as_bytes());
        let message = format_stream_debug_helper_compile_failure(
            Path::new("/tmp/NovaStreamDebugHelper.java"),
            &diagnostics,
        );

        assert!(
            message.contains("this.helper"),
            "expected unqualified-method hint in message:\n{message}"
        );
    }

    #[tokio::test]
    async fn compile_failures_include_user_expression_context() {
        let jdwp_server = MockJdwpServer::spawn().await.unwrap();
        let jdwp = JdwpClient::connect(jdwp_server.addr()).await.unwrap();
        let cancel = CancellationToken::new();
        let cfg = StreamDebugConfig::default();
        let chain = analyze_stream_expression("list.stream().count()").unwrap();

        struct FailingCompiler;
        impl HelperClassCompiler for FailingCompiler {
            fn compile<'a>(
                &'a self,
                _cancel: &'a CancellationToken,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<CompiledHelperClass, WireStreamDebugError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    Err(WireStreamDebugError::Compile(
                        "/tmp/Foo.java:1:1: cannot find symbol".to_string(),
                    ))
                })
            }
        }

        let result =
            debug_stream_wire_with_compiler(&jdwp, &chain, &cfg, &cancel, &FailingCompiler).await;
        match result {
            Err(WireStreamDebugError::Compile(msg)) => {
                assert!(
                    msg.contains("while evaluating `list.stream().count()`"),
                    "expected expression context in message:\n{msg}"
                );
                assert!(
                    msg.contains("cannot find symbol"),
                    "expected original diagnostics to be preserved:\n{msg}"
                );
            }
            other => panic!("expected Compile error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refuses_existing_stream_values() {
        let jdwp_server = MockJdwpServer::spawn().await.unwrap();
        let jdwp = JdwpClient::connect(jdwp_server.addr()).await.unwrap();
        let cancel = CancellationToken::new();
        let cfg = StreamDebugConfig::default();
        let chain = analyze_stream_expression("s.filter(x -> x > 0).count()").unwrap();

        struct PanicCompiler;
        impl HelperClassCompiler for PanicCompiler {
            fn compile<'a>(
                &'a self,
                _cancel: &'a CancellationToken,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<CompiledHelperClass, WireStreamDebugError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    panic!("compiler should not be invoked for unsafe existing stream values");
                })
            }
        }

        let result =
            debug_stream_wire_with_compiler(&jdwp, &chain, &cfg, &cancel, &PanicCompiler).await;
        match result {
            Err(WireStreamDebugError::UnsafeExistingStream { stream_expr }) => {
                assert_eq!(stream_expr, "s");
            }
            other => panic!("expected UnsafeExistingStream, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sample_stream_inspection_temporarily_pins_object_ids() {
        let mut config = MockJdwpServerConfig::default();
        // Delay a command that runs *after* we pin but *before* we return, so
        // the test can observe the pinned set mid-flight.
        config.delayed_replies = vec![DelayedReply {
            command_set: 9, // ObjectReference
            command: 2,     // GetValues
            delay: Duration::from_millis(200),
        }];

        let server = MockJdwpServer::spawn_with_config(config).await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let thread = client.all_threads().await.unwrap()[0];
        let object_id: ObjectId = 0xDEAD_BEEF;

        assert!(server.pinned_object_ids().await.is_empty());

        let client_task = client.clone();
        let task = tokio::spawn(async move {
            let arg = JdwpValue::Object {
                tag: b'L',
                id: object_id,
            };
            sample_object_children_via_invoke_method(&client_task, 0x9001, thread, 0x4001, &[arg])
                .await
                .unwrap()
        });

        // Wait until the sampling code has pinned the object.
        for _ in 0..50 {
            if server.pinned_object_ids().await.contains(&object_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            server.pinned_object_ids().await.contains(&object_id),
            "expected object to be pinned during inspection"
        );

        let children = task.await.unwrap();
        assert!(
            !children.is_empty(),
            "expected mock object to have at least one child"
        );

        assert!(
            server.pinned_object_ids().await.is_empty(),
            "expected pin to be released after sampling"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preview_object_inspection_temporarily_pins_object_ids() {
        let mut config = MockJdwpServerConfig::default();
        // Delay `ObjectReference.ReferenceType`, which `inspect::preview_object` issues after
        // pinning but before returning.
        config.delayed_replies = vec![DelayedReply {
            command_set: 9, // ObjectReference
            command: 1,     // ReferenceType
            delay: Duration::from_millis(200),
        }];

        let server = MockJdwpServer::spawn_with_config(config).await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let object_id: ObjectId = 0xDEAD_BEEF;

        assert!(server.pinned_object_ids().await.is_empty());

        let client_task = client.clone();
        let task = tokio::spawn(async move {
            inspect_object_preview_temporarily_pinned(&client_task, object_id)
                .await
                .unwrap()
        });

        // Wait until the preview code has pinned the object.
        for _ in 0..50 {
            if server.pinned_object_ids().await.contains(&object_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            server.pinned_object_ids().await.contains(&object_id),
            "expected object to be pinned during preview inspection"
        );

        let preview = task.await.unwrap();
        assert_eq!(
            preview.kind,
            inspect::ObjectKindPreview::Plain,
            "expected default mock preview kind for unknown object ids"
        );

        assert!(
            server.pinned_object_ids().await.is_empty(),
            "expected pin to be released after preview inspection"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sample_from_list_object_filters_size_and_unwraps_boxed_primitives() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let sample = stream_sample_from_list_object(&client, server.sample_arraylist_id())
            .await
            .unwrap();

        assert_eq!(
            sample.collection_type.as_deref(),
            Some("java.util.ArrayList"),
            "expected collection_type from preview_object: {sample:?}"
        );
        assert_eq!(
            sample.elements,
            vec!["10", "20", "30"],
            "expected only indexed elements (no `size` metadata) and boxed primitives to be unwrapped: {sample:?}"
        );
        assert_eq!(
            sample.element_type.as_deref(),
            Some("int"),
            "expected primitive wrapper to infer element type: {sample:?}"
        );
        assert!(
            !sample.truncated,
            "expected sample list size to match returned elements: {sample:?}"
        );
    }

    #[derive(Debug, Clone)]
    struct TestCompiler {
        delay: Duration,
    }

    impl HelperClassCompiler for TestCompiler {
        fn compile<'a>(
            &'a self,
            cancel: &'a CancellationToken,
        ) -> Pin<
            Box<dyn Future<Output = Result<CompiledHelperClass, WireStreamDebugError>> + Send + 'a>,
        > {
            Box::pin(async move {
                tokio::select! {
                    _ = cancel.cancelled() => Err(WireStreamDebugError::Cancelled),
                    _ = tokio::time::sleep(self.delay) => Ok(CompiledHelperClass {
                        name: "NovaStreamDebugHelper".to_string(),
                        // The mock JDWP server does not validate class bytes.
                        bytecode: vec![0xCA, 0xFE, 0xBA, 0xBE],
                    }),
                }
            })
        }
    }

    #[tokio::test]
    async fn setup_delay_does_not_count_towards_max_total_time() {
        let jdwp_server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            delayed_replies: vec![DelayedReply {
                command_set: 14,
                command: 2, // ClassLoaderReference.DefineClass
                delay: Duration::from_millis(300),
            }],
            ..Default::default()
        })
        .await
        .unwrap();

        let jdwp = JdwpClient::connect(jdwp_server.addr()).await.unwrap();
        let cancel = CancellationToken::new();
        let cfg = StreamDebugConfig::default(); // 250ms max_total_time
        let chain = analyze_stream_expression("list.stream().map(x -> x).count()").unwrap();

        let result = debug_stream_wire_with_compiler(
            &jdwp,
            &chain,
            &cfg,
            &cancel,
            &TestCompiler {
                delay: Duration::from_millis(0),
            },
        )
        .await;

        assert!(result.is_ok(), "expected success, got {result:?}");
        let runtime = result.unwrap();
        assert!(
            runtime.total_duration_ms >= 250,
            "expected setup time to contribute to total_duration_ms"
        );
    }

    #[tokio::test]
    async fn compilation_delay_does_not_count_towards_max_total_time() {
        let jdwp_server = MockJdwpServer::spawn().await.unwrap();
        let jdwp = JdwpClient::connect(jdwp_server.addr()).await.unwrap();
        let cancel = CancellationToken::new();
        let cfg = StreamDebugConfig::default(); // 250ms max_total_time
        let chain = analyze_stream_expression("list.stream().map(x -> x).count()").unwrap();

        // Simulate a slow `javac` compilation (> default max_total_time).
        let result = debug_stream_wire_with_compiler(
            &jdwp,
            &chain,
            &cfg,
            &cancel,
            &TestCompiler {
                delay: Duration::from_millis(300),
            },
        )
        .await;

        assert!(result.is_ok(), "expected success, got {result:?}");
    }

    #[tokio::test]
    async fn evaluation_timeout_does_not_wait_for_delayed_jdwp_reply() {
        let jdwp_server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            delayed_replies: vec![DelayedReply {
                command_set: 3,
                command: 3, // ClassType.InvokeMethod
                delay: Duration::from_millis(300),
            }],
            ..Default::default()
        })
        .await
        .unwrap();

        let jdwp = JdwpClient::connect(jdwp_server.addr()).await.unwrap();
        let cancel = CancellationToken::new();
        let chain = analyze_stream_expression("list.stream().map(x -> x).count()").unwrap();
        let cfg = StreamDebugConfig {
            max_total_time: Duration::from_millis(50),
            ..StreamDebugConfig::default()
        };

        let result = debug_stream_wire_with_compiler(
            &jdwp,
            &chain,
            &cfg,
            &cancel,
            &TestCompiler {
                delay: Duration::from_millis(0),
            },
        )
        .await;

        match result {
            Err(WireStreamDebugError::Timeout) => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_aborts_compilation() {
        let jdwp_server = MockJdwpServer::spawn().await.unwrap();
        let jdwp = JdwpClient::connect(jdwp_server.addr()).await.unwrap();

        let cancel = CancellationToken::new();
        let chain = analyze_stream_expression("list.stream().map(x -> x).count()").unwrap();
        let cfg = StreamDebugConfig::default();

        let compiler = TestCompiler {
            delay: Duration::from_millis(200),
        };

        let handle = {
            let jdwp = jdwp.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                debug_stream_wire_with_compiler(&jdwp, &chain, &cfg, &cancel, &compiler).await
            })
        };

        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();

        let result = handle.await.unwrap();
        match result {
            Err(WireStreamDebugError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_aborts_evaluation() {
        let jdwp_server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            delayed_replies: vec![DelayedReply {
                command_set: 3,
                command: 3, // ClassType.InvokeMethod
                delay: Duration::from_secs(1),
            }],
            ..Default::default()
        })
        .await
        .unwrap();

        let jdwp = JdwpClient::connect(jdwp_server.addr()).await.unwrap();

        let cancel = CancellationToken::new();
        let chain = analyze_stream_expression("list.stream().map(x -> x).count()").unwrap();
        let cfg = StreamDebugConfig {
            max_total_time: Duration::from_secs(5),
            ..StreamDebugConfig::default()
        };
        let compiler = TestCompiler {
            delay: Duration::from_millis(0),
        };

        let handle = {
            let jdwp = jdwp.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                debug_stream_wire_with_compiler(&jdwp, &chain, &cfg, &cancel, &compiler).await
            })
        };

        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();

        let result = handle.await.unwrap();
        match result {
            Err(WireStreamDebugError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }
}
