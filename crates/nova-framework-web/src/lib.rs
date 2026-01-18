use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

static JAXRS_METHOD_SIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:public|protected|private|static|final|\s)+\s*[\w<>\[\].$]+\s+(\w+)\s*\("#)
        .unwrap()
});

static JAVA_METHOD_SIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:(?:public|protected|private|static|final|synchronized|abstract|default)\s+)*[A-Za-z0-9_<>\[\].$,@?&\s,]+?\s+[A-Za-z_][A-Za-z0-9_]*\s*\("#)
        .unwrap()
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub path: String,
    pub methods: Vec<String>,
    pub handler: HandlerLocation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerLocation {
    pub file: Option<PathBuf>,
    /// 1-based line number.
    pub line: u32,
}

pub fn extract_jaxrs_endpoints(sources: &[&str]) -> Vec<Endpoint> {
    sources
        .iter()
        .flat_map(|src| extract_jaxrs_endpoints_from_source(src, None))
        .collect()
}

/// Convenience wrapper for [`extract_http_endpoints_from_source`] when source file paths are
/// unavailable.
pub fn extract_http_endpoints_from_sources(sources: &[&str]) -> Vec<Endpoint> {
    sources
        .iter()
        .flat_map(|src| extract_http_endpoints_from_source(src, None))
        .collect()
}

/// Extract HTTP endpoints from Java sources across supported web frameworks.
///
/// This is a best-effort line-based extractor intended for lightweight tooling
/// (e.g. Nova's LSP extensions). It currently supports:
/// - JAX-RS (`@Path`, `@GET`, ...)
/// - Spring MVC (`@RequestMapping`, `@GetMapping`, ...)
/// - Micronaut (`@Controller`, `@Get`, ...)
pub fn extract_http_endpoints(sources: &[(&str, Option<PathBuf>)]) -> Vec<Endpoint> {
    sources
        .iter()
        .flat_map(|(src, file)| extract_http_endpoints_from_source(src, file.clone()))
        .collect()
}

pub fn extract_http_endpoints_from_source(source: &str, file: Option<PathBuf>) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    endpoints.extend(extract_jaxrs_endpoints_from_source(source, file.clone()));
    endpoints.extend(extract_spring_mvc_endpoints_from_source(
        source,
        file.clone(),
    ));
    endpoints.extend(extract_micronaut_endpoints_from_source(source, file));
    endpoints
}

pub fn extract_http_endpoints_in_dir(
    project_root: impl AsRef<Path>,
) -> std::io::Result<Vec<Endpoint>> {
    let project_root = project_root.as_ref();
    let mut java_files = Vec::new();
    collect_java_files(project_root, &mut java_files)?;
    java_files.sort();
    java_files.dedup();

    let mut endpoints = Vec::new();
    for file in java_files {
        let content = match std::fs::read_to_string(&file) {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::debug!(
                    path = %file.display(),
                    err = %err,
                    "failed to read Java source while extracting HTTP endpoints"
                );
                continue;
            }
        };

        let rel = file
            .strip_prefix(project_root)
            .unwrap_or(&file)
            .to_path_buf();
        endpoints.extend(extract_http_endpoints_from_source(&content, Some(rel)));
    }

    Ok(endpoints)
}

pub fn extract_jaxrs_endpoints_in_dir(
    project_root: impl AsRef<Path>,
) -> std::io::Result<Vec<Endpoint>> {
    let project_root = project_root.as_ref();
    let mut java_files = Vec::new();
    collect_java_files(project_root, &mut java_files)?;
    java_files.sort();
    java_files.dedup();

    let mut endpoints = Vec::new();
    for file in java_files {
        let content = match std::fs::read_to_string(&file) {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::debug!(
                    path = %file.display(),
                    err = %err,
                    "failed to read Java source while extracting JAX-RS endpoints"
                );
                continue;
            }
        };

        let rel = file
            .strip_prefix(project_root)
            .unwrap_or(&file)
            .to_path_buf();
        endpoints.extend(extract_jaxrs_endpoints_from_source(&content, Some(rel)));
    }

    Ok(endpoints)
}

#[derive(Debug, Clone)]
struct PendingAnnotation {
    name: String,
    args: Option<String>,
}

