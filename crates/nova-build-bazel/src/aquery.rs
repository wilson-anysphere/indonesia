use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

/// A parsed `Javac` action from `bazel aquery --output=textproto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavacAction {
    pub owner: Option<String>,
    pub arguments: Vec<String>,
}

/// The compilation settings Nova cares about for Java semantic analysis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaCompileInfo {
    /// The compile classpath entries (jars and directories).
    pub classpath: Vec<String>,
    /// Java module-path entries.
    #[serde(default)]
    pub module_path: Vec<String>,
    /// Source roots (directories containing `.java` sources).
    #[serde(default)]
    pub source_roots: Vec<String>,
    /// `--source` / `-source` version if present.
    #[serde(default)]
    pub source: Option<String>,
    /// `--target` / `-target` version if present.
    #[serde(default)]
    pub target: Option<String>,
}

/// Parse a textproto `aquery` output and return all `Javac` actions.
pub fn parse_aquery_textproto(output: &str) -> Vec<JavacAction> {
    let mut actions = Vec::new();
    let mut current: Option<Vec<String>> = None;
    let mut depth: i32 = 0;
    for line in output.lines() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("action {") {
            depth = 0;
            current = Some(Vec::new());
        }

        if let Some(buf) = current.as_mut() {
            buf.push(line.to_string());

            // Keep track of nested braces in the block. textproto uses braces for nested messages.
            // This is not a complete parser, but it's sufficient to extract repeated scalar fields.
            let open = line.matches('{').count() as i32;
            let close = line.matches('}').count() as i32;
            depth += open - close;
            if depth == 0 {
                let block = std::mem::take(buf);
                current = None;
                if let Some(action) = parse_action_block(&block.join("\n")) {
                    actions.push(action);
                }
            }
        }
    }
    actions
}

fn parse_action_block(block: &str) -> Option<JavacAction> {
    let mut mnemonic = None::<String>;
    let mut owner = None::<String>;
    let mut args = Vec::new();

    for line in block.lines() {
        let line = line.trim();
        if let Some(value) = parse_quoted_field(line, "mnemonic:") {
            mnemonic = Some(value);
        } else if let Some(value) = parse_quoted_field(line, "owner:") {
            owner = Some(value);
        } else if let Some(value) = parse_quoted_field(line, "arguments:") {
            args.push(value);
        }
    }

    if mnemonic.as_deref() != Some("Javac") {
        return None;
    }

    Some(JavacAction {
        owner,
        arguments: args,
    })
}

fn parse_quoted_field(line: &str, prefix: &str) -> Option<String> {
    let line = line.strip_prefix(prefix)?.trim_start();
    let first = line.find('"')?;
    let last = line.rfind('"')?;
    if first == last {
        return None;
    }
    let raw = &line[first + 1..last];
    Some(unescape_textproto(raw))
}

// textproto strings are C-escaped. For the Bazel outputs we care about, handling a minimal
// subset is enough.
fn unescape_textproto(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }

        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Extract the classpath/module-path/source roots from a parsed `Javac` action.
pub fn extract_java_compile_info(action: &JavacAction) -> JavaCompileInfo {
    let mut info = JavaCompileInfo::default();
    let mut source_roots = BTreeSet::<String>::new();

    let mut it = action.arguments.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-classpath" | "--class-path" => {
                if let Some(cp) = it.next() {
                    info.classpath = split_path_list(cp);
                }
            }
            "--module-path" => {
                if let Some(mp) = it.next() {
                    info.module_path = split_path_list(mp);
                }
            }
            "--source" | "-source" => {
                if let Some(v) = it.next() {
                    info.source = Some(v.clone());
                }
            }
            "--target" | "-target" => {
                if let Some(v) = it.next() {
                    info.target = Some(v.clone());
                }
            }
            "-sourcepath" | "--source-path" => {
                if let Some(v) = it.next() {
                    for root in split_path_list(v) {
                        if !root.is_empty() {
                            source_roots.insert(root);
                        }
                    }
                }
            }
            other => {
                if other.ends_with(".java") {
                    if let Some(parent) = other.rsplit_once('/') {
                        source_roots.insert(parent.0.to_string());
                    } else if let Some(parent) = other.rsplit_once('\\') {
                        source_roots.insert(parent.0.to_string());
                    }
                }
            }
        }
    }

    info.source_roots = source_roots.into_iter().collect();
    info
}

fn split_path_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }

    // Prefer `;` if it appears anywhere in the argument. This matches the platform default on
    // Windows and avoids breaking `C:\...` drive letters when we only see a single entry.
    if value.contains(';') {
        return value
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
    }

    if looks_like_windows_absolute_path(value) {
        // Windows absolute paths contain `:` as part of the drive letter (e.g. `C:\...`).
        // Splitting on `:` would incorrectly turn `C:\foo\bar.jar` into `C` and `\foo\bar.jar`.
        //
        // If we can *reliably* detect a colon-separated list of drive-letter paths (rare, but
        // unambiguous), split it. Otherwise treat the entire value as a single path entry.
        if let Some(split) = split_windows_drive_list(value) {
            return split;
        }

        return vec![value.to_string()];
    }

    value
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn looks_like_windows_absolute_path(value: &str) -> bool {
    // UNC path (`\\server\share\...`) or drive-letter path (`C:\...`).
    value.contains("\\\\") || is_windows_drive_absolute_path(value)
}

fn is_windows_drive_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
}

fn split_windows_drive_list(value: &str) -> Option<Vec<String>> {
    // Support colon-separated lists of drive-letter paths like:
    // `C:\a.jar:D:\b.jar`
    //
    // We only split on `:` when it is followed by an unambiguous drive-letter prefix.
    let bytes = value.as_bytes();
    let mut split_points = Vec::new();
    for i in 0..bytes.len().saturating_sub(3) {
        if bytes[i] == b':'
            && bytes[i + 1].is_ascii_alphabetic()
            && bytes[i + 2] == b':'
            && bytes[i + 3] == b'\\'
        {
            split_points.push(i);
        }
    }

    if split_points.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    let mut start = 0;
    for idx in split_points {
        let part = &value[start..idx];
        if !part.is_empty() {
            parts.push(part.to_string());
        }
        start = idx + 1; // skip the `:` separator
    }

    let rest = &value[start..];
    if !rest.is_empty() {
        parts.push(rest.to_string());
    }

    Some(parts)
}

/// Convenience helper: parse a textproto output and return compile info keyed by action owner.
pub fn compile_info_by_owner(output: &str) -> HashMap<String, JavaCompileInfo> {
    let mut map = HashMap::new();
    for action in parse_aquery_textproto(output) {
        if let Some(owner) = action.owner.clone() {
            map.insert(owner, extract_java_compile_info(&action));
        }
    }
    map
}
