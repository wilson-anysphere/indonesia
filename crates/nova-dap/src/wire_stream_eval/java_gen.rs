use std::collections::HashSet;

use nova_stream_debug::StreamOperationKind;

/// Sanitize a local-variable name into a valid Java identifier suitable for use as a method
/// parameter.
///
/// This intentionally uses a conservative ASCII-only identifier definition:
/// - start: `_`, `$`, or `[A-Za-z]`
/// - rest: `_`, `$`, or `[A-Za-z0-9]`
///
/// Any other character is replaced with `_`. If the resulting identifier does not start with a
/// valid start character (e.g. the original name started with a digit), we prefix `_`.
///
/// Note: `this` is a Java keyword and commonly appears in JDWP local-variable tables. We rewrite
/// it to `__this` so it can be passed as a parameter.
pub fn sanitize_java_param_name(name: &str) -> String {
    let name = name.trim();
    if name == "this" {
        return "__this".to_string();
    }

    let mut out: String = name
        .chars()
        .map(|ch| {
            if is_java_identifier_part_ascii(ch) {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if out.is_empty() {
        out.push('_');
        return out;
    }

    if !out
        .chars()
        .next()
        .is_some_and(is_java_identifier_start_ascii)
    {
        out.insert(0, '_');
    }

    if is_java_keyword(&out) {
        out.push('_');
    }

    out
}

/// Rewrites `this` *tokens* in `source` to `replacement`, avoiding rewrites inside:
/// - string literals (`"..."`)
/// - character literals (`'a'`)
///
/// This is a lightweight token-aware rewrite suitable for Java expressions and statement snippets.
/// It does not attempt full Java lexing (comments, text blocks, etc.), but is careful to handle
/// backslash escapes so quoted delimiters inside literals do not terminate the literal.
pub fn rewrite_this_tokens(source: &str, replacement: &str) -> String {
    // Fast-path: avoid allocation if there is no candidate.
    if !source.contains("this") {
        return source.to_string();
    }

    let mut out = String::with_capacity(source.len());
    let mut chars = source.char_indices().peekable();

    let mut in_str = false;
    let mut in_char = false;
    let mut escape = false;
    let mut prev = None::<char>;

    while let Some((idx, ch)) = chars.next() {
        if escape {
            out.push(ch);
            escape = false;
            prev = Some(ch);
            continue;
        }

        if in_str {
            out.push(ch);
            match ch {
                '\\' => escape = true,
                '"' => in_str = false,
                _ => {}
            }
            prev = Some(ch);
            continue;
        }

        if in_char {
            out.push(ch);
            match ch {
                '\\' => escape = true,
                '\'' => in_char = false,
                _ => {}
            }
            prev = Some(ch);
            continue;
        }

        match ch {
            '"' => {
                out.push(ch);
                in_str = true;
                prev = Some(ch);
                continue;
            }
            '\'' => {
                out.push(ch);
                in_char = true;
                prev = Some(ch);
                continue;
            }
            't' => {
                if source[idx..].starts_with("this") {
                    let prev_ok = prev.map_or(true, |p| !is_java_identifier_part_ascii(p));
                    let next = source.get(idx + 4..).and_then(|s| s.chars().next());
                    let next_ok = next.map_or(true, |n| !is_java_identifier_part_ascii(n));
                    if prev_ok && next_ok {
                        out.push_str(replacement);

                        // Consume `h`, `i`, `s`.
                        let mut last = 't';
                        for _ in 0..3 {
                            let Some((_idx, next_ch)) = chars.next() else {
                                break;
                            };
                            last = next_ch;
                        }
                        prev = Some(last);
                        continue;
                    }
                }
            }
            _ => {}
        }

        out.push(ch);
        prev = Some(ch);
    }

    out
}

/// Rewrites *unqualified* method call sites to compile in injected helper contexts.
///
/// The injected stream-eval helper is a separate top-level class, so unqualified method calls like
/// `getNums()` or `helper(x)` (which are valid inside the paused frame due to implicit `this`)
/// will not compile unless qualified.
///
/// This pass:
/// - skips string (`"..."`) and char (`'a'`) literals
/// - detects identifier tokens immediately followed by optional whitespace + `(`
/// - refuses to rewrite calls already qualified via `.`
/// - rewrites:
///   - instance methods: `foo(` -> `__this.foo(`
///   - static methods: `foo(` -> `<DeclaringClassFqcn>.foo(`
///
/// Rewrites are selective: only identifiers present in `instance_method_names`/`static_method_names`
/// are modified, so static-import calls like `toList()` are preserved.
pub fn rewrite_unqualified_method_calls(
    source: &str,
    instance_method_names: &HashSet<String>,
    static_method_names: &HashSet<String>,
    has_this_object: bool,
    declaring_class_fqcn: &str,
) -> String {
    // Fast path: avoid allocation if there are no call sites.
    if !source.contains('(') {
        return source.to_string();
    }
    if instance_method_names.is_empty() && static_method_names.is_empty() {
        return source.to_string();
    }

    let mut out = String::with_capacity(source.len());
    let mut chars = source.char_indices().peekable();
    let mut in_str = false;
    let mut in_char = false;
    let mut escape = false;
    let mut prev_non_ws = None::<char>;

    while let Some((_idx, ch)) = chars.next() {
        if in_str {
            out.push(ch);
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            if !ch.is_whitespace() {
                prev_non_ws = Some(ch);
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
            if !ch.is_whitespace() {
                prev_non_ws = Some(ch);
            }
            continue;
        }

        match ch {
            '"' => {
                in_str = true;
                out.push(ch);
                prev_non_ws = Some(ch);
                continue;
            }
            '\'' => {
                in_char = true;
                out.push(ch);
                prev_non_ws = Some(ch);
                continue;
            }
            _ if is_java_identifier_start_ascii(ch) => {
                let mut ident = String::new();
                ident.push(ch);
                while let Some((_, next_ch)) = chars.peek().copied() {
                    if is_java_identifier_part_ascii(next_ch) {
                        ident.push(next_ch);
                        chars.next();
                    } else {
                        break;
                    }
                }

                let is_keyword = is_java_keyword(&ident);
                let is_call = if !is_keyword {
                    let mut lookahead = chars.clone();
                    while let Some((_, next_ch)) = lookahead.peek().copied() {
                        if next_ch.is_whitespace() {
                            lookahead.next();
                            continue;
                        }
                        break;
                    }
                    lookahead.peek().is_some_and(|(_, next_ch)| *next_ch == '(')
                } else {
                    false
                };

                let is_unqualified = is_call && prev_non_ws != Some('.');
                if is_unqualified {
                    if has_this_object && instance_method_names.contains(&ident) {
                        out.push_str("__this.");
                        out.push_str(&ident);
                    } else if static_method_names.contains(&ident)
                        && !declaring_class_fqcn.is_empty()
                    {
                        out.push_str(declaring_class_fqcn);
                        out.push('.');
                        out.push_str(&ident);
                    } else {
                        out.push_str(&ident);
                    }
                } else {
                    out.push_str(&ident);
                }

                prev_non_ws = ident.chars().last();
                continue;
            }
            _ => {}
        }

        out.push(ch);
        if !ch.is_whitespace() {
            prev_non_ws = Some(ch);
        }
    }

    out
}

/// Generate Java source for a stream-evaluation helper class.
///
/// The generated class is intended to be compiled and injected into the target JVM. This function
/// is pure and does not invoke `javac` or a JVM.
pub fn generate_stream_eval_helper_java_source(
    package_name: &str,
    class_name: &str,
    imports: &[String],
    locals: &[(String, String)],
    fields: &[(String, String)],
    static_fields: &[(String, String)],
    stages: &[String],
    terminal: Option<&str>,
    max_sample_size: usize,
) -> String {
    const THIS_IDENT: &str = "__this";
    const MAX_STREAM_SAMPLE_SIZE: usize = 25;
    const DEFAULT_IMPORTS: [&str; 4] = [
        "java.util.*",
        "java.util.stream.*",
        "java.util.function.*",
        "static java.util.stream.Collectors.*",
    ];

    let mut out = String::new();

    let class_name = class_name.trim();
    let class_name = if class_name.is_empty() {
        "__NovaStreamEvalHelper"
    } else {
        class_name
    };

    let package_name = package_name.trim();
    if !package_name.is_empty() {
        out.push_str("package ");
        out.push_str(package_name);
        out.push_str(";\n\n");
    }

    let max_sample_size = max_sample_size.min(MAX_STREAM_SAMPLE_SIZE);

    // Emit imports, preserving first-seen order while deduping.
    //
    // Start with a conservative set of defaults so common stream expressions compile without
    // requiring fully-qualified names (e.g. `collect(toList())`).
    let mut seen_imports: HashSet<String> = HashSet::new();
    for import in DEFAULT_IMPORTS
        .iter()
        .copied()
        .chain(imports.iter().map(|s| s.as_str()))
    {
        let import = import.trim();
        if import.is_empty() {
            continue;
        }

        // Normalize import lines so callers can pass:
        // - `java.util.*`
        // - `import java.util.*;`
        // - `import\tstatic java.util.stream.Collectors.*; // comment`
        //
        // and we always emit `import <spec>;` without duplicating `import`.
        let mut spec = import;

        // Drop `// ...` trailing comments first (common in import sections).
        spec = spec
            .split_once("//")
            .map(|(before, _after)| before)
            .unwrap_or(spec);
        spec = spec.trim();

        // Drop anything after the first `;` to remove trailing comments like `import Foo; // ...`.
        spec = spec
            .split_once(';')
            .map(|(before, _after)| before)
            .unwrap_or(spec);
        spec = spec.trim();

        // Strip a leading `import` keyword followed by *any* whitespace (spaces/tabs).
        if let Some(rest) = spec.strip_prefix("import") {
            if rest.chars().next().is_some_and(|ch| ch.is_whitespace()) {
                spec = rest.trim_start();
            }
        }
        spec = spec.trim();

        // Canonicalize whitespace so `import  static  foo.Bar.*;` and `import static foo.Bar.*;`
        // dedupe to the same key.
        let spec = spec.split_whitespace().collect::<Vec<_>>().join(" ");
        if spec.is_empty() {
            continue;
        }

        if seen_imports.insert(spec.clone()) {
            out.push_str("import ");
            out.push_str(&spec);
            out.push_str(";\n");
        }
    }
    if !seen_imports.is_empty() {
        out.push('\n');
    }

    out.push_str("public final class ");
    out.push_str(class_name);
    out.push_str(" {\n");
    out.push_str("  private ");
    out.push_str(class_name);
    out.push_str("() {}\n\n");

    // Stream-sampling helper: works for both reference and primitive streams.
    //
    // Important: clamp to our inspector element expansion limit so we avoid wasted work even if
    // the caller requests a larger sample.
    out.push_str("  public static java.util.List<?> sampleStream(Object stream, int max) {\n");
    out.push_str("    if (stream == null) {\n");
    out.push_str("      return java.util.Collections.emptyList();\n");
    out.push_str("    }\n\n");
    out.push_str("    int n = max;\n");
    out.push_str("    if (n < 0) {\n");
    out.push_str("      n = 0;\n");
    out.push_str("    } else if (n > ");
    out.push_str(&MAX_STREAM_SAMPLE_SIZE.to_string());
    out.push_str(") {\n");
    out.push_str("      n = ");
    out.push_str(&MAX_STREAM_SAMPLE_SIZE.to_string());
    out.push_str(";\n");
    out.push_str("    }\n\n");
    out.push_str("    if (stream instanceof java.util.stream.Stream) {\n");
    out.push_str("      return ((java.util.stream.Stream<?>) stream)\n");
    out.push_str("          .limit(n)\n");
    out.push_str("          .collect(java.util.stream.Collectors.toList());\n");
    out.push_str("    } else if (stream instanceof java.util.stream.IntStream) {\n");
    out.push_str("      return ((java.util.stream.IntStream) stream)\n");
    out.push_str("          .limit(n)\n");
    out.push_str("          .boxed()\n");
    out.push_str("          .collect(java.util.stream.Collectors.toList());\n");
    out.push_str("    } else if (stream instanceof java.util.stream.LongStream) {\n");
    out.push_str("      return ((java.util.stream.LongStream) stream)\n");
    out.push_str("          .limit(n)\n");
    out.push_str("          .boxed()\n");
    out.push_str("          .collect(java.util.stream.Collectors.toList());\n");
    out.push_str("    } else if (stream instanceof java.util.stream.DoubleStream) {\n");
    out.push_str("      return ((java.util.stream.DoubleStream) stream)\n");
    out.push_str("          .limit(n)\n");
    out.push_str("          .boxed()\n");
    out.push_str("          .collect(java.util.stream.Collectors.toList());\n");
    out.push_str("    }\n\n");
    out.push_str("    return java.util.Collections.emptyList();\n");
    out.push_str("  }\n\n");

    // Determine the most specific type we can for `this` based on locals.
    let this_ty = locals
        .iter()
        .find_map(|(name, ty)| (name.trim() == "this").then(|| ty.trim()))
        .filter(|ty| !ty.is_empty())
        .unwrap_or("Object");

    // Compute a deterministic, collision-free parameter list.
    let mut used_params: HashSet<String> = HashSet::new();
    used_params.insert(THIS_IDENT.to_string());
    let mut params: Vec<(String, String)> = Vec::new();
    params.push((this_ty.to_string(), THIS_IDENT.to_string()));

    for (name, ty) in locals {
        let name = name.trim();
        if name == "this" {
            continue;
        }
        let ty = ty.trim();
        if ty.is_empty() {
            continue;
        }

        let base = sanitize_java_param_name(name);
        let param = if used_params.insert(base.clone()) {
            base
        } else {
            // Resolve collisions deterministically.
            let mut idx = 2usize;
            loop {
                let candidate = format!("{base}_{idx}");
                if used_params.insert(candidate.clone()) {
                    break candidate;
                }
                idx += 1;
            }
        };

        params.push((ty.to_string(), param));
    }

    // Bind instance fields after locals so unqualified field references can compile.
    // For now we only bind fields that can retain their original identifier spelling
    // (i.e. sanitization is a no-op and there are no collisions).
    let mut fields_sorted: Vec<_> = fields.iter().collect();
    fields_sorted.sort_by(|a, b| a.0.trim().cmp(b.0.trim()));
    for (name, ty) in fields_sorted {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let ty = ty.trim();
        if ty.is_empty() {
            continue;
        }

        let sanitized = sanitize_java_param_name(name);
        if sanitized != name {
            // Best-effort: skip fields whose identifiers cannot be represented directly
            // without rewriting the expression.
            continue;
        }
        if !used_params.insert(sanitized.clone()) {
            // Locals (or other fields) already bound this name.
            continue;
        }

        params.push((ty.to_string(), sanitized));
    }

    // Bind static fields after instance fields. Locals and instance fields shadow static fields,
    // mirroring Java name resolution for unqualified identifiers.
    let mut static_fields_sorted: Vec<_> = static_fields.iter().collect();
    static_fields_sorted.sort_by(|a, b| a.0.trim().cmp(b.0.trim()));
    for (name, ty) in static_fields_sorted {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let ty = ty.trim();
        if ty.is_empty() {
            continue;
        }

        let sanitized = sanitize_java_param_name(name);
        if sanitized != name {
            continue;
        }
        if !used_params.insert(sanitized.clone()) {
            continue;
        }

        params.push((ty.to_string(), sanitized));
    }

    for (idx, stage) in stages.iter().enumerate() {
        let stage_name = format!("stage{idx}");
        let stage = stage.trim();
        let stage = stage.strip_suffix(';').unwrap_or(stage).trim();
        let stage = rewrite_this_tokens(stage, THIS_IDENT);
        let stage_is_void = is_known_void_stream_expression(&stage);

        out.push_str("  public static ");
        if stage_is_void {
            out.push_str("void ");
        } else {
            out.push_str("java.util.List<?> ");
        }
        out.push_str(&stage_name);
        out.push('(');

        for (param_idx, (ty, name)) in params.iter().enumerate() {
            if param_idx > 0 {
                out.push_str(", ");
            }
            out.push_str(ty);
            out.push(' ');
            out.push_str(name);
        }
        out.push_str(") {\n");

        if stage_is_void {
            out.push_str("    ");
            out.push_str(&stage);
            out.push_str(";\n  }\n");
        } else {
            out.push_str("    return ");
            out.push_str("sampleStream(");
            out.push_str(&stage);
            out.push_str(", ");
            out.push_str(&max_sample_size.to_string());
            out.push_str(")");
            out.push_str(";\n  }\n");
        }

        if idx + 1 < stages.len() {
            out.push('\n');
        }
    }

    if let Some(terminal_expr) = terminal {
        let terminal_expr = terminal_expr.trim();
        if !terminal_expr.is_empty() {
            if !stages.is_empty() {
                out.push('\n');
                out.push('\n');
            }

            let terminal_expr = terminal_expr
                .strip_suffix(';')
                .unwrap_or(terminal_expr)
                .trim();
            let terminal_expr = rewrite_this_tokens(terminal_expr, THIS_IDENT);
            let terminal_is_void = is_known_void_stream_expression(&terminal_expr);

            out.push_str("  public static ");
            if terminal_is_void {
                out.push_str("void ");
            } else {
                out.push_str("Object ");
            }
            out.push_str("terminal(");

            for (param_idx, (ty, name)) in params.iter().enumerate() {
                if param_idx > 0 {
                    out.push_str(", ");
                }
                out.push_str(ty);
                out.push(' ');
                out.push_str(name);
            }
            out.push_str(") {\n");

            if terminal_is_void {
                out.push_str("    ");
                out.push_str(&terminal_expr);
                out.push_str(";\n  }\n");
            } else {
                out.push_str("    return ");
                out.push_str(&terminal_expr);
                out.push_str(";\n  }\n");
            }
        }
    }

    out.push_str("}\n");
    out
}

fn is_known_void_stream_expression(expr: &str) -> bool {
    // Best-effort void detection:
    // - Prefer stream analysis when available (most precise).
    // - Fall back to a syntactic check for `.forEach(...)`/`.forEachOrdered(...)` at the end of the
    //   expression so we don't emit `return <void expr>;` when analysis fails due to unsupported
    //   intermediate ops (e.g. `mapToInt`).
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

    // `forEach` and `forEachOrdered` are void-returning across the standard library types that
    // matter for stream debugging (Stream / primitive streams / Iterable / Map).
    expr_ends_with_member_call_named(expr, "forEach")
        || expr_ends_with_member_call_named(expr, "forEachOrdered")
}

fn expr_ends_with_member_call_named(expr: &str, method: &str) -> bool {
    if !expr.ends_with(')') {
        return false;
    }

    // Find the `(` matching the final `)` while ignoring parentheses inside string/char literals.
    // This avoids false negatives for expressions like:
    //   s.forEach(x -> System.out.println(\")\"))
    // where the string literal contains a `)` character.
    let Some(open_paren) = open_paren_for_final_call(expr) else {
        return false;
    };

    let before_paren = &expr[..open_paren];

    // Trim whitespace before the `(`.
    let Some(method_end) = before_paren
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx + ch.len_utf8()))
    else {
        return false;
    };

    // Scan backwards over identifier characters to find the method name.
    let mut seen_ident = false;
    let mut method_start = method_end;
    for (idx, ch) in before_paren[..method_end].char_indices().rev() {
        if is_java_identifier_part_ascii(ch) {
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

    let name = &before_paren[method_start..method_end];
    if name != method {
        return false;
    }

    // Ensure this is a member call like `foo.forEach(...)`, not a bare `forEach(...)`.
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

    // Unbalanced parens; treat as "not a call" (and do not claim void).
    if !stack.is_empty() {
        return None;
    }

    last_match
}

fn is_java_identifier_start_ascii(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_java_identifier_part_ascii(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
}

fn is_java_keyword(s: &str) -> bool {
    matches!(
        s,
        // Java keywords
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            // Module system / contextual keywords (keep simple; better safe than sorry).
            | "module"
            | "open"
            | "requires"
            | "exports"
            | "opens"
            | "to"
            | "uses"
            | "provides"
            | "with"
            | "transitive"
            // literals / reserved identifiers
            | "true"
            | "false"
            | "null"
            // newer keywords
            | "var"
            | "yield"
            | "record"
            | "sealed"
            | "permits"
            | "non-sealed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    use nova_scheduler::CancellationToken;
    use tempfile::TempDir;

    fn tool_available(name: &str) -> bool {
        Command::new(name)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn is_valid_java_ident_ascii(s: &str) -> bool {
        let mut chars = s.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !is_java_identifier_start_ascii(first) {
            return false;
        }
        chars.all(is_java_identifier_part_ascii)
    }

    #[test]
    fn rewrite_this_respects_string_char_and_escaped_quotes() {
        let expr = r#"this.foo() + "this" + 't' + "\"this\"""#;
        let rewritten = rewrite_this_tokens(expr, "__this");
        assert_eq!(rewritten, r#"__this.foo() + "this" + 't' + "\"this\"""#);
    }

    #[test]
    fn rewrite_this_respects_identifier_boundaries_and_multiple_occurrences() {
        let expr = "this + thisThing + otherthis + _this + this.foo() + this";
        let rewritten = rewrite_this_tokens(expr, "__this");
        assert_eq!(
            rewritten,
            "__this + thisThing + otherthis + _this + __this.foo() + __this"
        );
    }

    #[test]
    fn rewrite_unqualified_method_calls_rewrites_instance_methods_only_at_unqualified_call_sites() {
        let instance: HashSet<String> = ["getNums".to_string()].into_iter().collect();
        let static_: HashSet<String> = HashSet::new();
        let rewritten = rewrite_unqualified_method_calls(
            "getNums().stream()",
            &instance,
            &static_,
            true,
            "com.example.Foo",
        );
        assert_eq!(rewritten, "__this.getNums().stream()");
    }

    #[test]
    fn rewrite_unqualified_method_calls_does_not_rewrite_unknown_methods() {
        let instance: HashSet<String> = HashSet::new();
        let static_: HashSet<String> = HashSet::new();
        let rewritten =
            rewrite_unqualified_method_calls("toList()", &instance, &static_, true, "Foo");
        assert_eq!(rewritten, "toList()");
    }

    #[test]
    fn rewrite_unqualified_method_calls_rewrites_inside_lambda_bodies() {
        let instance: HashSet<String> = ["helper".to_string()].into_iter().collect();
        let static_: HashSet<String> = HashSet::new();
        let rewritten = rewrite_unqualified_method_calls(
            "x -> helper(x)",
            &instance,
            &static_,
            true,
            "com.example.Foo",
        );
        assert_eq!(rewritten, "x -> __this.helper(x)");
    }

    #[test]
    fn rewrite_unqualified_method_calls_rewrites_static_methods_when_no_this_object() {
        let instance: HashSet<String> = HashSet::new();
        let static_: HashSet<String> = ["staticHelper".to_string()].into_iter().collect();
        let rewritten = rewrite_unqualified_method_calls(
            "staticHelper(1)",
            &instance,
            &static_,
            false,
            "com.example.Foo",
        );
        assert_eq!(rewritten, "com.example.Foo.staticHelper(1)");
    }

    #[test]
    fn rewrite_unqualified_method_calls_does_not_rewrite_inside_string_literals() {
        let instance: HashSet<String> = ["helper".to_string()].into_iter().collect();
        let static_: HashSet<String> = HashSet::new();
        let rewritten = rewrite_unqualified_method_calls(
            "\"helper(\" + helper(x)",
            &instance,
            &static_,
            true,
            "com.example.Foo",
        );
        assert_eq!(rewritten, "\"helper(\" + __this.helper(x)");
    }

    #[tokio::test]
    async fn generated_helper_compiles_for_primitive_stream_sampling_when_javac_available() {
        if !tool_available("javac") {
            eprintln!("skipping javac compile test: `javac` not available");
            return;
        }

        let tmp = TempDir::new().unwrap();
        let class_name = "NovaStreamEvalHelper_Test";
        let source_path = tmp.path().join(format!("{class_name}.java"));

        // The stage expression returns a *primitive* stream; the generated stage method should
        // compile without callers needing to append `.boxed()` / `.collect(...)`.
        let src = generate_stream_eval_helper_java_source(
            "",
            class_name,
            &[],
            &[("this".to_string(), "Object".to_string())],
            &[],
            &[],
            &["IntStream.range(0, 3)".to_string()],
            None,
            3,
        );
        std::fs::write(&source_path, &src).unwrap();

        let cancel = CancellationToken::new();
        let javac = crate::javac::HotSwapJavacConfig {
            javac: "javac".to_string(),
            release: Some("8".to_string()),
            ..crate::javac::HotSwapJavacConfig::default()
        };

        let compiled = crate::javac::compile_java_for_hot_swap(&cancel, &javac, &source_path).await;
        assert!(
            compiled.is_ok(),
            "expected generated helper source to compile (do we have the right imports and sampleStream implementation?)\nsource:\n{src}\nerror:\n{}",
            compiled.err().unwrap().to_string()
        );
    }

    #[test]
    fn sanitize_java_param_name_handles_tricky_and_invalid_identifiers() {
        let tricky = ["$x", "_x", "x1"];
        for name in tricky {
            let sanitized = sanitize_java_param_name(name);
            assert_eq!(sanitized, name);
            assert!(is_valid_java_ident_ascii(&sanitized));
        }

        let invalid = [("1x", "_1x"), ("foo-bar", "foo_bar")];
        for (name, expected) in invalid {
            let sanitized = sanitize_java_param_name(name);
            assert_eq!(sanitized, expected);
            assert!(is_valid_java_ident_ascii(&sanitized));
            // Stability: applying twice is idempotent.
            assert_eq!(sanitize_java_param_name(name), sanitized);
        }
    }

    #[test]
    fn java_source_generation_includes_package_imports_and_stage_methods() {
        let src = generate_stream_eval_helper_java_source(
            "com.example",
            "__NovaStreamEvalHelper",
            &[
                // Simulate best-effort imports scraped from the paused source file.
                "import com.acme.Foo".to_string(),
                // Exercise whitespace/canonicalization (`import\tstatic` should still work).
                "import\tstatic com.acme.Util.*".to_string(),
                // Duplicates should not produce repeated import lines (semicolons normalized).
                "import com.acme.Foo;".to_string(),
                // Tabbed imports should not cause us to emit `import import\t...;`.
                "import\tcom.acme.Tabbed; // trailing comment".to_string(),
                "import java.util.*;".to_string(),
            ],
            &[
                ("this".to_string(), "com.example.Foo".to_string()),
                ("foo-bar".to_string(), "int".to_string()),
            ],
            &[],
            &[],
            &["this.foo()".to_string(), "this.bar()".to_string()],
            None,
            5,
        );

        assert!(src.contains("package com.example;"));
        // Default imports (including common static imports).
        assert!(src.contains("import java.util.*;"));
        assert!(src.contains("import java.util.stream.*;"));
        assert!(src.contains("import java.util.function.*;"));
        assert!(src.contains("import static java.util.stream.Collectors.*;"));
        // Best-effort file imports should be preserved.
        assert!(src.contains("import com.acme.Foo;"));
        assert!(src.contains("import com.acme.Tabbed;"));
        assert!(src.contains("import static com.acme.Util.*;"));
        // Dedupe (semicolons normalized).
        assert_eq!(src.matches("import com.acme.Foo;").count(), 1);
        assert_eq!(src.matches("import com.acme.Tabbed;").count(), 1);
        assert_eq!(src.matches("import java.util.*;").count(), 1);
        assert!(src.contains("public final class __NovaStreamEvalHelper"));

        assert!(
            src.contains("public static java.util.List<?> sampleStream("),
            "expected sampleStream helper method:\n{src}"
        );

        // Ensure locals are exposed via valid parameter names.
        assert!(src.contains("com.example.Foo __this"));
        assert!(src.contains("int foo_bar"));

        // Ensure stages are generated and `this` is rewritten.
        assert!(src.contains("public static java.util.List<?> stage0"));
        assert!(src.contains("public static java.util.List<?> stage1"));
        assert!(
            src.contains("return sampleStream(__this.foo(), 5);"),
            "expected stage0 to sample stream via helper:\n{src}"
        );
        assert!(
            src.contains("return sampleStream(__this.bar(), 5);"),
            "expected stage1 to sample stream via helper:\n{src}"
        );

        // Imports are emitted deterministically: defaults first, then file imports.
        let idx_default = src.find("import java.util.*;").unwrap();
        let idx_custom = src.find("import com.acme.Foo;").unwrap();
        assert!(
            idx_default < idx_custom,
            "expected default imports before file imports:\n{src}"
        );
    }

    #[test]
    fn sanitize_java_param_name_handles_keywords() {
        assert_eq!(sanitize_java_param_name("class"), "class_");
        assert_eq!(sanitize_java_param_name("return"), "return_");
        assert_eq!(sanitize_java_param_name("this"), "__this");
    }

    #[test]
    fn java_source_generation_emits_void_methods_for_foreach_stages() {
        let src = generate_stream_eval_helper_java_source(
            "com.example",
            "__NovaStreamEvalHelper",
            &[],
            &[
                ("this".to_string(), "Object".to_string()),
                (
                    "s".to_string(),
                    "java.util.stream.Stream<Integer>".to_string(),
                ),
            ],
            &[],
            &[],
            &["s.forEach(System.out::println)".to_string()],
            None,
            10,
        );

        assert!(
            src.contains("public static void stage0"),
            "expected stage0 to be void for forEach:\n{src}"
        );
        assert!(
            !src.contains("return s.forEach"),
            "should not emit `return <void expr>;`:\n{src}"
        );
        assert!(src.contains("s.forEach(System.out::println);"));
    }

    #[test]
    fn java_source_generation_emits_void_methods_for_foreach_ordered_stages() {
        let src = generate_stream_eval_helper_java_source(
            "com.example",
            "__NovaStreamEvalHelper",
            &[],
            &[
                ("this".to_string(), "Object".to_string()),
                (
                    "s".to_string(),
                    "java.util.stream.Stream<Integer>".to_string(),
                ),
            ],
            &[],
            &[],
            &["s.forEachOrdered(System.out::println)".to_string()],
            None,
            10,
        );

        assert!(
            src.contains("public static void stage0"),
            "expected stage0 to be void for forEachOrdered:\n{src}"
        );
        assert!(
            !src.contains("return s.forEachOrdered"),
            "should not emit `return <void expr>;`:\n{src}"
        );
        assert!(src.contains("s.forEachOrdered(System.out::println);"));
    }

    #[test]
    fn java_source_generation_emits_void_methods_for_int_stream_foreach_stages() {
        let src = generate_stream_eval_helper_java_source(
            "com.example",
            "__NovaStreamEvalHelper",
            &[],
            &[("this".to_string(), "Object".to_string())],
            &[],
            &[],
            &["java.util.stream.IntStream.range(0, 3).forEach(System.out::println)".to_string()],
            None,
            10,
        );

        assert!(
            src.contains("public static void stage0"),
            "expected stage0 to be void for IntStream.forEach:\n{src}"
        );
        assert!(
            !src.contains("return java.util.stream.IntStream.range"),
            "should not emit `return <void expr>;`:\n{src}"
        );
        assert!(
            src.contains("java.util.stream.IntStream.range(0, 3).forEach(System.out::println);")
        );
    }

    #[test]
    fn java_source_generation_includes_bound_fields_after_locals() {
        let src = generate_stream_eval_helper_java_source(
            "",
            "__NovaStreamEvalHelper",
            &[],
            &[("this".to_string(), "com.example.Foo".to_string())],
            &[(
                "nums".to_string(),
                "java.util.List<java.lang.Integer>".to_string(),
            )],
            &[(
                "MY_LIST".to_string(),
                "java.util.List<java.lang.Integer>".to_string(),
            )],
            &["nums.stream()".to_string()],
            None,
            100,
        );

        // `__this` always comes first.
        assert!(
            src.contains("public static java.util.List<?> stage0(com.example.Foo __this, java.util.List<java.lang.Integer> nums, java.util.List<java.lang.Integer> MY_LIST)")
        );
        assert!(src.contains("return sampleStream(nums.stream(), 25);"));
    }

    #[test]
    fn java_source_generation_emits_void_methods_for_foreach_even_when_analysis_has_no_terminal() {
        // `mapToInt` is not currently part of the stream analyzer's supported op set. If the
        // analyzer bails before it reaches the terminal `forEach`, we still want to avoid emitting
        // `return <void expr>;` which will not compile.
        let src = generate_stream_eval_helper_java_source(
            "com.example",
            "__NovaStreamEvalHelper",
            &[],
            &[
                ("this".to_string(), "Object".to_string()),
                (
                    "s".to_string(),
                    "java.util.stream.Stream<Integer>".to_string(),
                ),
            ],
            &[],
            &[],
            &[
                r#"s.map(x -> x).mapToInt(x -> x).forEach(x -> System.out.println(")"))"#
                    .to_string(),
            ],
            None,
            10,
        );

        assert!(
            src.contains("public static void stage0"),
            "expected stage0 to be void for forEach:\n{src}"
        );
        assert!(
            !src.contains("return s.map"),
            "should not emit `return <void expr>;`:\n{src}"
        );
        assert!(src
            .contains(r#"s.map(x -> x).mapToInt(x -> x).forEach(x -> System.out.println(")"));"#));
    }

    #[test]
    fn java_source_generation_resolves_param_name_collisions_deterministically() {
        let src = generate_stream_eval_helper_java_source(
            "",
            "__NovaStreamEvalHelper",
            &[],
            &[
                ("this".to_string(), "Foo".to_string()),
                // All of these sanitize to `foo_bar` and must be disambiguated.
                ("foo-bar".to_string(), "int".to_string()),
                ("foo_bar".to_string(), "int".to_string()),
                ("foo bar".to_string(), "int".to_string()),
            ],
            &[],
            &[],
            &["this.toString();".to_string()],
            None,
            3,
        );

        assert!(
            src.contains("stage0(Foo __this, int foo_bar, int foo_bar_2, int foo_bar_3)"),
            "{src}"
        );
        // Ensure trailing semicolons are stripped before wrapping in `return ...;`.
        assert!(
            src.contains("return sampleStream(__this.toString(), 3);"),
            "{src}"
        );
    }

    #[test]
    fn java_source_generation_dedupes_imports() {
        let src = generate_stream_eval_helper_java_source(
            "",
            "__NovaStreamEvalHelper",
            &[
                "java.util.List".to_string(),
                "import java.util.List;".to_string(),
                "java.util.List;".to_string(),
            ],
            &[("this".to_string(), "Object".to_string())],
            &[],
            &[],
            &["this".to_string()],
            None,
            1,
        );

        assert_eq!(src.matches("import java.util.List;").count(), 1, "{src}");
    }

    #[test]
    fn java_source_generation_emits_void_terminal_for_foreach() {
        let src = generate_stream_eval_helper_java_source(
            "com.example",
            "__NovaStreamEvalHelper",
            &[],
            &[
                ("this".to_string(), "Object".to_string()),
                (
                    "s".to_string(),
                    "java.util.stream.Stream<Integer>".to_string(),
                ),
            ],
            &[],
            &[],
            &[],
            Some("s.forEach(System.out::println)"),
            10,
        );

        assert!(
            src.contains("public static void terminal"),
            "expected terminal to be void for forEach:\n{src}"
        );
        assert!(
            !src.contains("return s.forEach"),
            "should not emit `return <void expr>;`:\n{src}"
        );
        assert!(src.contains("s.forEach(System.out::println);"));
    }
}
