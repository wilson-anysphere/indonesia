use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use nova_jdwp::wire::types::{MethodId, INVOKE_SINGLE_THREADED};
use nova_stream_debug::StreamOperationKind;
use tokio_util::sync::CancellationToken;

use crate::java_type::signature_to_java_source_type;
use crate::javac::{compile_java_to_dir, stream_eval_temp_dir, HotSwapJavacConfig};

use super::*;

static STREAM_EVAL_CLASS_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamEvalStage {
    /// The stream source expression used to obtain the initial sample (`stage0`).
    SourceSample,
    /// An intermediate operation (`stageN` for N > 0).
    IntermediateOp { stage: usize },
    /// The terminal operation (`terminal`).
    Terminal,
    /// We could not attribute the compilation error to a specific stage.
    Unknown,
}

impl StreamEvalStage {
    fn label(self) -> &'static str {
        match self {
            StreamEvalStage::SourceSample => "source sample",
            StreamEvalStage::IntermediateOp { .. } => "intermediate op",
            StreamEvalStage::Terminal => "terminal",
            StreamEvalStage::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
struct ScopedLocal {
    name: String,
    signature: String,
    generic_signature: Option<String>,
    slot: u32,
    value: JdwpValue,
}

/// A compiled+loaded stream evaluation class for a particular stopped frame.
///
/// This is intentionally narrow: it exists to allow stream debugging to evaluate
/// arbitrary Java expressions (including lambdas) in the context of a paused
/// stack frame.
#[derive(Debug, Clone)]
pub(crate) struct JavaStreamEvaluator {
    /// Fully-qualified binary class name (e.g. `com.example.NovaStreamEval_1`).
    pub(crate) class_name: String,
    class_id: ReferenceTypeId,
    thread: ThreadId,
    args: Vec<JdwpValue>,
    stage_methods: Vec<MethodId>,
    terminal_method: Option<MethodId>,
    stage_expressions: Vec<String>,
    terminal_expression: Option<String>,
}

impl JavaStreamEvaluator {
    pub(crate) async fn invoke_stage(
        &self,
        dbg: &mut Debugger,
        cancel: &CancellationToken,
        stage: usize,
    ) -> Result<JdwpValue> {
        check_cancel(cancel)?;

        let Some(method_id) = self.stage_methods.get(stage).copied() else {
            return Err(DebuggerError::InvalidRequest(format!(
                "invalid stage index {stage} (have {})",
                self.stage_methods.len()
            )));
        };

        let (value, exception) = cancellable_jdwp(
            cancel,
            dbg.jdwp.class_type_invoke_method(
                self.class_id,
                self.thread,
                method_id,
                &self.args,
                INVOKE_SINGLE_THREADED,
            ),
        )
        .await?;

        if exception != 0 {
            let thrown = dbg.format_thrown_exception(cancel, exception).await?;
            let expr = self
                .stage_expressions
                .get(stage)
                .map(|s| s.as_str())
                .unwrap_or("<expr>");
            return Err(DebuggerError::InvalidRequest(format!(
                "evaluation threw {thrown} while evaluating `{expr}`"
            )));
        }

        Ok(value)
    }

    pub(crate) async fn invoke_terminal(
        &self,
        dbg: &mut Debugger,
        cancel: &CancellationToken,
    ) -> Result<JdwpValue> {
        check_cancel(cancel)?;

        let Some(method_id) = self.terminal_method else {
            return Err(DebuggerError::InvalidRequest(
                "no terminal expression was configured".to_string(),
            ));
        };

        let (value, exception) = cancellable_jdwp(
            cancel,
            dbg.jdwp.class_type_invoke_method(
                self.class_id,
                self.thread,
                method_id,
                &self.args,
                INVOKE_SINGLE_THREADED,
            ),
        )
        .await?;

        if exception != 0 {
            let thrown = dbg.format_thrown_exception(cancel, exception).await?;
            let expr = self
                .terminal_expression
                .as_deref()
                .unwrap_or("<terminal expr>");
            return Err(DebuggerError::InvalidRequest(format!(
                "evaluation threw {thrown} while evaluating `{expr}`"
            )));
        }

        Ok(value)
    }
}

impl Debugger {
    pub(crate) async fn prepare_stream_evaluator(
        &mut self,
        cancel: &CancellationToken,
        frame_id: i64,
        javac: &HotSwapJavacConfig,
        stage_expressions: &[String],
        terminal_expression: Option<&str>,
    ) -> Result<JavaStreamEvaluator> {
        check_cancel(cancel)?;

        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown frameId {frame_id}"
            )));
        };

        let locals = self.scoped_locals_with_values(cancel, &frame).await?;
        let (args, param_specs, needs_this_rewrite) = build_param_bindings(&locals);

        let class_sig = self.signature(cancel, frame.location.class_id).await?;
        let package = package_from_signature(&class_sig);

        let nonce = STREAM_EVAL_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let simple_name = format!("NovaStreamEval_{nonce}");
        let fqcn = match package.as_deref() {
            Some(pkg) if !pkg.is_empty() => format!("{pkg}.{simple_name}"),
            _ => simple_name.clone(),
        };

        let mut imports = vec![
            "import java.util.*;".to_string(),
            "import java.util.stream.*;".to_string(),
            "import java.util.function.*;".to_string(),
            "import static java.util.stream.Collectors.*;".to_string(),
        ];
        if let Some(extra) = self.best_effort_imports_for_frame(cancel, &frame).await? {
            imports.extend(extra);
        }
        let imports = dedup_lines(imports);

        let stage_exprs: Vec<String> = stage_expressions
            .iter()
            .map(|expr| normalize_expr(expr, needs_this_rewrite))
            .collect();
        let terminal_expr =
            terminal_expression.map(|expr| normalize_expr(expr, needs_this_rewrite));

        let source = render_eval_source(
            package.as_deref(),
            &simple_name,
            &imports,
            &param_specs,
            &stage_exprs,
            terminal_expr.as_deref(),
        );

        // Compile in a throwaway directory.
        let temp_dir = stream_eval_temp_dir().map_err(|err| {
            DebuggerError::InvalidRequest(format!("failed to create javac output dir: {err}"))
        })?;
        let output_dir = temp_dir.path();
        let source_path = output_dir.join(format!("{simple_name}.java"));
        std::fs::write(&source_path, &source).map_err(|err| {
            DebuggerError::InvalidRequest(format!(
                "failed to write generated eval source {}: {err}",
                source_path.display()
            ))
        })?;

        // If we don't have a resolved build-system language level, default to a conservative
        // `--release 8` so the injected helper class can load on older debuggee JVMs.
        let javac = crate::javac::apply_stream_eval_defaults(javac);
        let compiled = match compile_java_to_dir(cancel, &javac, &source_path, output_dir).await {
            Ok(classes) => classes,
            Err(err) => {
                if cancel.is_cancelled() {
                    return Err(JdwpError::Cancelled.into());
                }
                // JDK 8's `javac` doesn't support `--release`. If we hit that case, retry with
                // `-source/-target` so stream-debug works even when only a JDK 8 toolchain is
                // available on the host.
                let should_retry_without_release = javac.release.is_some()
                    && crate::javac::javac_error_is_release_flag_unsupported(&err);
                if should_retry_without_release {
                    let mut fallback = javac.clone();
                    if let Some(release) = fallback.release.take() {
                        if fallback.source.is_none() {
                            fallback.source = Some(release.clone());
                        }
                        if fallback.target.is_none() {
                            fallback.target = Some(release);
                        }
                    }

                    // Ensure a clean output directory so we don't accidentally load stale classes.
                    let _ = std::fs::remove_dir_all(output_dir);
                    std::fs::create_dir_all(output_dir).map_err(|err| {
                        DebuggerError::InvalidRequest(format!(
                            "failed to recreate javac output dir {}: {err}",
                            output_dir.display()
                        ))
                    })?;
                    std::fs::write(&source_path, &source).map_err(|err| {
                        DebuggerError::InvalidRequest(format!(
                            "failed to write generated eval source {}: {err}",
                            source_path.display()
                        ))
                    })?;

                    match compile_java_to_dir(cancel, &fallback, &source_path, output_dir).await {
                        Ok(classes) => classes,
                        Err(err2) => {
                            let original = format_stream_eval_compile_failure(
                                &source_path,
                                &source,
                                &stage_exprs,
                                terminal_expr.as_deref(),
                                &err,
                            );
                            let retry = format_stream_eval_compile_failure(
                                &source_path,
                                &source,
                                &stage_exprs,
                                terminal_expr.as_deref(),
                                &err2,
                            );
                            return Err(DebuggerError::InvalidRequest(format!(
                                "{original}\n\nretry without `--release` failed:\n{retry}"
                            )));
                        }
                    }
                } else {
                    let message = format_stream_eval_compile_failure(
                        &source_path,
                        &source,
                        &stage_exprs,
                        terminal_expr.as_deref(),
                        &err,
                    );
                    return Err(DebuggerError::InvalidRequest(message));
                }
            }
        };

        check_cancel(cancel)?;

        let loader = cancellable_jdwp(
            cancel,
            self.jdwp
                .reference_type_class_loader(frame.location.class_id),
        )
        .await?;
        if loader == 0 {
            return Err(DebuggerError::InvalidRequest(
                "unable to resolve classloader for paused frame; cannot define eval class"
                    .to_string(),
            ));
        }

        let mut eval_class_id: Option<ReferenceTypeId> = None;
        for class in compiled {
            let id = cancellable_jdwp(
                cancel,
                self.jdwp
                    .class_loader_define_class(loader, &class.class_name, &class.bytecode),
            )
            .await?;
            if class.class_name == fqcn {
                eval_class_id = Some(id);
            }
        }

        let eval_class_id = eval_class_id.ok_or_else(|| {
            DebuggerError::InvalidRequest(format!(
                "javac did not produce expected evaluation class `{fqcn}`"
            ))
        })?;

        let methods =
            cancellable_jdwp(cancel, self.jdwp.reference_type_methods(eval_class_id)).await?;
        let mut by_name: HashMap<String, MethodId> = HashMap::new();
        for method in methods {
            by_name.insert(method.name, method.method_id);
        }

        let mut stage_methods = Vec::with_capacity(stage_exprs.len());
        for idx in 0..stage_exprs.len() {
            let name = format!("stage{idx}");
            let id = by_name.get(&name).copied().ok_or_else(|| {
                DebuggerError::InvalidRequest(format!(
                    "generated evaluation method `{name}` was not found on `{fqcn}`"
                ))
            })?;
            stage_methods.push(id);
        }

        let terminal_method = match terminal_expr.as_deref() {
            Some(_) => Some(by_name.get("terminal").copied().ok_or_else(|| {
                DebuggerError::InvalidRequest(format!(
                    "generated evaluation method `terminal` was not found on `{fqcn}`"
                ))
            })?),
            None => None,
        };

        Ok(JavaStreamEvaluator {
            class_name: fqcn,
            class_id: eval_class_id,
            thread: frame.thread,
            args,
            stage_methods,
            terminal_method,
            stage_expressions: stage_exprs,
            terminal_expression: terminal_expr,
        })
    }

    async fn scoped_locals_with_values(
        &mut self,
        cancel: &CancellationToken,
        frame: &FrameHandle,
    ) -> Result<Vec<ScopedLocal>> {
        check_cancel(cancel)?;

        let mut locals: Vec<(String, String, Option<String>, u32, u64, u32)> = Vec::new();

        let generic_result = cancellable_jdwp(
            cancel,
            self.jdwp.method_variable_table_with_generic(
                frame.location.class_id,
                frame.location.method_id,
            ),
        )
        .await;

        match generic_result {
            Ok((_argc, vars)) => {
                for v in vars {
                    if v.code_index <= frame.location.index
                        && frame.location.index < v.code_index + (v.length as u64)
                    {
                        locals.push((
                            v.name,
                            v.signature,
                            v.generic_signature,
                            v.slot,
                            v.code_index,
                            v.length,
                        ));
                    }
                }
            }
            Err(err) if is_unsupported_command_error(&err) => {
                let (_argc, vars) = cancellable_jdwp(
                    cancel,
                    self.jdwp
                        .method_variable_table(frame.location.class_id, frame.location.method_id),
                )
                .await?;
                for v in vars {
                    if v.code_index <= frame.location.index
                        && frame.location.index < v.code_index + (v.length as u64)
                    {
                        locals.push((v.name, v.signature, None, v.slot, v.code_index, v.length));
                    }
                }
            }
            Err(err) => return Err(err.into()),
        }

        // Deterministic + avoid duplicates by slot.
        locals.sort_by(|a, b| a.3.cmp(&b.3));
        locals.dedup_by(|a, b| a.3 == b.3);

        let slots: Vec<(u32, String)> = locals
            .iter()
            .map(|(_name, sig, _gs, slot, ..)| (*slot, sig.clone()))
            .collect();

        let values = cancellable_jdwp(
            cancel,
            self.jdwp
                .stack_frame_get_values(frame.thread, frame.frame_id, &slots),
        )
        .await?;

        let mut out = Vec::with_capacity(locals.len());
        for ((name, signature, generic_signature, slot, ..), value) in
            locals.into_iter().zip(values.into_iter())
        {
            out.push(ScopedLocal {
                name,
                signature,
                generic_signature,
                slot,
                value,
            });
        }
        Ok(out)
    }

    async fn best_effort_imports_for_frame(
        &mut self,
        cancel: &CancellationToken,
        frame: &FrameHandle,
    ) -> Result<Option<Vec<String>>> {
        check_cancel(cancel)?;
        let source_file = match self.source_file(cancel, frame.location.class_id).await {
            Ok(file) => file,
            Err(_) => return Ok(None),
        };
        let Some(path) = self
            .resolve_source_path(cancel, frame.location.class_id, &source_file)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(extract_import_lines(&path)))
    }

    async fn format_thrown_exception(
        &mut self,
        cancel: &CancellationToken,
        exception: ObjectId,
    ) -> Result<String> {
        check_cancel(cancel)?;

        let runtime_type =
            match cancellable_jdwp(cancel, self.inspector.runtime_type_name(exception)).await {
                Ok(name) => name,
                Err(_) => "Exception".to_string(),
            };

        let message_id =
            match cancellable_jdwp(cancel, self.inspector.object_children(exception)).await {
                Ok(children) => children.into_iter().find_map(|child| {
                    (child.name == "detailMessage")
                        .then_some(child.value)
                        .and_then(|value| match value {
                            JdwpValue::Object { id, .. } if id != 0 => Some(id),
                            _ => None,
                        })
                }),
                Err(_) => None,
            };
        let message = match message_id {
            Some(id) => cancellable_jdwp(cancel, self.jdwp.string_reference_value(id))
                .await
                .ok(),
            None => None,
        };

        Ok(match message {
            Some(msg) if !msg.is_empty() => format!("{runtime_type}: {msg}"),
            _ => runtime_type,
        })
    }
}

