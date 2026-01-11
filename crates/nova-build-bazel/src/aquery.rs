use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::BufRead;

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
    /// `--release` version if present.
    #[serde(default)]
    pub release: Option<String>,
    /// `-d` output directory if present.
    #[serde(default)]
    pub output_dir: Option<String>,
    /// Whether `--enable-preview` is passed.
    #[serde(default)]
    pub enable_preview: bool,
}

/// Parse a textproto `aquery` output and return all `Javac` actions.
pub fn parse_aquery_textproto(output: &str) -> Vec<JavacAction> {
    parse_aquery_textproto_streaming(std::io::BufReader::new(std::io::Cursor::new(
        output.as_bytes(),
    )))
    .collect()
}

/// Parse `bazel aquery --output=textproto` output in a streaming fashion.
///
/// This iterator scans the output line-by-line and yields `Javac` actions without buffering the
/// entire output string in memory. Only `mnemonic`, `owner`, and `arguments` fields from each
/// `action { ... }` block are retained.
pub fn parse_aquery_textproto_streaming<R: BufRead>(
    reader: R,
) -> impl Iterator<Item = JavacAction> {
    AqueryTextprotoStreamingParser::new(reader)
}

struct AqueryTextprotoStreamingParser<R: BufRead> {
    reader: R,
    line_buf: String,
    in_action: bool,
    depth: i32,
    mnemonic: Option<String>,
    owner: Option<String>,
    arguments: Vec<String>,
    done: bool,
}

impl<R: BufRead> AqueryTextprotoStreamingParser<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line_buf: String::new(),
            in_action: false,
            depth: 0,
            mnemonic: None,
            owner: None,
            arguments: Vec::new(),
            done: false,
        }
    }
}

impl<R: BufRead> Iterator for AqueryTextprotoStreamingParser<R> {
    type Item = JavacAction;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            self.line_buf.clear();
            let bytes = match self.reader.read_line(&mut self.line_buf) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(bytes) => bytes,
                Err(_) => {
                    self.done = true;
                    return None;
                }
            };

            if bytes == 0 {
                self.done = true;
                return None;
            }

            let line = self.line_buf.trim_end_matches(['\n', '\r']);
            let trimmed_start = line.trim_start();

            if !self.in_action {
                if trimmed_start.starts_with("action {") {
                    self.in_action = true;
                    self.depth = brace_delta_unquoted(trimmed_start);
                    self.mnemonic = None;
                    self.owner = None;
                    self.arguments.clear();

                    if self.depth <= 0 {
                        // Malformed (or single-line) action block. Reset and keep scanning.
                        self.in_action = false;
                        self.depth = 0;
                    }
                }
                continue;
            }

            let trimmed = trimmed_start.trim();
            if self.depth == 1 {
                if let Some(value) = parse_quoted_field(trimmed, "mnemonic:") {
                    self.mnemonic = Some(value);
                } else if let Some(value) = parse_quoted_field(trimmed, "owner:") {
                    self.owner = Some(value);
                } else if let Some(value) = parse_quoted_field(trimmed, "arguments:") {
                    self.arguments.push(value);
                }
            }

            self.depth += brace_delta_unquoted(trimmed_start);
            if self.depth <= 0 {
                self.in_action = false;
                self.depth = 0;

                if self.mnemonic.as_deref() == Some("Javac") {
                    return Some(JavacAction {
                        owner: self.owner.take(),
                        arguments: std::mem::take(&mut self.arguments),
                    });
                }

                self.mnemonic = None;
                self.owner = None;
                self.arguments.clear();
            }
        }
    }
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

fn brace_delta_unquoted(line: &str) -> i32 {
    let mut delta = 0i32;
    let mut in_string = false;
    let mut escape = false;

    for c in line.chars() {
        if in_string {
            if escape {
                escape = false;
                continue;
            }

            match c {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match c {
            '"' => in_string = true,
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }

    delta
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
            "--release" => {
                if let Some(v) = it.next() {
                    info.release = Some(v.clone());
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
            "-d" => {
                if let Some(v) = it.next() {
                    info.output_dir = Some(v.clone());
                }
            }
            "--enable-preview" => {
                info.enable_preview = true;
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
                if let Some(release) = other.strip_prefix("--release=") {
                    info.release = Some(release.to_string());
                    continue;
                }

                if let Some(output_dir) = other.strip_prefix("-d=") {
                    info.output_dir = Some(output_dir.to_string());
                    continue;
                }

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

// NOTE: this module intentionally avoids building a full in-memory representation of the
// `bazel aquery` output. The consumer (e.g. `BazelWorkspace`) should scan the stream and retain
// only the actions it needs.