fn extract_jaxrs_endpoints_from_source(source: &str, file: Option<PathBuf>) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    let mut pending_annotations: Vec<PendingAnnotation> = Vec::new();
    let mut class_path: Option<String> = None;
    let mut brace_depth: i32 = 0;
    let mut in_class = false;

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let line_no_comment = raw_line.split("//").next().unwrap_or(raw_line);
        let mut line = line_no_comment;

        line = consume_leading_annotations(line, &mut pending_annotations);
        if line.trim().is_empty() {
            continue;
        }

        if !in_class {
            if line.contains(" class ") || line.trim_start().starts_with("class ") {
                class_path = pending_annotations
                    .iter()
                    .find(|ann| ann.name == "Path")
                    .and_then(|ann| ann.args.as_deref().and_then(extract_first_string_literal));
                pending_annotations.clear();
                in_class = true;
            }
        } else if brace_depth == 1 {
            let http_methods: Vec<String> = pending_annotations
                .iter()
                .filter_map(|ann| match ann.name.as_str() {
                    "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" => {
                        Some(ann.name.clone())
                    }
                    _ => None,
                })
                .collect();

            if !http_methods.is_empty() && JAXRS_METHOD_SIG_RE.is_match(line) {
                let method_path = pending_annotations
                    .iter()
                    .find(|ann| ann.name == "Path")
                    .and_then(|ann| ann.args.as_deref().and_then(extract_first_string_literal));

                let full_path = join_paths(class_path.as_deref(), method_path.as_deref());

                endpoints.push(Endpoint {
                    path: full_path,
                    methods: http_methods,
                    handler: HandlerLocation {
                        file: file.clone(),
                        line: line_no,
                    },
                });
            }

            pending_annotations.clear();
        } else if brace_depth < 1 {
            pending_annotations.clear();
        }

        brace_depth += count_braces(line);
    }

    endpoints
}

pub fn extract_spring_mvc_endpoints(sources: &[(&str, Option<PathBuf>)]) -> Vec<Endpoint> {
    sources
        .iter()
        .flat_map(|(src, file)| extract_spring_mvc_endpoints_from_source(src, file.clone()))
        .collect()
}

fn extract_spring_mvc_endpoints_from_source(source: &str, file: Option<PathBuf>) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    let mut pending_annotations: Vec<PendingAnnotation> = Vec::new();

    let mut class_base_paths: Vec<String> = Vec::new();
    let mut in_class = false;
    let mut class_body_started = false;
    let mut brace_depth: i32 = 0;

    // Best-effort check to reduce false positives.
    let mut is_controller_class = false;

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let line_no_comment = raw_line.split("//").next().unwrap_or(raw_line);
        let mut line = line_no_comment;

        line = consume_leading_annotations(line, &mut pending_annotations);
        if line.trim().is_empty() {
            continue;
        }

        if !in_class {
            if looks_like_java_class_decl(line) {
                is_controller_class = pending_annotations.iter().any(|ann| {
                    matches!(
                        ann.name.as_str(),
                        "RestController" | "Controller" | "RequestMapping"
                    )
                });
                class_base_paths = pending_annotations
                    .iter()
                    .find(|ann| ann.name == "RequestMapping")
                    .and_then(|ann| ann.args.as_deref().map(extract_spring_mapping_paths))
                    .unwrap_or_else(Vec::new);
                pending_annotations.clear();
                in_class = true;
                class_body_started = false;
            }
        } else if brace_depth == 1 {
            if is_controller_class && JAVA_METHOD_SIG_RE.is_match(line) {
                if let Some(mapping) = parse_spring_method_mapping(&pending_annotations) {
                    let base_paths: Vec<Option<&str>> = if class_base_paths.is_empty() {
                        vec![None]
                    } else {
                        class_base_paths.iter().map(|p| Some(p.as_str())).collect()
                    };
                    let method_paths: Vec<Option<&str>> = if mapping.paths.is_empty() {
                        vec![None]
                    } else {
                        mapping.paths.iter().map(|p| Some(p.as_str())).collect()
                    };
                    for base_path in base_paths {
                        for method_path in &method_paths {
                            let full_path = join_paths(base_path, *method_path);
                            endpoints.push(Endpoint {
                                path: full_path,
                                methods: mapping.methods.clone(),
                                handler: HandlerLocation {
                                    file: file.clone(),
                                    line: line_no,
                                },
                            });
                        }
                    }
                }
            }
            pending_annotations.clear();
        }

        brace_depth += count_braces(line);
        if in_class && !class_body_started && brace_depth > 0 {
            // Handle the "Allman" brace style where `{` is on the next line.
            class_body_started = true;
        }
        if in_class && class_body_started && brace_depth <= 0 {
            // Reset once we leave the class body so multiple classes per file still work.
            in_class = false;
            is_controller_class = false;
            class_base_paths.clear();
            class_body_started = false;
            pending_annotations.clear();
        }
    }

    endpoints
}