fn is_unsupported_command_error(err: &JdwpError) -> bool {
    const ERROR_NOT_FOUND: u16 = 41;
    const ERROR_UNSUPPORTED_VERSION: u16 = 68;
    const ERROR_NOT_IMPLEMENTED: u16 = 99;
    matches!(
        err,
        JdwpError::VmError(ERROR_NOT_FOUND | ERROR_UNSUPPORTED_VERSION | ERROR_NOT_IMPLEMENTED)
    )
}

fn package_from_signature(signature: &str) -> Option<String> {
    let internal = signature.strip_prefix('L')?.strip_suffix(';')?;
    let (pkg, _class) = internal.rsplit_once('/')?;
    Some(pkg.replace('/', "."))
}

fn build_param_bindings(locals: &[ScopedLocal]) -> (Vec<JdwpValue>, Vec<String>, bool) {
    let mut args = Vec::with_capacity(locals.len());
    let mut params = Vec::with_capacity(locals.len());
    let mut needs_this_rewrite = false;

    for local in locals {
        if local.name.trim().is_empty() {
            continue;
        }

        let (param_name, needs_rewrite) = if local.name == "this" {
            (String::from("__this"), true)
        } else {
            (local.name.clone(), false)
        };
        needs_this_rewrite |= needs_rewrite;

        let chosen_sig = local
            .generic_signature
            .as_deref()
            .unwrap_or(local.signature.as_str());
        let ty = signature_to_java_source_type(chosen_sig);
        params.push(format!("{ty} {param_name}"));
        args.push(local.value.clone());
    }

    (args, params, needs_this_rewrite)
}

