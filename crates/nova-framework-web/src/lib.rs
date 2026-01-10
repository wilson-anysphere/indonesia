use std::path::{Path, PathBuf};

use regex::Regex;

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

pub fn extract_jaxrs_endpoints_in_dir(project_root: impl AsRef<Path>) -> std::io::Result<Vec<Endpoint>> {
    let project_root = project_root.as_ref();
    let mut java_files = Vec::new();
    collect_java_files(project_root, &mut java_files)?;

    let mut endpoints = Vec::new();
    for file in java_files {
        let content = match std::fs::read_to_string(&file) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let rel = file.strip_prefix(project_root).unwrap_or(&file).to_path_buf();
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
    let method_sig_re = Regex::new(
        r#"^\s*(?:public|protected|private|static|final|\s)+\s*[\w<>\[\].$]+\s+(\w+)\s*\("#,
    )
    .unwrap();

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

            if !http_methods.is_empty() && method_sig_re.is_match(line) {
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

fn consume_leading_annotations<'a>(
    line: &'a str,
    pending: &mut Vec<PendingAnnotation>,
) -> &'a str {
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
        let name = full_name.rsplit('.').next().unwrap_or(full_name).to_string();

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
    let start = args.find('"')?;
    let rest = &args[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_java_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(path);
        }
    }

    Ok(())
}

pub fn looks_like_jaxrs_project(root: &Path) -> bool {
    for file in ["pom.xml", "build.gradle", "build.gradle.kts"] {
        let path = root.join(file);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if content.contains("javax.ws.rs") || content.contains("jakarta.ws.rs") {
                return true;
            }
        }
    }
    false
}