struct ParsedMapping {
    methods: Vec<String>,
    paths: Vec<String>,
}

fn parse_spring_method_mapping(annotations: &[PendingAnnotation]) -> Option<ParsedMapping> {
    for ann in annotations {
        let (method, path_keys) = match ann.name.as_str() {
            "GetMapping" => ("GET", &["path", "value"][..]),
            "PostMapping" => ("POST", &["path", "value"][..]),
            "PutMapping" => ("PUT", &["path", "value"][..]),
            "DeleteMapping" => ("DELETE", &["path", "value"][..]),
            "PatchMapping" => ("PATCH", &["path", "value"][..]),
            _ => continue,
        };

        let paths = ann
            .args
            .as_deref()
            .map(|args| extract_mapping_paths(args, path_keys))
            .unwrap_or_else(Vec::new);
        return Some(ParsedMapping {
            methods: vec![method.to_string()],
            paths,
        });
    }

    for ann in annotations {
        if ann.name != "RequestMapping" {
            continue;
        }

        let methods = spring_request_mapping_methods(ann.args.as_deref());
        let paths = ann
            .args
            .as_deref()
            .map(|args| extract_mapping_paths(args, &["path", "value"]))
            .unwrap_or_else(Vec::new);

        return Some(ParsedMapping { methods, paths });
    }

    None
}

fn spring_request_mapping_methods(args: Option<&str>) -> Vec<String> {
    let mut methods = Vec::new();
    if let Some(args) = args {
        let mut rest = args;
        while let Some(pos) = rest.find("RequestMethod.") {
            rest = &rest[pos + "RequestMethod.".len()..];
            let end = rest.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
            if end == 0 {
                continue;
            }
            let method = rest[..end].to_ascii_uppercase();
            if is_http_method(&method) && !methods.iter().any(|m| m == &method) {
                methods.push(method);
            }
            rest = &rest[end..];
        }
    }

    if methods.is_empty() {
        // No explicit method restriction in @RequestMapping => all methods.
        all_http_methods()
    } else {
        methods
    }
}

fn extract_spring_mapping_paths(args: &str) -> Vec<String> {
    extract_mapping_paths(args, &["path", "value"])
}

pub fn extract_micronaut_endpoints(sources: &[(&str, Option<PathBuf>)]) -> Vec<Endpoint> {
    sources
        .iter()
        .flat_map(|(src, file)| extract_micronaut_endpoints_from_source(src, file.clone()))
        .collect()
}

fn extract_micronaut_endpoints_from_source(source: &str, file: Option<PathBuf>) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    let mut pending_annotations: Vec<PendingAnnotation> = Vec::new();

    let mut class_base_paths: Vec<String> = Vec::new();
    let mut in_class = false;
    let mut class_body_started = false;
    let mut brace_depth: i32 = 0;
    let mut is_controller_class = false;

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let line_no_comment = raw_line.split("//").next().unwrap_or(raw_line);
        let mut line = line_no_comment;

        line = consume_leading_annotations(line, &mut pending_annotations);
        if line.trim().is_empty() {
            continue;
        }

        if !in_class {
            if looks_like_java_class_decl(line) {
                is_controller_class = pending_annotations
                    .iter()
                    .any(|ann| ann.name == "Controller");
                class_base_paths = pending_annotations
                    .iter()
                    .find(|ann| ann.name == "Controller")
                    .and_then(|ann| ann.args.as_deref().map(extract_micronaut_mapping_paths))
                    .unwrap_or_else(Vec::new);
                pending_annotations.clear();
                in_class = true;
                class_body_started = false;
            }
        } else if brace_depth == 1 {
            if is_controller_class && JAVA_METHOD_SIG_RE.is_match(line) {
                if let Some(mapping) = parse_micronaut_method_mapping(&pending_annotations) {
                    let base_paths: Vec<Option<&str>> = if class_base_paths.is_empty() {
                        vec![None]
                    } else {
                        class_base_paths.iter().map(|p| Some(p.as_str())).collect()
                    };
                    let method_paths: Vec<Option<&str>> = if mapping.paths.is_empty() {
                        vec![None]
                    } else {
                        mapping.paths.iter().map(|p| Some(p.as_str())).collect()
                    };
                    for base_path in base_paths {
                        for method_path in &method_paths {
                            let full_path = join_paths(base_path, *method_path);
                            endpoints.push(Endpoint {
                                path: full_path,
                                methods: mapping.methods.clone(),
                                handler: HandlerLocation {
                                    file: file.clone(),
                                    line: line_no,
                                },
                            });
                        }
                    }
                }
            }
            pending_annotations.clear();
        }

        brace_depth += count_braces(line);
        if in_class && !class_body_started && brace_depth > 0 {
            class_body_started = true;
        }
        if in_class && class_body_started && brace_depth <= 0 {
            in_class = false;
            is_controller_class = false;
            class_base_paths.clear();
            class_body_started = false;
            pending_annotations.clear();
        }
    }

    endpoints
}