fn normalize_expr(expr: &str, rewrite_this: bool) -> String {
    let expr = expr.trim();
    let expr = expr.strip_suffix(';').unwrap_or(expr).trim();
    if rewrite_this {
        rewrite_this_token(expr)
    } else {
        expr.to_string()
    }
}

fn rewrite_this_token(expr: &str) -> String {
    // Minimal lexical scan: replace identifier tokens equal to `this`, but do not
    // replace inside string/char literals.
    let mut out = String::with_capacity(expr.len());
    let mut chars = expr.chars().peekable();
    let mut in_str = false;
    let mut in_char = false;
    let mut escape = false;

    while let Some(ch) = chars.next() {
        if in_str {
            out.push(ch);
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }

        if in_char {
            out.push(ch);
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_str = true;
                out.push(ch);
            }
            '\'' => {
                in_char = true;
                out.push(ch);
            }
            _ if is_ident_start(ch) => {
                let mut ident = String::new();
                ident.push(ch);
                while let Some(next) = chars.peek().copied() {
                    if is_ident_part(next) {
                        ident.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if ident == "this" {
                    out.push_str("__this");
                } else {
                    out.push_str(&ident);
                }
            }
            _ => out.push(ch),
        }
    }

    out
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_alphabetic()
}

fn is_ident_part(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

fn render_eval_source(
    package: Option<&str>,
    class_name: &str,
    imports: &[String],
    params: &[String],
    stage_exprs: &[String],
    terminal_expr: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(pkg) = package {
        if !pkg.is_empty() {
            out.push_str("package ");
            out.push_str(pkg);
            out.push_str(";\n\n");
        }
    }

    for import in imports {
        out.push_str(import.trim());
        out.push('\n');
    }
    out.push('\n');

    out.push_str("public final class ");
    out.push_str(class_name);
    out.push_str(" {\n");

    let params = params.join(", ");
    for (idx, expr) in stage_exprs.iter().enumerate() {
        if is_known_void_stream_expression(expr) {
            out.push_str("  public static void stage");
            out.push_str(&idx.to_string());
            out.push('(');
            out.push_str(&params);
            out.push_str(") { ");
            out.push_str(expr);
            out.push_str("; }\n");
        } else {
            out.push_str("  public static Object stage");
            out.push_str(&idx.to_string());
            out.push('(');
            out.push_str(&params);
            out.push_str(") { return ");
            out.push_str(expr);
            out.push_str("; }\n");
        }
    }

    if let Some(expr) = terminal_expr {
        if is_known_void_stream_expression(expr) {
            out.push_str("  public static void terminal(");
            out.push_str(&params);
            out.push_str(") { ");
            out.push_str(expr);
            out.push_str("; }\n");
        } else {
            out.push_str("  public static Object terminal(");
            out.push_str(&params);
            out.push_str(") { return ");
            out.push_str(expr);
            out.push_str("; }\n");
        }
    }

    out.push_str("}\n");
    out
}

fn is_known_void_stream_expression(expr: &str) -> bool {
    // Stream debugging uses `JavaStreamEvaluator` to compile small helper classes where each stage
    // is exposed as a static method. Historically we emitted `Object`-returning methods containing
    // `return <expr>;`. This fails to compile for `void` expressions like `Stream.forEach(...)`.
    //
    // We determine "known void" by running the stream analyzer over the expression and checking
    // the resolved terminal operation kind/return type. If analysis fails (not a stream pipeline),
    // fall back to "not void".
    match nova_stream_debug::analyze_stream_expression(expr) {
        Ok(chain) => {
            if let Some(term) = chain.terminal {
                term.kind == StreamOperationKind::ForEach
                    || term
                        .resolved
                        .as_ref()
                        .is_some_and(|resolved| resolved.return_type == "void")
            } else {
                expr_ends_with_void_foreach_call(expr)
            }
        }
        Err(_) => expr_ends_with_void_foreach_call(expr),
    }
}

fn expr_ends_with_void_foreach_call(expr: &str) -> bool {
    let expr = expr.trim();
    let expr = expr.strip_suffix(';').unwrap_or(expr).trim();
    expr_ends_with_member_call_named(expr, "forEach")
        || expr_ends_with_member_call_named(expr, "forEachOrdered")
}

fn expr_ends_with_member_call_named(expr: &str, method: &str) -> bool {
    if !expr.ends_with(')') {
        return false;
    }

    let Some(open_paren) = open_paren_for_final_call(expr) else {
        return false;
    };
    let before_paren = &expr[..open_paren];

    let Some(method_end) = before_paren
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx + ch.len_utf8()))
    else {
        return false;
    };

    let mut seen_ident = false;
    let mut method_start = method_end;
    for (idx, ch) in before_paren[..method_end].char_indices().rev() {
        if is_ident_part(ch) {
            seen_ident = true;
            method_start = idx;
        } else if seen_ident {
            break;
        } else if ch.is_whitespace() {
            continue;
        } else {
            break;
        }
    }

    if !seen_ident {
        return false;
    }

    if &before_paren[method_start..method_end] != method {
        return false;
    }

    before_paren[..method_start]
        .chars()
        .rev()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| ch == '.')
}

