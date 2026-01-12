use std::sync::OnceLock;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use nova_jdwp::{FrameId, JdwpClient, JdwpError, JdwpValue, ObjectKindPreview};
use nova_types::{format_type, PrimitiveType, Type, TypeStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamValueKind {
    Stream,
    IntStream,
    LongStream,
    DoubleStream,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StreamSource {
    Collection {
        collection_expr: String,
        stream_expr: String,
        method: String,
    },
    StaticFactory {
        class_expr: String,
        stream_expr: String,
        method: String,
    },
    ExistingStream {
        stream_expr: String,
    },
}

impl StreamSource {
    pub fn stream_expr(&self) -> &str {
        match self {
            StreamSource::Collection { stream_expr, .. } => stream_expr,
            StreamSource::StaticFactory { stream_expr, .. } => stream_expr,
            StreamSource::ExistingStream { stream_expr } => stream_expr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamOperationKind {
    Map,
    Filter,
    FlatMap,
    Sorted,
    Distinct,
    Limit,
    Peek,
    Collect,
    Count,
    ForEach,
    Reduce,
    Unknown,
}

impl StreamOperationKind {
    fn from_method(name: &str) -> Self {
        match name {
            "map" => Self::Map,
            "filter" => Self::Filter,
            "flatMap" => Self::FlatMap,
            "sorted" => Self::Sorted,
            "distinct" => Self::Distinct,
            "limit" => Self::Limit,
            "peek" => Self::Peek,
            "collect" => Self::Collect,
            "count" => Self::Count,
            "forEach" => Self::ForEach,
            // Treat `forEachOrdered` as the same kind of terminal side-effecting operation.
            // This keeps the operation taxonomy stable while ensuring void terminals compile
            // correctly in injected helper methods.
            "forEachOrdered" => Self::ForEach,
            "reduce" => Self::Reduce,
            _ => Self::Unknown,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Collect | Self::Count | Self::ForEach | Self::Reduce
        )
    }

    fn is_side_effecting(self) -> bool {
        matches!(self, Self::Peek | Self::ForEach)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedMethod {
    receiver: Type,
    name: String,
    arg_count: usize,
    return_type: Type,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedMethodInfo {
    pub receiver: String,
    pub name: String,
    pub arg_count: usize,
    pub return_type: String,
}

impl From<&ResolvedMethod> for ResolvedMethodInfo {
    fn from(value: &ResolvedMethod) -> Self {
        Self {
            receiver: type_to_string(&value.receiver),
            name: value.name.clone(),
            arg_count: value.arg_count,
            return_type: type_to_string(&value.return_type),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamOperation {
    pub name: String,
    pub kind: StreamOperationKind,
    pub call_source: String,
    pub arg_count: usize,
    pub expr: String,
    pub resolved: Option<ResolvedMethodInfo>,
}

impl StreamOperation {
    pub fn is_terminal(&self) -> bool {
        self.kind.is_terminal()
    }

    pub fn is_side_effecting(&self) -> bool {
        self.kind.is_side_effecting()
    }

    fn returns_void(&self) -> bool {
        // Prefer the resolved return type when available so future void-returning
        // operations (beyond `forEach`) are handled automatically.
        self.resolved
            .as_ref()
            .is_some_and(|resolved| resolved.return_type == "void")
            || matches!(self.kind, StreamOperationKind::ForEach)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamChain {
    pub expression: String,
    pub stream_kind: StreamValueKind,
    pub source: StreamSource,
    pub intermediates: Vec<StreamOperation>,
    pub terminal: Option<StreamOperation>,
}

#[derive(Debug, Error)]
pub enum StreamAnalysisError {
    #[error("empty expression")]
    EmptyExpression,
    #[error("unbalanced parentheses in expression")]
    UnbalancedParens,
    #[error("expression does not contain a stream pipeline")]
    NoStreamPipeline,
}

pub fn analyze_stream_expression(expr: &str) -> Result<StreamChain, StreamAnalysisError> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err(StreamAnalysisError::EmptyExpression);
    }

    let expr = expr.strip_suffix(';').unwrap_or(expr).trim();
    let dotted = parse_dotted_expr(expr)?;
    analyze_dotted_expr(expr, &dotted)
}

fn analyze_dotted_expr(
    expr: &str,
    dotted: &DottedExpr,
) -> Result<StreamChain, StreamAnalysisError> {
    fn stream_kind_from_class_expr(class_expr: &str) -> Option<StreamValueKind> {
        if class_expr.ends_with("IntStream") {
            Some(StreamValueKind::IntStream)
        } else if class_expr.ends_with("LongStream") {
            Some(StreamValueKind::LongStream)
        } else if class_expr.ends_with("DoubleStream") {
            Some(StreamValueKind::DoubleStream)
        } else if class_expr.ends_with("Stream") {
            Some(StreamValueKind::Stream)
        } else {
            None
        }
    }

    if dotted.segments.is_empty() {
        return Err(StreamAnalysisError::NoStreamPipeline);
    }

    let mut stream_kind = StreamValueKind::Stream;
    let mut source_end = None::<usize>;
    let mut source = None::<StreamSource>;

    for (idx, seg) in dotted.segments.iter().enumerate() {
        let Some((name, args)) = seg.as_call() else {
            continue;
        };
        let arg_count = args.len();

        match name {
            "stream" | "parallelStream" if arg_count == 0 && idx > 0 => {
                source_end = Some(idx);
                source = Some(StreamSource::Collection {
                    collection_expr: dotted.prefix_source(idx - 1),
                    stream_expr: dotted.prefix_source(idx),
                    method: name.to_string(),
                });
                stream_kind = StreamValueKind::Stream;
                break;
            }
            // `Arrays.stream(int[])` / `Arrays.stream(long[])` / `Arrays.stream(double[])` returns
            // the corresponding primitive stream type. Without this, we'd classify the source as
            // `ExistingStream` and default to `Stream`, which picks the wrong sampling suffix.
            //
            // We only attempt primitive inference for obvious array literals (e.g.
            // `new long[]{...}`) since we don't have type information for arbitrary identifiers.
            "stream" if idx > 0 && matches!(arg_count, 1 | 3) => {
                let class_expr = dotted.prefix_source(idx - 1);
                if class_expr.ends_with("Arrays") {
                    let kind = args.first().and_then(|arg| {
                        let normalized: String =
                            arg.chars().filter(|ch| !ch.is_whitespace()).collect();
                        if normalized.starts_with("newint[]") {
                            Some(StreamValueKind::IntStream)
                        } else if normalized.starts_with("newlong[]") {
                            Some(StreamValueKind::LongStream)
                        } else if normalized.starts_with("newdouble[]") {
                            Some(StreamValueKind::DoubleStream)
                        } else {
                            None
                        }
                    });

                    if let Some(kind) = kind {
                        source_end = Some(idx);
                        source = Some(StreamSource::StaticFactory {
                            class_expr,
                            stream_expr: dotted.prefix_source(idx),
                            method: name.to_string(),
                        });
                        stream_kind = kind;
                        break;
                    }
                }
            }
            "of" if idx > 0 => {
                let class_expr = dotted.prefix_source(idx - 1);
                let kind = stream_kind_from_class_expr(&class_expr);

                if let Some(kind) = kind {
                    source_end = Some(idx);
                    source = Some(StreamSource::StaticFactory {
                        class_expr,
                        stream_expr: dotted.prefix_source(idx),
                        method: name.to_string(),
                    });
                    stream_kind = kind;
                    break;
                }
            }
            "range" | "rangeClosed" if idx > 0 && arg_count == 2 => {
                let class_expr = dotted.prefix_source(idx - 1);
                let kind = stream_kind_from_class_expr(&class_expr).filter(|k| {
                    matches!(*k, StreamValueKind::IntStream | StreamValueKind::LongStream)
                });

                if let Some(kind) = kind {
                    source_end = Some(idx);
                    source = Some(StreamSource::StaticFactory {
                        class_expr,
                        stream_expr: dotted.prefix_source(idx),
                        method: name.to_string(),
                    });
                    stream_kind = kind;
                    break;
                }
            }
            "iterate" if idx > 0 && matches!(arg_count, 2 | 3) => {
                let class_expr = dotted.prefix_source(idx - 1);
                let kind = stream_kind_from_class_expr(&class_expr);

                if let Some(kind) = kind {
                    source_end = Some(idx);
                    source = Some(StreamSource::StaticFactory {
                        class_expr,
                        stream_expr: dotted.prefix_source(idx),
                        method: name.to_string(),
                    });
                    stream_kind = kind;
                    break;
                }
            }
            "generate" if idx > 0 && arg_count == 1 => {
                let class_expr = dotted.prefix_source(idx - 1);
                let kind = stream_kind_from_class_expr(&class_expr);

                if let Some(kind) = kind {
                    source_end = Some(idx);
                    source = Some(StreamSource::StaticFactory {
                        class_expr,
                        stream_expr: dotted.prefix_source(idx),
                        method: name.to_string(),
                    });
                    stream_kind = kind;
                    break;
                }
            }
            "empty" if idx > 0 && arg_count == 0 => {
                let class_expr = dotted.prefix_source(idx - 1);
                let kind = stream_kind_from_class_expr(&class_expr);

                if let Some(kind) = kind {
                    source_end = Some(idx);
                    source = Some(StreamSource::StaticFactory {
                        class_expr,
                        stream_expr: dotted.prefix_source(idx),
                        method: name.to_string(),
                    });
                    stream_kind = kind;
                    break;
                }
            }
            "concat" if idx > 0 && arg_count == 2 => {
                let class_expr = dotted.prefix_source(idx - 1);
                let kind = stream_kind_from_class_expr(&class_expr);

                if let Some(kind) = kind {
                    source_end = Some(idx);
                    source = Some(StreamSource::StaticFactory {
                        class_expr,
                        stream_expr: dotted.prefix_source(idx),
                        method: name.to_string(),
                    });
                    stream_kind = kind;
                    break;
                }
            }
            _ => {}
        }
    }

    let source_end = if let Some(source_end) = source_end {
        source_end
    } else {
        let first_op = dotted
            .segments
            .iter()
            .enumerate()
            .find_map(|(idx, seg)| {
                let (name, _) = seg.as_call()?;
                let kind = StreamOperationKind::from_method(name);
                (kind != StreamOperationKind::Unknown).then_some(idx)
            })
            .ok_or(StreamAnalysisError::NoStreamPipeline)?;

        if first_op == 0 {
            return Err(StreamAnalysisError::NoStreamPipeline);
        }

        source = Some(StreamSource::ExistingStream {
            stream_expr: dotted.prefix_source(first_op - 1),
        });
        first_op - 1
    };

    let source = source.ok_or(StreamAnalysisError::NoStreamPipeline)?;

    let mut receiver = stream_receiver_type(stream_kind);
    let mut intermediates = Vec::new();
    let mut terminal = None;

    for (idx, seg) in dotted.segments.iter().enumerate().skip(source_end + 1) {
        let Some((name, args)) = seg.as_call() else {
            continue;
        };
        let arg_count = args.len();
        let kind = StreamOperationKind::from_method(name);
        if kind == StreamOperationKind::Unknown {
            break;
        }

        let Some(resolved) = resolve_stream_method(&receiver, name, arg_count) else {
            break;
        };

        let op = StreamOperation {
            name: name.to_string(),
            kind,
            call_source: seg.source.clone(),
            arg_count,
            expr: dotted.prefix_source(idx),
            resolved: Some(ResolvedMethodInfo::from(&resolved)),
        };

        receiver = resolved.return_type.clone();

        if op.is_terminal() {
            terminal = Some(op);
            break;
        }

        intermediates.push(op);
    }

    Ok(StreamChain {
        expression: expr.to_string(),
        stream_kind,
        source,
        intermediates,
        terminal,
    })
}

fn resolve_stream_method(receiver: &Type, name: &str, arg_count: usize) -> Option<ResolvedMethod> {
    let receiver_name = match receiver {
        Type::Named(name) => name.as_str(),
        _ => return None,
    };

    let (return_type, ok) = match receiver_name {
        "java.util.stream.Stream" => (resolve_stream_method_stream(name, arg_count)?, true),
        "java.util.stream.IntStream" => (resolve_stream_method_int_stream(name, arg_count)?, true),
        "java.util.stream.LongStream" => {
            (resolve_stream_method_long_stream(name, arg_count)?, true)
        }
        "java.util.stream.DoubleStream" => {
            (resolve_stream_method_double_stream(name, arg_count)?, true)
        }
        _ => (Type::Unknown, false),
    };

    ok.then_some(ResolvedMethod {
        receiver: receiver.clone(),
        name: name.to_string(),
        arg_count,
        return_type,
    })
}

fn resolve_stream_method_stream(name: &str, arg_count: usize) -> Option<Type> {
    match (name, arg_count) {
        ("map", 1)
        | ("filter", 1)
        | ("flatMap", 1)
        | ("sorted", 0)
        | ("sorted", 1)
        | ("distinct", 0)
        | ("limit", 1)
        | ("peek", 1) => Some(Type::Named("java.util.stream.Stream".to_string())),
        ("collect", 1) | ("collect", 3) => Some(Type::Named("java.lang.Object".to_string())),
        ("count", 0) => Some(Type::Primitive(PrimitiveType::Long)),
        ("forEach", 1) => Some(Type::Void),
        ("forEachOrdered", 1) => Some(Type::Void),
        ("reduce", 1) | ("reduce", 2) | ("reduce", 3) => {
            Some(Type::Named("java.lang.Object".to_string()))
        }
        _ => None,
    }
}

fn resolve_stream_method_int_stream(name: &str, arg_count: usize) -> Option<Type> {
    match (name, arg_count) {
        ("map", 1)
        | ("filter", 1)
        | ("flatMap", 1)
        | ("sorted", 0)
        | ("distinct", 0)
        | ("limit", 1)
        | ("peek", 1) => Some(Type::Named("java.util.stream.IntStream".to_string())),
        ("count", 0) => Some(Type::Primitive(PrimitiveType::Long)),
        ("forEach", 1) | ("forEachOrdered", 1) => Some(Type::Void),
        _ => None,
    }
}

fn resolve_stream_method_long_stream(name: &str, arg_count: usize) -> Option<Type> {
    match (name, arg_count) {
        ("map", 1)
        | ("filter", 1)
        | ("flatMap", 1)
        | ("sorted", 0)
        | ("distinct", 0)
        | ("limit", 1)
        | ("peek", 1) => Some(Type::Named("java.util.stream.LongStream".to_string())),
        ("count", 0) => Some(Type::Primitive(PrimitiveType::Long)),
        ("forEach", 1) | ("forEachOrdered", 1) => Some(Type::Void),
        _ => None,
    }
}

fn resolve_stream_method_double_stream(name: &str, arg_count: usize) -> Option<Type> {
    match (name, arg_count) {
        ("map", 1)
        | ("filter", 1)
        | ("flatMap", 1)
        | ("sorted", 0)
        | ("distinct", 0)
        | ("limit", 1)
        | ("peek", 1) => Some(Type::Named("java.util.stream.DoubleStream".to_string())),
        ("count", 0) => Some(Type::Primitive(PrimitiveType::Long)),
        ("forEach", 1) | ("forEachOrdered", 1) => Some(Type::Void),
        _ => None,
    }
}

fn stream_receiver_type(kind: StreamValueKind) -> Type {
    match kind {
        StreamValueKind::Stream => Type::Named("java.util.stream.Stream".to_string()),
        StreamValueKind::IntStream => Type::Named("java.util.stream.IntStream".to_string()),
        StreamValueKind::LongStream => Type::Named("java.util.stream.LongStream".to_string()),
        StreamValueKind::DoubleStream => Type::Named("java.util.stream.DoubleStream".to_string()),
    }
}

fn type_to_string(ty: &Type) -> String {
    static ENV: OnceLock<TypeStore> = OnceLock::new();
    let env = ENV.get_or_init(TypeStore::with_minimal_jdk);
    format_type(env, ty)
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct StreamDebugConfig {
    pub max_sample_size: usize,
    pub max_total_time: Duration,
    pub allow_side_effects: bool,
    pub allow_terminal_ops: bool,
}

impl Default for StreamDebugConfig {
    fn default() -> Self {
        Self {
            max_sample_size: 25,
            max_total_time: Duration::from_millis(250),
            allow_side_effects: false,
            allow_terminal_ops: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSample {
    pub elements: Vec<String>,
    pub truncated: bool,
    pub element_type: Option<String>,
    pub collection_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStepResult {
    pub operation: String,
    pub kind: StreamOperationKind,
    pub executed: bool,
    pub input: StreamSample,
    pub output: StreamSample,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamTerminalResult {
    pub operation: String,
    pub kind: StreamOperationKind,
    pub executed: bool,
    pub value: Option<String>,
    pub type_name: Option<String>,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDebugResult {
    pub expression: String,
    pub source: StreamSource,
    pub source_sample: StreamSample,
    pub source_duration_ms: u128,
    pub steps: Vec<StreamStepResult>,
    pub terminal: Option<StreamTerminalResult>,
    pub total_duration_ms: u128,
}

#[derive(Debug, Error)]
pub enum StreamDebugError {
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
    #[error("evaluation exceeded time limit")]
    Timeout,
    #[error(transparent)]
    Jdwp(#[from] JdwpError),
    #[error("expected collection result from evaluation")]
    ExpectedCollection,
}

pub fn debug_stream<C: JdwpClient>(
    jdwp: &mut C,
    frame_id: FrameId,
    chain: &StreamChain,
    config: &StreamDebugConfig,
    cancel: &CancellationToken,
) -> Result<StreamDebugResult, StreamDebugError> {
    let started = Instant::now();

    let enforce_limits = || {
        if cancel.is_cancelled() {
            return Err(StreamDebugError::Cancelled);
        }
        if started.elapsed() > config.max_total_time {
            return Err(StreamDebugError::Timeout);
        }
        Ok(())
    };

    enforce_limits()?;

    if let StreamSource::ExistingStream { stream_expr } = &chain.source {
        // `ExistingStream` can be either:
        // - an actual, already-instantiated Stream value (unsafe: sampling consumes it), or
        // - a stream-producing expression (usually safe to re-evaluate, e.g. `Arrays.stream(arr)`).
        //
        // Heuristic: if the source expression has no call segments, treat it as an existing stream
        // value and refuse by default.
        if is_pure_access_expr(stream_expr) {
            return Err(StreamDebugError::UnsafeExistingStream {
                stream_expr: stream_expr.clone(),
            });
        }
    }

    let mut safe_expr = chain.source.stream_expr().to_string();
    let sample_eval_expr = |stream_expr: &str| {
        match &chain.source {
        // `ExistingStream` sources are inherently hard to type-check: without type information we
        // can't reliably tell whether expressions like `Arrays.stream(arr)` return a `Stream` or
        // a primitive stream (e.g. `LongStream` for `long[]`).
        //
        // Use a `BaseStream.spliterator()` -> `StreamSupport.stream(...)` bridge to collect
        // *any* stream type into a `List` without needing `.boxed()` or stream-specific `collect`
        // overloads.
        StreamSource::ExistingStream { .. } => format!(
            "java.util.stream.StreamSupport.stream(({stream_expr}).limit({}).spliterator(), false).collect(java.util.stream.Collectors.toList())",
            config.max_sample_size
        ),
        _ => format!(
            "{stream_expr}{}",
            sample_suffix(chain.stream_kind, config.max_sample_size)
        ),
    }
    };

    let source_eval_expr = sample_eval_expr(&safe_expr);
    let (source_sample, source_duration_ms) =
        timed(|| eval_sample(jdwp, frame_id, &source_eval_expr))?;

    let mut last_sample = source_sample.clone();
    let mut steps = Vec::new();

    for op in &chain.intermediates {
        enforce_limits()?;

        if op.is_side_effecting() && !config.allow_side_effects {
            steps.push(StreamStepResult {
                operation: op.name.clone(),
                kind: op.kind,
                executed: false,
                input: last_sample.clone(),
                output: last_sample.clone(),
                duration_ms: 0,
            });
            continue;
        }

        safe_expr = format!("{safe_expr}.{}", op.call_source);
        let eval_expr = sample_eval_expr(&safe_expr);
        let (output, duration_ms) = timed(|| eval_sample(jdwp, frame_id, &eval_expr))?;

        steps.push(StreamStepResult {
            operation: op.name.clone(),
            kind: op.kind,
            executed: true,
            input: last_sample.clone(),
            output: output.clone(),
            duration_ms,
        });
        last_sample = output;
    }

    let terminal = if let Some(term) = &chain.terminal {
        if !config.allow_terminal_ops || (term.is_side_effecting() && !config.allow_side_effects) {
            Some(StreamTerminalResult {
                operation: term.name.clone(),
                kind: term.kind,
                executed: false,
                value: None,
                type_name: None,
                duration_ms: 0,
            })
        } else {
            enforce_limits()?;
            let eval_expr = format!(
                "{safe_expr}.limit({}).{}",
                config.max_sample_size, term.call_source
            );

            // Some terminal operations (e.g. `forEach`) return `void`. The underlying
            // evaluator often expects value-returning expressions and may wrap the
            // expression in an `Object`-returning helper method (e.g. `return <expr>;`),
            // which fails to compile for `void` expressions.
            //
            // To keep side effects while satisfying value-returning evaluators, execute
            // the `void` expression inside an `Object`-returning wrapper and then
            // surface a legacy `"void"` terminal result to callers.
            let (value, type_name, duration_ms) = if term.returns_void() {
                let wrapped = wrap_void_expression(&eval_expr);
                let (_, duration_ms) = timed(|| eval_void(jdwp, frame_id, &wrapped))?;
                (
                    Some("void".to_string()),
                    Some("void".to_string()),
                    duration_ms,
                )
            } else {
                let (value, duration_ms) = timed(|| eval_scalar(jdwp, frame_id, &eval_expr))?;
                (Some(value.display), value.type_name, duration_ms)
            };
            Some(StreamTerminalResult {
                operation: term.name.clone(),
                kind: term.kind,
                executed: true,
                value,
                type_name,
                duration_ms,
            })
        }
    } else {
        None
    };

    Ok(StreamDebugResult {
        expression: chain.expression.clone(),
        source: chain.source.clone(),
        source_sample,
        source_duration_ms,
        steps,
        terminal,
        total_duration_ms: started.elapsed().as_millis(),
    })
}

fn sample_suffix(stream_kind: StreamValueKind, max: usize) -> String {
    match stream_kind {
        StreamValueKind::Stream => format!(
            ".limit({}).collect(java.util.stream.Collectors.toList())",
            max
        ),
        StreamValueKind::IntStream
        | StreamValueKind::LongStream
        | StreamValueKind::DoubleStream => {
            format!(
                ".limit({}).boxed().collect(java.util.stream.Collectors.toList())",
                max
            )
        }
    }
}

fn eval_sample<C: JdwpClient>(
    jdwp: &mut C,
    frame_id: FrameId,
    expression: &str,
) -> Result<StreamSample, StreamDebugError> {
    let value = jdwp.evaluate(expression, frame_id)?;
    let obj = match value {
        JdwpValue::Object(obj) => obj,
        _ => return Err(StreamDebugError::ExpectedCollection),
    };

    let preview = jdwp.preview_object(obj.id)?;
    let (raw_elements, size, collection_type) = match preview.kind {
        ObjectKindPreview::List { size, sample } => (sample, size, preview.runtime_type),
        ObjectKindPreview::Set { size, sample } => (sample, size, preview.runtime_type),
        ObjectKindPreview::Array { length, sample, .. } => (sample, length, preview.runtime_type),
        _ => return Err(StreamDebugError::ExpectedCollection),
    };

    let truncated = raw_elements.len() < size;
    let mut element_type: Option<String> = None;
    let mut elements = Vec::with_capacity(raw_elements.len());
    for v in &raw_elements {
        let (display, ty) = format_sample_value(jdwp, v);
        elements.push(display);
        if element_type.is_none() {
            element_type = ty;
        }
    }

    Ok(StreamSample {
        elements,
        truncated,
        element_type,
        collection_type: Some(collection_type),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScalarPreview {
    display: String,
    type_name: Option<String>,
}

fn eval_scalar<C: JdwpClient>(
    jdwp: &mut C,
    frame_id: FrameId,
    expression: &str,
) -> Result<ScalarPreview, StreamDebugError> {
    let value = jdwp.evaluate(expression, frame_id)?;
    Ok(ScalarPreview {
        display: format_value(jdwp, &value),
        type_name: Some(type_name_for_value(&value)),
    })
}

fn eval_void<C: JdwpClient>(
    jdwp: &mut C,
    frame_id: FrameId,
    expression: &str,
) -> Result<(), StreamDebugError> {
    let _ = jdwp.evaluate(expression, frame_id)?;
    Ok(())
}

fn wrap_void_expression(expr: &str) -> String {
    // Use a lambda-based wrapper so `<expr>` can remain a statement expression while the
    // whole evaluation becomes an `Object` expression.
    //
    // Example:
    //   ((Supplier<Object>)(() -> { <expr>; return null; })).get()
    //
    // This is intentionally fully-qualified to avoid relying on imports in the
    // evaluator's compilation context.
    format!("((java.util.function.Supplier<Object>)(() -> {{{expr};return null;}})).get()")
}

fn format_sample_value<C: JdwpClient>(jdwp: &mut C, value: &JdwpValue) -> (String, Option<String>) {
    match value {
        JdwpValue::Null => ("null".to_string(), None),
        JdwpValue::Void => ("void".to_string(), Some("void".to_string())),
        JdwpValue::Boolean(v) => (v.to_string(), Some("boolean".to_string())),
        JdwpValue::Byte(v) => (v.to_string(), Some("byte".to_string())),
        JdwpValue::Short(v) => (v.to_string(), Some("short".to_string())),
        JdwpValue::Int(v) => (v.to_string(), Some("int".to_string())),
        JdwpValue::Long(v) => (v.to_string(), Some("long".to_string())),
        JdwpValue::Float(v) => (v.to_string(), Some("float".to_string())),
        JdwpValue::Double(v) => (v.to_string(), Some("double".to_string())),
        JdwpValue::Char(v) => (v.to_string(), Some("char".to_string())),
        JdwpValue::Object(obj) => {
            // For list samples, object values frequently have a placeholder runtime type (e.g.
            // `java.lang.Object`). Use `preview_object` to recover a more useful type name and
            // unwrap boxed primitives.
            if let Ok(preview) = jdwp.preview_object(obj.id) {
                match preview.kind {
                    ObjectKindPreview::String { value } => {
                        return (value, Some(preview.runtime_type));
                    }
                    ObjectKindPreview::PrimitiveWrapper { value } => {
                        return format_sample_value(jdwp, &value);
                    }
                    ObjectKindPreview::Optional { value } => {
                        let display = match value {
                            None => "Optional.empty".to_string(),
                            Some(v) => format!("Optional[{}]", format_value(jdwp, &v)),
                        };
                        return (display, Some(preview.runtime_type));
                    }
                    _ => {
                        return (
                            format!("{}#{}", preview.runtime_type, obj.id),
                            Some(preview.runtime_type),
                        );
                    }
                }
            }

            (
                format!("{}#{}", obj.runtime_type, obj.id),
                Some(obj.runtime_type.clone()),
            )
        }
    }
}

fn type_name_for_value(value: &JdwpValue) -> String {
    match value {
        JdwpValue::Null => "null".to_string(),
        JdwpValue::Void => "void".to_string(),
        JdwpValue::Boolean(_) => "boolean".to_string(),
        JdwpValue::Byte(_) => "byte".to_string(),
        JdwpValue::Short(_) => "short".to_string(),
        JdwpValue::Int(_) => "int".to_string(),
        JdwpValue::Long(_) => "long".to_string(),
        JdwpValue::Float(_) => "float".to_string(),
        JdwpValue::Double(_) => "double".to_string(),
        JdwpValue::Char(_) => "char".to_string(),
        JdwpValue::Object(obj) => obj.runtime_type.clone(),
    }
}

fn format_value<C: JdwpClient>(jdwp: &mut C, value: &JdwpValue) -> String {
    match value {
        JdwpValue::Null => "null".to_string(),
        JdwpValue::Void => "void".to_string(),
        JdwpValue::Boolean(v) => v.to_string(),
        JdwpValue::Byte(v) => v.to_string(),
        JdwpValue::Short(v) => v.to_string(),
        JdwpValue::Int(v) => v.to_string(),
        JdwpValue::Long(v) => v.to_string(),
        JdwpValue::Float(v) => v.to_string(),
        JdwpValue::Double(v) => v.to_string(),
        JdwpValue::Char(v) => v.to_string(),
        JdwpValue::Object(obj) => {
            // Best-effort: render user-friendly values for common JDK wrappers.
            if let Ok(preview) = jdwp.preview_object(obj.id) {
                match preview.kind {
                    ObjectKindPreview::String { value } => return value,
                    ObjectKindPreview::PrimitiveWrapper { value } => {
                        return format_value(jdwp, &value)
                    }
                    ObjectKindPreview::Optional { value } => {
                        return match value {
                            None => "Optional.empty".to_string(),
                            Some(v) => format!("Optional[{}]", format_value(jdwp, &v)),
                        };
                    }
                    _ => return format!("{}#{}", preview.runtime_type, obj.id),
                }
            }

            format!("{}#{}", obj.runtime_type, obj.id)
        }
    }
}

fn timed<T, E>(f: impl FnOnce() -> Result<T, E>) -> Result<(T, u128), E> {
    let start = Instant::now();
    let value = f()?;
    Ok((value, start.elapsed().as_millis()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DottedExpr {
    segments: Vec<Segment>,
}

impl DottedExpr {
    fn prefix_source(&self, end: usize) -> String {
        self.segments
            .iter()
            .take(end + 1)
            .map(|s| s.source.as_str())
            .collect::<Vec<_>>()
            .join(".")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SegmentKind {
    Access,
    Call { name: String, args: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    source: String,
    kind: SegmentKind,
}

impl Segment {
    fn as_call(&self) -> Option<(&str, &[String])> {
        match &self.kind {
            SegmentKind::Call { name, args } => Some((name.as_str(), args.as_slice())),
            _ => None,
        }
    }
}

fn parse_dotted_expr(source: &str) -> Result<DottedExpr, StreamAnalysisError> {
    let raw_segments = split_top_level(source, '.')?;
    let mut segments = Vec::with_capacity(raw_segments.len());

    for raw in raw_segments {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }

        if let Some((name, args)) = parse_call_segment(raw)? {
            segments.push(Segment {
                source: raw.to_string(),
                kind: SegmentKind::Call { name, args },
            });
        } else {
            segments.push(Segment {
                source: raw.to_string(),
                kind: SegmentKind::Access,
            });
        }
    }

    Ok(DottedExpr { segments })
}

fn parse_call_segment(raw: &str) -> Result<Option<(String, Vec<String>)>, StreamAnalysisError> {
    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    let mut depth_brace = 0usize;
    let mut in_str = false;
    let mut in_char = false;
    let mut escape = false;
    let mut open_paren_idx = None;

    for (i, ch) in raw.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_str {
            if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if in_char {
            if ch == '\\' {
                escape = true;
            } else if ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => in_str = true,
            '\'' => in_char = true,
            '(' => {
                if depth_paren == 0 && depth_bracket == 0 && depth_brace == 0 {
                    open_paren_idx = Some(i);
                    break;
                }
                depth_paren += 1;
            }
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            _ => {}
        }
    }

    let Some(open_paren_idx) = open_paren_idx else {
        return Ok(None);
    };

    let (head, tail) = raw.split_at(open_paren_idx);
    let mut head = head.trim();

    if let Some(stripped) = head.strip_prefix('<') {
        let (_, rest) = split_angle_args(stripped)?;
        head = rest.trim();
    }

    let name = head.to_string();
    if name.is_empty() {
        return Err(StreamAnalysisError::UnbalancedParens);
    }

    let args_raw = extract_matching_parens(tail).ok_or(StreamAnalysisError::UnbalancedParens)?;
    let args = split_top_level(args_raw, ',')?
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(Some((name, args)))
}

fn split_top_level(source: &str, delim: char) -> Result<Vec<&str>, StreamAnalysisError> {
    let mut parts = Vec::new();
    let mut start = 0usize;

    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    let mut depth_brace = 0usize;
    let mut depth_angle = 0usize;

    let mut in_str = false;
    let mut in_char = false;
    let mut escape = false;

    for (i, ch) in source.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_str {
            if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if in_char {
            if ch == '\\' {
                escape = true;
            } else if ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => in_str = true,
            '\'' => in_char = true,
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            '<' => depth_angle += 1,
            '>' => depth_angle = depth_angle.saturating_sub(1),
            _ => {}
        }

        if ch == delim
            && depth_paren == 0
            && depth_bracket == 0
            && depth_brace == 0
            && depth_angle == 0
            && !in_str
            && !in_char
        {
            parts.push(&source[start..i]);
            start = i + ch.len_utf8();
        }
    }

    if depth_paren != 0 || depth_bracket != 0 || depth_brace != 0 {
        return Err(StreamAnalysisError::UnbalancedParens);
    }

    parts.push(&source[start..]);
    Ok(parts)
}

fn extract_matching_parens(tail: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut in_char = false;
    let mut escape = false;

    let mut start = None;
    for (i, ch) in tail.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_str {
            if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if in_char {
            if ch == '\\' {
                escape = true;
            } else if ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => in_str = true,
            '\'' => in_char = true,
            '(' => {
                if depth == 0 {
                    start = Some(i + 1);
                }
                depth += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let start = start?;
                    return Some(&tail[start..i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_angle_args(source: &str) -> Result<(&str, &str), StreamAnalysisError> {
    let mut depth_angle = 1usize;
    let mut in_str = false;
    let mut in_char = false;
    let mut escape = false;

    for (i, ch) in source.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_str {
            if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if in_char {
            if ch == '\\' {
                escape = true;
            } else if ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => in_str = true,
            '\'' => in_char = true,
            '<' => depth_angle += 1,
            '>' => {
                depth_angle = depth_angle.saturating_sub(1);
                if depth_angle == 0 {
                    let inside = &source[..i];
                    let rest = &source[i + 1..];
                    return Ok((inside, rest));
                }
            }
            _ => {}
        }
    }

    Err(StreamAnalysisError::UnbalancedParens)
}

/// Returns `true` if the given expression looks like a pure access path (no call segments).
///
/// This is a best-effort heuristic used to distinguish:
/// - existing stream *values* (`streamVar`, `obj.streamField`, `streams[idx]`) which are unsafe to
///   sample (iterating consumes them), from
/// - stream-producing expressions (`Arrays.stream(arr)`, `Stream.of(...)`, `supplier.get()`) which
///   are usually safe to re-evaluate.
///
/// When the expression cannot be parsed confidently, this returns `true` to err on the side of
/// safety.
pub fn is_pure_access_expr(expr: &str) -> bool {
    match parse_dotted_expr(expr) {
        Ok(dotted) => dotted
            .segments
            .iter()
            .all(|seg| matches!(seg.kind, SegmentKind::Access)),
        // If we can't parse confidently, err on the side of safety (treat as a value).
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_jdwp::{MockJdwpClient, MockObject, ObjectPreview, ObjectRef};

    #[test]
    fn extracts_collection_stream_chain() {
        let expr = "list.stream().filter(x -> x > 0).map(x -> x * 2).collect(Collectors.toList())";
        let chain = analyze_stream_expression(expr).unwrap();

        match &chain.source {
            StreamSource::Collection {
                collection_expr,
                stream_expr,
                method,
            } => {
                assert_eq!(collection_expr, "list");
                assert_eq!(stream_expr, "list.stream()");
                assert_eq!(method, "stream");
            }
            _ => panic!("expected collection source"),
        }

        assert_eq!(chain.intermediates.len(), 2);
        assert_eq!(chain.intermediates[0].kind, StreamOperationKind::Filter);
        assert_eq!(chain.intermediates[1].kind, StreamOperationKind::Map);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Collect
        );
    }

    #[test]
    fn recognizes_long_stream_chain() {
        let expr = "LongStream.range(0, 10).map(x -> x + 1).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        match &chain.source {
            StreamSource::StaticFactory {
                class_expr,
                stream_expr,
                method,
            } => {
                assert_eq!(class_expr, "LongStream");
                assert_eq!(stream_expr, "LongStream.range(0, 10)");
                assert_eq!(method, "range");
            }
            _ => panic!("expected static factory source"),
        }

        assert_eq!(chain.stream_kind, StreamValueKind::LongStream);
        assert_eq!(chain.intermediates.len(), 1);
        assert_eq!(chain.intermediates[0].kind, StreamOperationKind::Map);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn recognizes_arrays_stream_long_array_chain() {
        let expr = "java.util.Arrays.stream(new long[]{1, 2}).map(x -> x + 1).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        match &chain.source {
            StreamSource::StaticFactory {
                class_expr,
                stream_expr,
                method,
            } => {
                assert_eq!(class_expr, "java.util.Arrays");
                assert_eq!(stream_expr, "java.util.Arrays.stream(new long[]{1, 2})");
                assert_eq!(method, "stream");
            }
            _ => panic!("expected static factory source"),
        }

        assert_eq!(chain.stream_kind, StreamValueKind::LongStream);
        assert_eq!(chain.intermediates.len(), 1);
        assert_eq!(chain.intermediates[0].kind, StreamOperationKind::Map);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn recognizes_long_stream_sorted_chain() {
        let expr = "LongStream.range(0, 10).sorted().count()";
        let chain = analyze_stream_expression(expr).unwrap();

        assert_eq!(chain.stream_kind, StreamValueKind::LongStream);
        assert_eq!(chain.intermediates.len(), 1);
        assert_eq!(chain.intermediates[0].kind, StreamOperationKind::Sorted);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn recognizes_long_stream_iterate_chain() {
        let expr = "LongStream.iterate(0, x -> x + 1).limit(10).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        match &chain.source {
            StreamSource::StaticFactory {
                class_expr,
                stream_expr,
                method,
            } => {
                assert_eq!(class_expr, "LongStream");
                assert_eq!(stream_expr, "LongStream.iterate(0, x -> x + 1)");
                assert_eq!(method, "iterate");
            }
            _ => panic!("expected static factory source"),
        }

        assert_eq!(chain.stream_kind, StreamValueKind::LongStream);
        assert_eq!(chain.intermediates.len(), 1);
        assert_eq!(chain.intermediates[0].kind, StreamOperationKind::Limit);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn recognizes_int_stream_sorted_chain() {
        let expr = "IntStream.range(0, 10).sorted().count()";
        let chain = analyze_stream_expression(expr).unwrap();

        assert_eq!(chain.stream_kind, StreamValueKind::IntStream);
        assert_eq!(chain.intermediates.len(), 1);
        assert_eq!(chain.intermediates[0].kind, StreamOperationKind::Sorted);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn recognizes_int_stream_empty_chain() {
        let expr = "IntStream.empty().count()";
        let chain = analyze_stream_expression(expr).unwrap();

        match &chain.source {
            StreamSource::StaticFactory {
                class_expr,
                stream_expr,
                method,
            } => {
                assert_eq!(class_expr, "IntStream");
                assert_eq!(stream_expr, "IntStream.empty()");
                assert_eq!(method, "empty");
            }
            _ => panic!("expected static factory source"),
        }

        assert_eq!(chain.stream_kind, StreamValueKind::IntStream);
        assert_eq!(chain.intermediates.len(), 0);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn recognizes_double_stream_chain() {
        let expr = "DoubleStream.of(1.0, 2.0).map(x -> x + 1).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        match &chain.source {
            StreamSource::StaticFactory {
                class_expr,
                stream_expr,
                method,
            } => {
                assert_eq!(class_expr, "DoubleStream");
                assert_eq!(stream_expr, "DoubleStream.of(1.0, 2.0)");
                assert_eq!(method, "of");
            }
            _ => panic!("expected static factory source"),
        }

        assert_eq!(chain.stream_kind, StreamValueKind::DoubleStream);
        assert_eq!(chain.intermediates.len(), 1);
        assert_eq!(chain.intermediates[0].kind, StreamOperationKind::Map);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn recognizes_double_stream_concat_chain() {
        let expr = "DoubleStream.concat(DoubleStream.of(1.0), DoubleStream.of(2.0)).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        match &chain.source {
            StreamSource::StaticFactory {
                class_expr,
                stream_expr,
                method,
            } => {
                assert_eq!(class_expr, "DoubleStream");
                assert_eq!(
                    stream_expr,
                    "DoubleStream.concat(DoubleStream.of(1.0), DoubleStream.of(2.0))"
                );
                assert_eq!(method, "concat");
            }
            _ => panic!("expected static factory source"),
        }

        assert_eq!(chain.stream_kind, StreamValueKind::DoubleStream);
        assert_eq!(chain.intermediates.len(), 0);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn recognizes_double_stream_generate_chain() {
        let expr = "DoubleStream.generate(() -> 1.0).limit(10).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        match &chain.source {
            StreamSource::StaticFactory {
                class_expr,
                stream_expr,
                method,
            } => {
                assert_eq!(class_expr, "DoubleStream");
                assert_eq!(stream_expr, "DoubleStream.generate(() -> 1.0)");
                assert_eq!(method, "generate");
            }
            _ => panic!("expected static factory source"),
        }

        assert_eq!(chain.stream_kind, StreamValueKind::DoubleStream);
        assert_eq!(chain.intermediates.len(), 1);
        assert_eq!(chain.intermediates[0].kind, StreamOperationKind::Limit);
        assert_eq!(
            chain.terminal.as_ref().unwrap().kind,
            StreamOperationKind::Count
        );
    }

    #[test]
    fn sample_suffix_boxes_primitive_streams() {
        assert_eq!(
            sample_suffix(StreamValueKind::Stream, 5),
            ".limit(5).collect(java.util.stream.Collectors.toList())"
        );
        assert_eq!(
            sample_suffix(StreamValueKind::IntStream, 5),
            ".limit(5).boxed().collect(java.util.stream.Collectors.toList())"
        );
        assert_eq!(
            sample_suffix(StreamValueKind::LongStream, 5),
            ".limit(5).boxed().collect(java.util.stream.Collectors.toList())"
        );
        assert_eq!(
            sample_suffix(StreamValueKind::DoubleStream, 5),
            ".limit(5).boxed().collect(java.util.stream.Collectors.toList())"
        );
    }

    #[test]
    fn sample_formatting_unwraps_boxed_primitive_wrappers() {
        let mut jdwp = MockJdwpClient::new();
        jdwp.set_evaluation(
            1,
            "list.stream().limit(3).collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 40,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );

        jdwp.insert_object(
            40,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 2,
                        sample: vec![
                            JdwpValue::Object(ObjectRef {
                                id: 41,
                                runtime_type: "java.lang.Object".to_string(),
                            }),
                            JdwpValue::Object(ObjectRef {
                                id: 42,
                                runtime_type: "java.lang.Object".to_string(),
                            }),
                        ],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.insert_object(
            41,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.lang.Long".to_string(),
                    kind: ObjectKindPreview::PrimitiveWrapper {
                        value: Box::new(JdwpValue::Long(1)),
                    },
                },
                children: Vec::new(),
            },
        );
        jdwp.insert_object(
            42,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.lang.Long".to_string(),
                    kind: ObjectKindPreview::PrimitiveWrapper {
                        value: Box::new(JdwpValue::Long(2)),
                    },
                },
                children: Vec::new(),
            },
        );

        let sample = eval_sample(
            &mut jdwp,
            1,
            "list.stream().limit(3).collect(java.util.stream.Collectors.toList())",
        )
        .unwrap();

        assert_eq!(sample.elements, vec!["1", "2"]);
        assert_eq!(sample.element_type.as_deref(), Some("long"));
    }

    #[test]
    fn debug_long_stream_evaluates_each_stage_with_mock_jdwp() {
        let expr = "LongStream.range(0, 10).map(x -> x + 1).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        let mut jdwp = MockJdwpClient::new();
        jdwp.set_evaluation(
            1,
            "LongStream.range(0, 10).limit(3).boxed().collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 20,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            20,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 3,
                        sample: vec![JdwpValue::Long(0), JdwpValue::Long(1), JdwpValue::Long(2)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "LongStream.range(0, 10).map(x -> x + 1).limit(3).boxed().collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 21,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            21,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 3,
                        sample: vec![JdwpValue::Long(1), JdwpValue::Long(2), JdwpValue::Long(3)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "LongStream.range(0, 10).map(x -> x + 1).limit(3).count()",
            Ok(JdwpValue::Long(3)),
        );

        let config = StreamDebugConfig {
            max_sample_size: 3,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: false,
            allow_terminal_ops: true,
        };
        let cancel = CancellationToken::default();
        let result = debug_stream(&mut jdwp, 1, &chain, &config, &cancel).unwrap();

        assert_eq!(result.source_sample.elements, vec!["0", "1", "2"]);
        assert_eq!(result.steps.len(), 1);
        assert_eq!(result.steps[0].output.elements, vec!["1", "2", "3"]);
        assert_eq!(
            result.terminal.as_ref().unwrap().value.as_deref(),
            Some("3")
        );
    }

    #[test]
    fn debug_double_stream_evaluates_each_stage_with_mock_jdwp() {
        let expr = "DoubleStream.of(1.0, 2.0).map(x -> x + 1).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        let mut jdwp = MockJdwpClient::new();
        jdwp.set_evaluation(
            1,
            "DoubleStream.of(1.0, 2.0).limit(3).boxed().collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 30,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            30,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 2,
                        sample: vec![JdwpValue::Double(1.0), JdwpValue::Double(2.0)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "DoubleStream.of(1.0, 2.0).map(x -> x + 1).limit(3).boxed().collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 31,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            31,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 2,
                        sample: vec![JdwpValue::Double(2.0), JdwpValue::Double(3.0)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "DoubleStream.of(1.0, 2.0).map(x -> x + 1).limit(3).count()",
            Ok(JdwpValue::Long(2)),
        );

        let config = StreamDebugConfig {
            max_sample_size: 3,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: false,
            allow_terminal_ops: true,
        };
        let cancel = CancellationToken::default();
        let result = debug_stream(&mut jdwp, 1, &chain, &config, &cancel).unwrap();

        assert_eq!(result.source_sample.elements, vec!["1", "2"]);
        assert_eq!(result.steps.len(), 1);
        assert_eq!(result.steps[0].output.elements, vec!["2", "3"]);
        assert_eq!(
            result.terminal.as_ref().unwrap().value.as_deref(),
            Some("2")
        );
    }

    #[test]
    fn debug_stream_evaluates_each_stage_with_mock_jdwp() {
        let expr = "list.stream().filter(x -> x > 0).map(x -> x * 2).count()";
        let chain = analyze_stream_expression(expr).unwrap();

        let mut jdwp = MockJdwpClient::new();
        jdwp.set_evaluation(
            1,
            "list.stream().limit(3).collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 10,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            10,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 3,
                        sample: vec![JdwpValue::Int(1), JdwpValue::Int(2), JdwpValue::Int(3)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "list.stream().filter(x -> x > 0).limit(3).collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 11,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            11,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 2,
                        sample: vec![JdwpValue::Int(2), JdwpValue::Int(3)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "list.stream().filter(x -> x > 0).map(x -> x * 2).limit(3).collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 12,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            12,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 2,
                        sample: vec![JdwpValue::Int(4), JdwpValue::Int(6)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "list.stream().filter(x -> x > 0).map(x -> x * 2).limit(3).count()",
            Ok(JdwpValue::Long(2)),
        );

        let config = StreamDebugConfig {
            max_sample_size: 3,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: false,
            allow_terminal_ops: true,
        };
        let cancel = CancellationToken::default();
        let result = debug_stream(&mut jdwp, 1, &chain, &config, &cancel).unwrap();

        assert_eq!(result.source_sample.elements, vec!["1", "2", "3"]);
        assert_eq!(result.steps.len(), 2);
        assert_eq!(result.steps[0].output.elements, vec!["2", "3"]);
        assert_eq!(result.steps[1].output.elements, vec!["4", "6"]);
        assert_eq!(
            result.terminal.as_ref().unwrap().value.as_deref(),
            Some("2")
        );
    }

    #[test]
    fn debug_stream_refuses_likely_consumable_existing_stream_values() {
        let expr = "s.filter(x -> x > 0).count()";
        let chain = analyze_stream_expression(expr).unwrap();
        assert!(matches!(chain.source, StreamSource::ExistingStream { .. }));

        let mut jdwp = MockJdwpClient::new();
        let config = StreamDebugConfig {
            max_sample_size: 2,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: false,
            allow_terminal_ops: true,
        };
        let cancel = CancellationToken::default();

        let err = debug_stream(&mut jdwp, 1, &chain, &config, &cancel).unwrap_err();
        match err {
            StreamDebugError::UnsafeExistingStream { stream_expr } => {
                assert_eq!(stream_expr, "s");
            }
            other => panic!("expected UnsafeExistingStream error, got {other:?}"),
        }
    }

    #[test]
    fn debug_stream_refuses_existing_stream_values_with_parens_in_index() {
        // `streams[(i)]` is still just an access path to an existing stream value (and therefore
        // unsafe to sample), even though the expression contains parentheses.
        let expr = "streams[(i)].filter(x -> x > 0).count()";
        let chain = analyze_stream_expression(expr).unwrap();
        assert!(matches!(chain.source, StreamSource::ExistingStream { .. }));

        let mut jdwp = MockJdwpClient::new();
        let config = StreamDebugConfig {
            max_sample_size: 2,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: false,
            allow_terminal_ops: true,
        };
        let cancel = CancellationToken::default();

        let err = debug_stream(&mut jdwp, 1, &chain, &config, &cancel).unwrap_err();
        match err {
            StreamDebugError::UnsafeExistingStream { stream_expr } => {
                assert_eq!(stream_expr, "streams[(i)]");
            }
            other => panic!("expected UnsafeExistingStream error, got {other:?}"),
        }
    }

    #[test]
    fn debug_stream_allows_re_evaluatable_existing_stream_expressions_with_calls() {
        let expr = "java.util.Arrays.stream(arr).filter(x -> x > 0).count()";
        let chain = analyze_stream_expression(expr).unwrap();
        assert!(matches!(chain.source, StreamSource::ExistingStream { .. }));

        let mut jdwp = MockJdwpClient::new();
        jdwp.set_evaluation(
            1,
            "java.util.stream.StreamSupport.stream((java.util.Arrays.stream(arr)).limit(2).spliterator(), false).collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 10,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            10,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 2,
                        sample: vec![JdwpValue::Int(-1), JdwpValue::Int(1)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "java.util.stream.StreamSupport.stream((java.util.Arrays.stream(arr).filter(x -> x > 0)).limit(2).spliterator(), false).collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 11,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            11,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 1,
                        sample: vec![JdwpValue::Int(1)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "java.util.Arrays.stream(arr).filter(x -> x > 0).limit(2).count()",
            Ok(JdwpValue::Long(1)),
        );

        let config = StreamDebugConfig {
            max_sample_size: 2,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: false,
            allow_terminal_ops: true,
        };
        let cancel = CancellationToken::default();

        let result = debug_stream(&mut jdwp, 1, &chain, &config, &cancel).unwrap();
        assert_eq!(result.source_sample.elements, vec!["-1", "1"]);
        assert_eq!(result.steps.len(), 1);
        assert_eq!(result.steps[0].output.elements, vec!["1"]);
        assert_eq!(
            result.terminal.as_ref().unwrap().value.as_deref(),
            Some("1")
        );
    }

    #[test]
    fn debug_stream_for_each_returns_void_when_allowed() {
        let expr = "list.stream().forEach(System.out::println)";
        let chain = analyze_stream_expression(expr).unwrap();

        let mut jdwp = MockJdwpClient::new();
        jdwp.set_evaluation(
            1,
            "list.stream().limit(3).collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 10,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            10,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 3,
                        sample: vec![JdwpValue::Int(1), JdwpValue::Int(2), JdwpValue::Int(3)],
                    },
                },
                children: Vec::new(),
            },
        );

        // The runtime wraps void-returning terminals so evaluators that expect
        // value-returning expressions still compile.
        jdwp.set_evaluation(
            1,
            "((java.util.function.Supplier<Object>)(() -> {list.stream().limit(3).forEach(System.out::println);return null;})).get()",
            Ok(JdwpValue::Void),
        );

        let config = StreamDebugConfig {
            max_sample_size: 3,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: true,
            allow_terminal_ops: true,
        };
        let cancel = CancellationToken::default();
        let result = debug_stream(&mut jdwp, 1, &chain, &config, &cancel).unwrap();

        let terminal = result.terminal.as_ref().expect("missing terminal result");
        assert!(terminal.executed);
        assert_eq!(terminal.value.as_deref(), Some("void"));
        assert_eq!(terminal.type_name.as_deref(), Some("void"));
    }

    #[test]
    fn analyze_recognizes_for_each_ordered_as_void_terminal() {
        let expr = "list.stream().forEachOrdered(System.out::println)";
        let chain = analyze_stream_expression(expr).unwrap();

        let term = chain.terminal.as_ref().expect("missing terminal op");
        assert_eq!(term.name, "forEachOrdered");
        assert_eq!(term.kind, StreamOperationKind::ForEach);
        assert_eq!(
            term.resolved.as_ref().unwrap().return_type,
            "void",
            "expected resolved return type to be void: {term:?}"
        );
    }

    #[test]
    fn debug_stream_for_each_ordered_returns_void_when_allowed() {
        let expr = "list.stream().forEachOrdered(System.out::println)";
        let chain = analyze_stream_expression(expr).unwrap();

        let mut jdwp = MockJdwpClient::new();
        jdwp.set_evaluation(
            1,
            "list.stream().limit(3).collect(java.util.stream.Collectors.toList())",
            Ok(JdwpValue::Object(ObjectRef {
                id: 10,
                runtime_type: "java.util.ArrayList".to_string(),
            })),
        );
        jdwp.insert_object(
            10,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "java.util.ArrayList".to_string(),
                    kind: ObjectKindPreview::List {
                        size: 3,
                        sample: vec![JdwpValue::Int(1), JdwpValue::Int(2), JdwpValue::Int(3)],
                    },
                },
                children: Vec::new(),
            },
        );

        jdwp.set_evaluation(
            1,
            "((java.util.function.Supplier<Object>)(() -> {list.stream().limit(3).forEachOrdered(System.out::println);return null;})).get()",
            Ok(JdwpValue::Void),
        );

        let config = StreamDebugConfig {
            max_sample_size: 3,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: true,
            allow_terminal_ops: true,
        };
        let cancel = CancellationToken::default();
        let result = debug_stream(&mut jdwp, 1, &chain, &config, &cancel).unwrap();

        let terminal = result.terminal.as_ref().expect("missing terminal result");
        assert!(terminal.executed);
        assert_eq!(terminal.value.as_deref(), Some("void"));
        assert_eq!(terminal.type_name.as_deref(), Some("void"));
    }

    #[test]
    fn analyze_recognizes_int_stream_for_each_as_void_terminal() {
        let expr = "IntStream.range(0, 3).forEach(System.out::println)";
        let chain = analyze_stream_expression(expr).unwrap();
        assert_eq!(chain.stream_kind, StreamValueKind::IntStream);

        let term = chain.terminal.as_ref().expect("missing terminal op");
        assert_eq!(term.name, "forEach");
        assert_eq!(term.kind, StreamOperationKind::ForEach);
        assert_eq!(
            term.resolved.as_ref().unwrap().return_type,
            "void",
            "expected resolved return type to be void: {term:?}"
        );
    }
}