fn parse_micronaut_method_mapping(annotations: &[PendingAnnotation]) -> Option<ParsedMapping> {
    for ann in annotations {
        let method = match ann.name.as_str() {
            "Get" => "GET",
            "Post" => "POST",
            "Put" => "PUT",
            "Delete" => "DELETE",
            "Patch" => "PATCH",
            "Head" => "HEAD",
            "Options" => "OPTIONS",
            _ => continue,
        };
        let path = ann
            .args
            .as_deref()
            .map(|args| extract_mapping_paths(args, &["uri", "value"]))
            .unwrap_or_else(Vec::new);
        return Some(ParsedMapping {
            methods: vec![method.to_string()],
            paths: path,
        });
    }
    None
}

fn extract_micronaut_mapping_paths(args: &str) -> Vec<String> {
    extract_mapping_paths(args, &["uri", "value"])
}

fn join_paths(class_path: Option<&str>, method_path: Option<&str>) -> String {
    let mut path = String::new();
    if let Some(cp) = class_path {
        path.push_str(cp.trim());
    }
    if let Some(mp) = method_path {
        let mp = mp.trim();
        if !mp.is_empty() {
            if !path.ends_with('/') && !mp.starts_with('/') {
                path.push('/');
            }
            if path.ends_with('/') && mp.starts_with('/') {
                path.pop();
            }
            path.push_str(mp);
        }
    }
    if path.is_empty() {
        "/".to_string()
    } else if !path.starts_with('/') {
        format!("/{}", path)
    } else {
        path
    }
}

fn count_braces(line: &str) -> i32 {
    let open = line.chars().filter(|c| *c == '{').count() as i32;
    let close = line.chars().filter(|c| *c == '}').count() as i32;
    open - close
}

fn consume_leading_annotations<'a>(line: &'a str, pending: &mut Vec<PendingAnnotation>) -> &'a str {
    let bytes = line.as_bytes();
    let mut idx = 0usize;

    loop {
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() || bytes[idx] != b'@' {
            break;
        }
        idx += 1;

        let start_name = idx;
        while idx < bytes.len()
            && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'.')
        {
            idx += 1;
        }
        if start_name == idx {
            break;
        }
        let full_name = &line[start_name..idx];
        let name = full_name
            .rsplit('.')
            .next()
            .unwrap_or(full_name)
            .to_string();

        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }

        let args = if idx < bytes.len() && bytes[idx] == b'(' {
            idx += 1;
            let start_args = idx;
            let mut depth = 1i32;
            while idx < bytes.len() && depth > 0 {
                match bytes[idx] {
                    b'(' => {
                        depth += 1;
                        idx += 1;
                    }
                    b')' => {
                        depth -= 1;
                        idx += 1;
                    }
                    b'"' => {
                        idx += 1;
                        while idx < bytes.len() {
                            if bytes[idx] == b'\\' {
                                idx = (idx + 2).min(bytes.len());
                                continue;
                            }
                            if bytes[idx] == b'"' {
                                idx += 1;
                                break;
                            }
                            idx += 1;
                        }
                    }
                    _ => idx += 1,
                }
            }
            let end_args = idx.saturating_sub(1);
            let args_raw = line.get(start_args..end_args).unwrap_or("").trim();
            if args_raw.is_empty() {
                None
            } else {
                Some(args_raw.to_string())
            }
        } else {
            None
        };

        pending.push(PendingAnnotation { name, args });
    }

    line.get(idx..).unwrap_or("")
}

fn extract_first_string_literal(args: &str) -> Option<String> {
    extract_string_literals(args).into_iter().next()
}

