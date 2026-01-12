use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use nova_classfile::{
    parse_field_signature, BaseType, ClassTypeSignature, TypeArgument, TypeSignature as GenericType,
};
use nova_jdwp::wire::types::{MethodId, INVOKE_SINGLE_THREADED};
use tokio_util::sync::CancellationToken;

use crate::javac::{compile_java_to_dir, hot_swap_temp_dir, HotSwapJavacConfig};

use super::*;

static STREAM_EVAL_CLASS_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let output_dir = hot_swap_temp_dir().map_err(|err| {
            DebuggerError::InvalidRequest(format!("failed to create javac output dir: {err}"))
        })?;
        let _cleanup = TempDirCleanup(output_dir.clone());

        let source_path = output_dir.join(format!("{simple_name}.java"));
        std::fs::write(&source_path, source).map_err(|err| {
            DebuggerError::InvalidRequest(format!(
                "failed to write generated eval source {}: {err}",
                source_path.display()
            ))
        })?;

        let compiled = match compile_java_to_dir(cancel, javac, &source_path, &output_dir).await {
            Ok(classes) => classes,
            Err(err) => {
                if cancel.is_cancelled() {
                    return Err(JdwpError::Cancelled.into());
                }
                return Err(DebuggerError::InvalidRequest(format!(
                    "javac failed: {err}"
                )));
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

struct TempDirCleanup(PathBuf);

impl Drop for TempDirCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

        let ty = java_source_type_for_local(&local.signature, local.generic_signature.as_deref());
        params.push(format!("{ty} {param_name}"));
        args.push(local.value.clone());
    }

    (args, params, needs_this_rewrite)
}

fn java_source_type_for_local(signature: &str, generic_signature: Option<&str>) -> String {
    // Prefer the generic signature when it parses cleanly and doesn't reference
    // undeclared type variables. Type variables would require us to generate
    // corresponding type parameters on the eval methods.
    if let Some(generic) = generic_signature {
        if let Ok(ty) = parse_field_signature(generic) {
            if !contains_type_variable(&ty) {
                return generic_type_to_java_source(&ty);
            }
        }
    }

    // Fall back to erased type.
    signature_to_type_name(signature).replace('$', ".")
}

fn contains_type_variable(ty: &GenericType) -> bool {
    match ty {
        GenericType::TypeVariable(_) => true,
        GenericType::Array(inner) => contains_type_variable(inner),
        GenericType::Class(ct) => ct
            .segments
            .iter()
            .flat_map(|seg| seg.type_arguments.iter())
            .any(|arg| type_argument_contains_type_variable(arg)),
        GenericType::Base(_) => false,
    }
}

fn type_argument_contains_type_variable(arg: &TypeArgument) -> bool {
    match arg {
        TypeArgument::Any => false,
        TypeArgument::Exact(inner) => contains_type_variable(inner),
        TypeArgument::Extends(inner) => contains_type_variable(inner),
        TypeArgument::Super(inner) => contains_type_variable(inner),
    }
}

fn generic_type_to_java_source(ty: &GenericType) -> String {
    match ty {
        GenericType::Base(base) => match base {
            BaseType::Byte => "byte".to_string(),
            BaseType::Char => "char".to_string(),
            BaseType::Double => "double".to_string(),
            BaseType::Float => "float".to_string(),
            BaseType::Int => "int".to_string(),
            BaseType::Long => "long".to_string(),
            BaseType::Short => "short".to_string(),
            BaseType::Boolean => "boolean".to_string(),
        },
        GenericType::Array(inner) => format!("{}[]", generic_type_to_java_source(inner)),
        GenericType::Class(ct) => class_type_to_java_source(ct),
        GenericType::TypeVariable(name) => name.clone(),
    }
}

fn class_type_to_java_source(ct: &ClassTypeSignature) -> String {
    let mut out = String::new();
    if !ct.package.is_empty() {
        out.push_str(&ct.package.join("."));
        out.push('.');
    }

    for (idx, seg) in ct.segments.iter().enumerate() {
        if idx > 0 {
            out.push('.');
        }
        out.push_str(&seg.name.replace('$', "."));

        if !seg.type_arguments.is_empty() {
            out.push('<');
            for (arg_idx, arg) in seg.type_arguments.iter().enumerate() {
                if arg_idx > 0 {
                    out.push_str(", ");
                }
                out.push_str(&type_argument_to_java_source(arg));
            }
            out.push('>');
        }
    }
    out
}

fn type_argument_to_java_source(arg: &TypeArgument) -> String {
    match arg {
        TypeArgument::Any => "?".to_string(),
        TypeArgument::Exact(inner) => generic_type_to_java_source(inner),
        TypeArgument::Extends(inner) => format!("? extends {}", generic_type_to_java_source(inner)),
        TypeArgument::Super(inner) => format!("? super {}", generic_type_to_java_source(inner)),
    }
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
        out.push_str("  public static Object stage");
        out.push_str(&idx.to_string());
        out.push('(');
        out.push_str(&params);
        out.push_str(") { return ");
        out.push_str(expr);
        out.push_str("; }\n");
    }

    if let Some(expr) = terminal_expr {
        out.push_str("  public static Object terminal(");
        out.push_str(&params);
        out.push_str(") { return ");
        out.push_str(expr);
        out.push_str("; }\n");
    }

    out.push_str("}\n");
    out
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
        let normalized = line.trim().to_string();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            // Ensure import lines end in `;` to keep javac happy.
            if (normalized.starts_with("import ") || normalized.starts_with("import static "))
                && !normalized.ends_with(';')
            {
                out.push(format!("{normalized};"));
            } else {
                out.push(normalized);
            }
        }
    }
    out
}