fn open_paren_for_final_call(expr: &str) -> Option<usize> {
    let mut stack: Vec<usize> = Vec::new();
    let mut last_match = None::<usize>;

    let mut in_str = false;
    let mut in_char = false;
    let mut escape = false;

    for (idx, ch) in expr.char_indices() {
        if escape {
            escape = false;
            continue;
        }

        if in_str {
            match ch {
                '\\' => escape = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }

        if in_char {
            match ch {
                '\\' => escape = true,
                '\'' => in_char = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_str = true,
            '\'' => in_char = true,
            '(' => stack.push(idx),
            ')' => {
                let open = stack.pop()?;
                last_match = Some(open);
            }
            _ => {}
        }
    }

    if !stack.is_empty() {
        return None;
    }

    last_match
}

fn extract_import_lines(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("import ") || trimmed.starts_with("import static ") {
                Some(trimmed.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn dedup_lines(lines: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Normalize import lines so that:
        // - trailing semicolons are consistent
        // - `import ...; // comment` is reduced to `import ...;`
        // - duplicates are removed even if semicolons/comments differ
        let normalized = if line.starts_with("import ") || line.starts_with("import static ") {
            // Keep only the portion of the line up to (and including) the first ';' if present.
            // This drops any trailing comment content and avoids appending an extra ';' later.
            let stmt = match line.split_once(';') {
                Some((before, _after)) => before.trim(),
                None => line,
            };

            // Canonicalize by stripping the leading `import ` and any trailing `;`.
            let spec = stmt
                .strip_prefix("import ")
                .unwrap_or(stmt)
                .trim()
                .trim_end_matches(';')
                .trim();
            format!("import {spec};")
        } else {
            line.to_string()
        };

        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

fn format_stream_eval_compile_failure(
    source_path: &Path,
    source: &str,
    stage_exprs: &[String],
    terminal_expr: Option<&str>,
    javac_error: &crate::hot_swap::CompileError,
) -> String {
    let diagnostics = javac_error.to_string();

    let (stage, expr) =
        stream_eval_stage_and_expression(source, stage_exprs, terminal_expr, &diagnostics);
    let javac_loc = parse_first_formatted_javac_location(&diagnostics);
    let user_expr = terminal_expr
        .or_else(|| stage_exprs.last().map(|s| s.as_str()))
        .filter(|s| !s.trim().is_empty());

    let mut out = String::new();
    out.push_str("stream debug helper compilation failed\n");
    if let Some(user_expr) = user_expr {
        out.push_str(&format!("User expression: `{user_expr}`\n"));
    }
    out.push_str(&format!("Generated source: {}\n", source_path.display()));
    out.push_str(&match stage {
        StreamEvalStage::IntermediateOp { stage } => {
            format!("Stage: intermediate op (stage{stage})\n")
        }
        other => format!("Stage: {}\n", other.label()),
    });
    if let Some(expr) = expr {
        if user_expr.map(|user| user != expr).unwrap_or(true) {
            out.push_str(&format!("Stage expression: `{expr}`\n"));
        }
    }

    if let Some((line, col)) = javac_loc {
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
    out.push('\n');
    out.push_str("Javac diagnostics:\n");
    out.push_str(&diagnostics);

    for hint in stream_eval_compile_hints(&diagnostics) {
        out.push_str("\n\nHint: ");
        out.push_str(&hint);
    }

    out
}

fn stream_eval_stage_and_expression<'a>(
    source: &str,
    stage_exprs: &'a [String],
    terminal_expr: Option<&'a str>,
    javac_output: &str,
) -> (StreamEvalStage, Option<&'a str>) {
    let Some((line, _col)) = parse_first_formatted_javac_location(javac_output) else {
        return (StreamEvalStage::Unknown, None);
    };

    match stage_decl_for_source_line(source, line) {
        StageDecl::Terminal => (StreamEvalStage::Terminal, terminal_expr),
        StageDecl::Stage(0) => (
            StreamEvalStage::SourceSample,
            stage_exprs.get(0).map(|s| s.as_str()),
        ),
        StageDecl::Stage(stage) => (
            StreamEvalStage::IntermediateOp { stage },
            stage_exprs.get(stage).map(|s| s.as_str()),
        ),
        StageDecl::Unknown => (StreamEvalStage::Unknown, None),
    }
}

fn parse_first_formatted_javac_location(output: &str) -> Option<(usize, usize)> {
    // `format_javac_failure` emits one diagnostic per line in the form:
    // `<file>:<line>:<col>: <message>`
    let line = output.lines().find(|l| !l.trim().is_empty())?;

    // Important: the message itself can contain `:` (e.g. `incompatible types: cannot infer...`),
    // so we *must not* split on `:` across the whole line. Instead split once on the first
    // `": "` separator between `<file>:<line>:<col>:` and the message.
    let (location, _message) = line.split_once(": ")?;

    let mut parts = location.rsplitn(3, ':');
    let col_s = parts.next()?.trim();
    let line_s = parts.next()?.trim();
    let _file = parts.next()?; // may contain ':' on Windows; rsplitn keeps it intact.

    let line_no = line_s.parse::<usize>().ok()?;
    let col_no = col_s.parse::<usize>().ok()?;
    Some((line_no, col_no))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageDecl {
    Stage(usize),
    Terminal,
    Unknown,
}

fn stage_decl_for_source_line(source: &str, error_line_1_based: usize) -> StageDecl {
    if error_line_1_based == 0 {
        return StageDecl::Unknown;
    }
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return StageDecl::Unknown;
    }
    let mut idx = error_line_1_based.saturating_sub(1);
    if idx >= lines.len() {
        idx = lines.len().saturating_sub(1);
    }

    for line in lines[..=idx].iter().rev() {
        if let Some(decl) = parse_stage_decl_line(line) {
            return decl;
        }
    }
    StageDecl::Unknown
}

fn parse_stage_decl_line(line: &str) -> Option<StageDecl> {
    // We only want to match method declarations in the generated helper source.
    let trimmed = line.trim_start();
    if !trimmed.starts_with("public static ") {
        return None;
    }

    // Only consider the method *signature* (everything before `{`). Otherwise we can get false
    // positives when user expressions contain e.g. `terminal(...)`.
    let header = trimmed.split_once('{').map(|(h, _)| h).unwrap_or(trimmed);

    // Parse the method name as the identifier immediately preceding the first `(`.
    let open_paren = header.find('(')?;
    let before_paren = header[..open_paren].trim_end();
    let method_name = before_paren.split_whitespace().last()?;

    if method_name == "terminal" {
        return Some(StageDecl::Terminal);
    }
    if let Some(suffix) = method_name.strip_prefix("stage") {
        if !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()) {
            let stage = suffix.parse::<usize>().ok()?;
            return Some(StageDecl::Stage(stage));
        }
    }

    None
}

fn stream_eval_compile_hints(javac_output: &str) -> Vec<String> {
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

    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_lines_normalizes_import_semicolons_and_trailing_comments() {
        let lines = vec![
            "import java.util.*".to_string(),
            "import java.util.*;".to_string(),
            "import static java.util.stream.Collectors.*".to_string(),
            "import static java.util.stream.Collectors.*; // comment".to_string(),
            "import static java.util.stream.Collectors.*;".to_string(),
        ];

        let deduped = dedup_lines(lines);
        assert_eq!(
            deduped,
            vec![
                "import java.util.*;".to_string(),
                "import static java.util.stream.Collectors.*;".to_string(),
            ]
        );
    }

    #[test]
    fn render_eval_source_uses_void_return_type_for_foreach_terminal() {
        let src = render_eval_source(
            None,
            "NovaStreamEval_Test",
            &[],
            &["java.util.stream.Stream<Integer> s".to_string()],
            &[],
            Some("s.forEach(System.out::println)"),
        );

        assert!(
            src.contains("public static void terminal"),
            "expected void terminal method, got:\n{src}"
        );
        assert!(
            !src.contains("return s.forEach"),
            "should not emit `return <void expr>;`:\n{src}"
        );
    }

    #[test]
    fn render_eval_source_uses_void_return_type_for_foreach_ordered_terminal() {
        let src = render_eval_source(
            None,
            "NovaStreamEval_Test",
            &[],
            &["java.util.stream.Stream<Integer> s".to_string()],
            &[],
            Some("s.forEachOrdered(System.out::println)"),
        );

        assert!(
            src.contains("public static void terminal"),
            "expected void terminal method, got:\n{src}"
        );
        assert!(
            !src.contains("return s.forEachOrdered"),
            "should not emit `return <void expr>;`:\n{src}"
        );
    }

    #[test]
    fn render_eval_source_uses_void_return_type_for_int_stream_foreach_terminal() {
        let src = render_eval_source(
            None,
            "NovaStreamEval_Test",
            &[],
            &[],
            &[],
            Some("java.util.stream.IntStream.range(0, 3).forEach(System.out::println)"),
        );

        assert!(
            src.contains("public static void terminal"),
            "expected void terminal method, got:\n{src}"
        );
        assert!(
            !src.contains("return java.util.stream.IntStream.range"),
            "should not emit `return <void expr>;`:\n{src}"
        );
    }

    #[test]
    fn render_eval_source_uses_void_return_type_for_foreach_terminal_when_analysis_has_no_terminal()
    {
        // `mapToInt` is not currently modeled by the stream analyzer. Ensure we still treat the
        // overall expression as void when it ends with `forEach`.
        let src = render_eval_source(
            None,
            "NovaStreamEval_Test",
            &[],
            &["java.util.stream.Stream<Integer> s".to_string()],
            &[],
            Some(r#"s.map(x -> x).mapToInt(x -> x).forEach(x -> System.out.println(")"))"#),
        );

        assert!(
            src.contains("public static void terminal"),
            "expected void terminal method, got:\n{src}"
        );
        assert!(
            !src.contains("return s.map"),
            "should not emit `return <void expr>;`:\n{src}"
        );
    }

    #[test]
    fn stream_eval_compile_failure_includes_stage_and_javac_diagnostics() {
        let source_path = Path::new("/tmp/NovaStreamEval_Test.java");
        let source = concat!(
            "package com.example;\n",
            "\n",
            "import java.util.*;\n",
            "import java.util.stream.*;\n",
            "\n",
            "public final class NovaStreamEval_Test {\n",
            "  // filler\n",
            "  // filler\n",
            "  // filler\n",
            "  public static Object stage0(java.util.List<Integer> list) { return list.stream().collect(Collectors.toList()); }\n",
            "}\n",
        );

        let raw_javac_stderr = concat!(
            "/tmp/NovaStreamEval_Test.java:10: error: cannot find symbol\n",
            "  public static Object stage0(java.util.List<Integer> list) { return list.stream().collect(Collectors.toList()); }\n",
            "                                                                                      ^\n",
            "  symbol:   variable Collectors\n",
            "  location: class NovaStreamEval_Test\n",
            "1 error\n",
        );

        let formatted = crate::javac::format_javac_failure(&[], raw_javac_stderr.as_bytes());
        let javac_err = crate::hot_swap::CompileError::new(formatted);

        let message = format_stream_eval_compile_failure(
            source_path,
            source,
            &[String::from("list.stream().collect(Collectors.toList())")],
            None,
            &javac_err,
        );

        assert!(
            message.contains("Stage: source sample"),
            "expected stage context in message:\n{message}"
        );
        assert!(
            message.contains("/tmp/NovaStreamEval_Test.java:10:"),
            "expected javac diagnostic location in message:\n{message}"
        );
        assert!(
            message.contains("cannot find symbol"),
            "expected javac diagnostic message in output:\n{message}"
        );
        assert!(
            message.contains("java.util.stream.Collectors"),
            "expected Collectors hint in output:\n{message}"
        );
    }

    #[test]
    fn stream_eval_compile_failure_stage_detection_tolerates_colons_in_message() {
        let source_path = Path::new("/tmp/NovaStreamEval_Test.java");
        let source = concat!(
            "package com.example;\n",
            "\n",
            "import java.util.*;\n",
            "import java.util.stream.*;\n",
            "\n",
            "public final class NovaStreamEval_Test {\n",
            "  // filler\n",
            "  // filler\n",
            "  // filler\n",
            "  public static Object stage0(java.util.List<Integer> list) { return list.stream().map(x -> x + 1).count(); }\n",
            "}\n",
        );

        // Simulate the formatted output from `javac::format_javac_failure`. The message contains a
        // colon (`incompatible types: ...`), which should not confuse stage attribution.
        let formatted = "/tmp/NovaStreamEval_Test.java:10:5: incompatible types: cannot infer type-variable(s) T";
        let javac_err = crate::hot_swap::CompileError::new(formatted);

        let message = format_stream_eval_compile_failure(
            source_path,
            source,
            &[String::from("list.stream().map(x -> x + 1).count()")],
            None,
            &javac_err,
        );

        assert!(
            message.contains("Stage: source sample"),
            "expected stage attribution despite colon in message:\n{message}"
        );
    }

    #[test]
    fn stream_eval_stage_detection_does_not_confuse_terminal_calls_in_expressions() {
        let source_path = Path::new("/tmp/NovaStreamEval_Test.java");
        let source = concat!(
            "public final class NovaStreamEval_Test {\n",
            "  public static Object stage0(Object x) { return x.toString().terminal(); }\n",
            "}\n",
        );

        let javac_err = crate::hot_swap::CompileError::new(
            "/tmp/NovaStreamEval_Test.java:2:1: error: cannot find symbol",
        );
        let message = format_stream_eval_compile_failure(
            source_path,
            source,
            &[String::from("x.toString().terminal()")],
            None,
            &javac_err,
        );

        assert!(
            message.contains("Stage: source sample"),
            "expected stage0 to be treated as source sample, not terminal:\n{message}"
        );
    }
}