fn extract_string_literals(input: &str) -> Vec<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] != b'"' {
            idx += 1;
            continue;
        }
        let start = idx + 1;
        idx += 1;
        while idx < bytes.len() {
            match bytes[idx] {
                b'\\' => idx = (idx + 2).min(bytes.len()),
                b'"' => {
                    if let Some(value) = input.get(start..idx) {
                        out.push(value.to_string());
                    }
                    idx += 1;
                    break;
                }
                _ => idx += 1,
            }
        }
    }
    out
}

fn skip_string_literal(bytes: &[u8], mut idx: usize) -> usize {
    // Skip starting quote.
    idx += 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\\' => idx = (idx + 2).min(bytes.len()),
            b'"' => return idx + 1,
            _ => idx += 1,
        }
    }
    bytes.len()
}

fn slice_until_top_level_comma(s: &str, start: usize) -> &str {
    let bytes = s.as_bytes();
    let mut idx = start;
    let mut paren_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut bracket_depth = 0i32;

    while idx < bytes.len() {
        match bytes[idx] {
            b'"' => {
                idx = skip_string_literal(bytes, idx);
                continue;
            }
            b'(' => paren_depth += 1,
            b')' => paren_depth = (paren_depth - 1).max(0),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = (brace_depth - 1).max(0),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = (bracket_depth - 1).max(0),
            b',' if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 => break,
            _ => {}
        }
        idx += 1;
    }

    s.get(start..idx).unwrap_or("")
}

fn extract_mapping_paths(args: &str, keys: &[&str]) -> Vec<String> {
    for key in keys {
        let values = extract_named_string_literals(args, key);
        if !values.is_empty() {
            return values;
        }
    }

    let bytes = args.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    if idx >= bytes.len() {
        return Vec::new();
    }

    let expr = slice_until_top_level_comma(args, idx);
    extract_string_literals(expr)
}

fn extract_named_string_literals(args: &str, key: &str) -> Vec<String> {
    let bytes = args.as_bytes();
    let key_bytes = key.as_bytes();

    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'"' {
            idx = skip_string_literal(bytes, idx);
            continue;
        }
        if idx + key_bytes.len() > bytes.len() {
            break;
        }
        if &bytes[idx..idx + key_bytes.len()] != key_bytes {
            idx += 1;
            continue;
        }

        let before_ok = idx == 0 || !is_ident_byte(bytes[idx - 1]);
        let after_idx = idx + key_bytes.len();
        let after_ok = after_idx == bytes.len() || !is_ident_byte(bytes[after_idx]);
        if !before_ok || !after_ok {
            idx += key_bytes.len();
            continue;
        }

        let mut j = after_idx;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'=' {
            idx += key_bytes.len();
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }

        let expr = slice_until_top_level_comma(args, j);
        return extract_string_literals(expr);
    }

    Vec::new()
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_http_method(method: &str) -> bool {
    matches!(
        method,
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" | "TRACE"
    )
}

fn all_http_methods() -> Vec<String> {
    vec![
        "GET".to_string(),
        "HEAD".to_string(),
        "POST".to_string(),
        "PUT".to_string(),
        "PATCH".to_string(),
        "DELETE".to_string(),
        "OPTIONS".to_string(),
        "TRACE".to_string(),
    ]
}

fn looks_like_java_class_decl(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("class ")
        || line.contains(" class ")
        || line.starts_with("interface ")
        || line.contains(" interface ")
        || line.starts_with("record ")
        || line.contains(" record ")
        || line.starts_with("enum ")
        || line.contains(" enum ")
}

fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Ignore common noise directories.
            let ignore = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| matches!(name, ".git" | "target" | "build" | "out"));
            if ignore {
                continue;
            }
            collect_java_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(path);
        }
    }

    Ok(())
}

pub fn looks_like_jaxrs_project(root: &Path) -> bool {
    let mut logged_read_error = false;
    for file in ["pom.xml", "build.gradle", "build.gradle.kts"] {
        let path = root.join(file);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => Some(content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                if !logged_read_error {
                    tracing::debug!(
                        target = "nova.framework.web",
                        path = %path.display(),
                        error = %err,
                        "failed to read build file while checking for JAX-RS usage"
                    );
                    logged_read_error = true;
                }
                None
            }
        };
        if content.as_ref().is_some_and(|content| {
            content.contains("javax.ws.rs") || content.contains("jakarta.ws.rs")
        }) {
            return true;
        }
    }
    false
}
