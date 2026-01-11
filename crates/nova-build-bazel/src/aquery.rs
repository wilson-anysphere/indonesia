use serde::{Deserialize, Serialize};
use std::borrow::Cow;
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
    #[serde(default, alias = "enable_preview")]
    pub preview: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JavacActionInfo {
    pub owner: Option<String>,
    pub compile_info: JavaCompileInfo,
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

pub(crate) fn parse_aquery_textproto_streaming_javac_action_info<R: BufRead>(
    reader: R,
) -> impl Iterator<Item = JavacActionInfo> {
    AqueryTextprotoStreamingJavacInfoParser::new(reader)
}

struct AqueryTextprotoStreamingParser<R: BufRead> {
    reader: R,
    line_buf: String,
    in_action: bool,
    depth: i32,
    is_javac: Option<bool>,
    owner: Option<String>,
    arguments: Vec<String>,
    collect_arguments: bool,
    done: bool,
}

impl<R: BufRead> AqueryTextprotoStreamingParser<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line_buf: String::new(),
            in_action: false,
            depth: 0,
            is_javac: None,
            owner: None,
            arguments: Vec::new(),
            collect_arguments: true,
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
                    self.is_javac = None;
                    self.owner = None;
                    self.arguments.clear();
                    self.collect_arguments = true;

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
                if let Some(value) = parse_quoted_field_raw(trimmed, "mnemonic:") {
                    let is_javac = value == "Javac";
                    self.is_javac = Some(is_javac);
                    self.collect_arguments = is_javac;
                    if !is_javac {
                        // Avoid retaining (potentially huge) argument vectors for actions we're
                        // going to discard anyway.
                        self.arguments.clear();
                        self.owner = None;
                    }
                } else if let Some(value) = parse_quoted_field(trimmed, "owner:") {
                    if self.is_javac != Some(false) {
                        self.owner = Some(value);
                    }
                } else if let Some(value) = parse_quoted_field(trimmed, "arguments:") {
                    if self.collect_arguments {
                        self.arguments.push(value);
                    }
                }
            }

            self.depth += brace_delta_unquoted(trimmed_start);
            if self.depth <= 0 {
                self.in_action = false;
                self.depth = 0;

                if self.is_javac == Some(true) {
                    return Some(JavacAction {
                        owner: self.owner.take(),
                        arguments: std::mem::take(&mut self.arguments),
                    });
                }

                self.is_javac = None;
                self.owner = None;
                self.arguments.clear();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingJavacArg {
    Classpath,
    ModulePath,
    Release,
    Source,
    Target,
    OutputDir,
    SourcePath,
}

struct AqueryTextprotoStreamingJavacInfoParser<R: BufRead> {
    reader: R,
    line_buf: String,
    in_action: bool,
    depth: i32,
    is_javac: Option<bool>,
    owner: Option<String>,
    info: JavaCompileInfo,
    sourcepath_roots: BTreeSet<String>,
    java_file_roots: BTreeSet<String>,
    pending: Option<PendingJavacArg>,
    done: bool,
}

impl<R: BufRead> AqueryTextprotoStreamingJavacInfoParser<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line_buf: String::new(),
            in_action: false,
            depth: 0,
            is_javac: None,
            owner: None,
            info: JavaCompileInfo::default(),
            sourcepath_roots: BTreeSet::new(),
            java_file_roots: BTreeSet::new(),
            pending: None,
            done: false,
        }
    }

}

fn apply_javac_argument(
    pending: &mut Option<PendingJavacArg>,
    info: &mut JavaCompileInfo,
    sourcepath_roots: &mut BTreeSet<String>,
    java_file_roots: &mut BTreeSet<String>,
    arg: &str,
) {
    if let Some(pending) = pending.take() {
        match pending {
            PendingJavacArg::Classpath => {
                info.classpath = split_path_list(arg);
            }
            PendingJavacArg::ModulePath => {
                info.module_path = split_path_list(arg);
            }
            PendingJavacArg::Release => {
                let release = arg.to_string();
                info.release = Some(release.clone());
                info.source = Some(release.clone());
                info.target = Some(release);
            }
            PendingJavacArg::Source => {
                if info.release.is_none() {
                    info.source = Some(arg.to_string());
                }
            }
            PendingJavacArg::Target => {
                if info.release.is_none() {
                    info.target = Some(arg.to_string());
                }
            }
            PendingJavacArg::OutputDir => {
                info.output_dir = Some(arg.to_string());
            }
            PendingJavacArg::SourcePath => {
                for root in split_path_list(arg) {
                    if !root.is_empty() {
                        sourcepath_roots.insert(root);
                    }
                }
            }
        }
        return;
    }

    match arg {
        "-classpath" | "--class-path" => *pending = Some(PendingJavacArg::Classpath),
        "--module-path" => *pending = Some(PendingJavacArg::ModulePath),
        "--release" => *pending = Some(PendingJavacArg::Release),
        "--source" | "-source" => *pending = Some(PendingJavacArg::Source),
        "--target" | "-target" => *pending = Some(PendingJavacArg::Target),
        "-d" => *pending = Some(PendingJavacArg::OutputDir),
        "--enable-preview" => {
            info.preview = true;
        }
        "-sourcepath" | "--source-path" => *pending = Some(PendingJavacArg::SourcePath),
        other => {
            if let Some(release) = other.strip_prefix("--release=") {
                let release = release.to_string();
                info.release = Some(release.clone());
                info.source = Some(release.clone());
                info.target = Some(release);
                return;
            }

            if let Some(output_dir) = other.strip_prefix("-d=") {
                info.output_dir = Some(output_dir.to_string());
                return;
            }

            if other.ends_with(".java") {
                if let Some(parent) = other.rsplit_once('/') {
                    if !java_file_roots.contains(parent.0) {
                        java_file_roots.insert(parent.0.to_string());
                    }
                } else if let Some(parent) = other.rsplit_once('\\') {
                    if !java_file_roots.contains(parent.0) {
                        java_file_roots.insert(parent.0.to_string());
                    }
                }
            }
        }
    }
}

impl<R: BufRead> Iterator for AqueryTextprotoStreamingJavacInfoParser<R> {
    type Item = JavacActionInfo;

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
            let delta = brace_delta_unquoted(trimmed_start);

            if !self.in_action {
                if trimmed_start.starts_with("action {") {
                    self.in_action = true;
                    self.depth = delta;
                    self.is_javac = None;
                    self.owner = None;
                    self.info = JavaCompileInfo::default();
                    self.sourcepath_roots.clear();
                    self.java_file_roots.clear();
                    self.pending = None;

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
                if let Some(value) = parse_quoted_field_raw(trimmed, "mnemonic:") {
                    let is_javac = value == "Javac";
                    self.is_javac = Some(is_javac);
                    if !is_javac {
                        // If we started parsing before the mnemonic was known, drop any partially
                        // collected data immediately to keep peak allocations bounded for
                        // non-Javac actions.
                        self.owner = None;
                        self.info = JavaCompileInfo::default();
                        self.sourcepath_roots.clear();
                        self.java_file_roots.clear();
                        self.pending = None;
                    }
                } else if let Some(value) = parse_quoted_field(trimmed, "owner:") {
                    if self.is_javac != Some(false) {
                        self.owner = Some(value);
                    }
                } else if let Some(raw) = parse_quoted_field_raw(trimmed, "arguments:") {
                    // Don't spend time parsing arguments for non-Javac actions, but allow arguments
                    // before we've seen the mnemonic.
                    if self.is_javac == Some(false) {
                        // skip
                    } else {
                        let value = if raw.contains('\\') {
                            Cow::Owned(unescape_textproto(raw))
                        } else {
                            Cow::Borrowed(raw)
                        };
                        apply_javac_argument(
                            &mut self.pending,
                            &mut self.info,
                            &mut self.sourcepath_roots,
                            &mut self.java_file_roots,
                            value.as_ref(),
                        );
                    }
                }
            }

            self.depth += delta;
            if self.depth <= 0 {
                self.in_action = false;
                self.depth = 0;

                if self.is_javac == Some(true) {
                    let mut info = std::mem::take(&mut self.info);
                    info.source_roots = if !self.sourcepath_roots.is_empty() {
                        std::mem::take(&mut self.sourcepath_roots)
                    } else {
                        std::mem::take(&mut self.java_file_roots)
                    }
                    .into_iter()
                    .collect();
                    self.pending = None;
                    return Some(JavacActionInfo {
                        owner: self.owner.take(),
                        compile_info: info,
                    });
                }

                self.is_javac = None;
                self.owner = None;
                self.info = JavaCompileInfo::default();
                self.sourcepath_roots.clear();
                self.java_file_roots.clear();
                self.pending = None;
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
    if !raw.contains('\\') {
        return Some(raw.to_string());
    }
    Some(unescape_textproto(raw))
}

fn parse_quoted_field_raw<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let line = line.strip_prefix(prefix)?.trim_start();
    let first = line.find('"')?;
    let last = line.rfind('"')?;
    if first == last {
        return None;
    }
    Some(&line[first + 1..last])
}

// textproto strings are C-escaped. For the Bazel outputs we care about, handling a minimal
// subset is enough.
fn unescape_textproto(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }

        let Some(esc) = chars.next() else {
            out.push('\\');
            break;
        };

        match esc {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'x' => {
                // Hex byte escape: \xNN (variable digits, at least 1)
                let mut value: u32 = 0;
                let mut consumed = 0;
                while let Some(&next) = chars.peek() {
                    let Some(hex) = next.to_digit(16) else { break };
                    chars.next();
                    consumed += 1;
                    value = (value << 4) + hex;
                }
                if consumed == 0 {
                    out.push('x');
                } else if let Some(ch) = char::from_u32(value & 0xFF) {
                    out.push(ch);
                }
            }
            'u' | 'U' => {
                // Unicode escape: \uXXXX or \UXXXXXXXX
                let digits = if esc == 'u' { 4 } else { 8 };
                let mut value: u32 = 0;
                let mut ok = true;
                for _ in 0..digits {
                    match chars.next().and_then(|c| c.to_digit(16)) {
                        Some(hex) => value = (value << 4) + hex,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    if let Some(ch) = char::from_u32(value) {
                        out.push(ch);
                    }
                }
            }
            d @ '0'..='7' => {
                // Octal escape: up to 3 digits (including the one we just consumed).
                let mut value: u32 = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().copied().and_then(|c| c.to_digit(8)) {
                        Some(oct) => {
                            chars.next();
                            value = (value << 3) + oct;
                        }
                        None => break,
                    }
                }
                if let Some(ch) = char::from_u32(value & 0xFF) {
                    out.push(ch);
                }
            }
            other => out.push(other),
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
    let mut sourcepath_roots = BTreeSet::<String>::new();
    let mut java_file_roots = BTreeSet::<String>::new();

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
                    info.source = Some(v.clone());
                    info.target = Some(v.clone());
                }
            }
            "--source" | "-source" => {
                if let Some(v) = it.next() {
                    if info.release.is_none() {
                        info.source = Some(v.clone());
                    }
                }
            }
            "--target" | "-target" => {
                if let Some(v) = it.next() {
                    if info.release.is_none() {
                        info.target = Some(v.clone());
                    }
                }
            }
            "-d" => {
                if let Some(v) = it.next() {
                    info.output_dir = Some(v.clone());
                }
            }
            "--enable-preview" => {
                info.preview = true;
            }
            "-sourcepath" | "--source-path" => {
                if let Some(v) = it.next() {
                    for root in split_path_list(v) {
                        if !root.is_empty() {
                            sourcepath_roots.insert(root);
                        }
                    }
                }
            }
            other => {
                if let Some(release) = other.strip_prefix("--release=") {
                    let release = release.to_string();
                    info.release = Some(release.clone());
                    info.source = Some(release.clone());
                    info.target = Some(release);
                    continue;
                }

                if let Some(output_dir) = other.strip_prefix("-d=") {
                    info.output_dir = Some(output_dir.to_string());
                    continue;
                }

                if other.ends_with(".java") {
                    if let Some(parent) = other.rsplit_once('/') {
                        if !java_file_roots.contains(parent.0) {
                            java_file_roots.insert(parent.0.to_string());
                        }
                    } else if let Some(parent) = other.rsplit_once('\\') {
                        if !java_file_roots.contains(parent.0) {
                            java_file_roots.insert(parent.0.to_string());
                        }
                    }
                }
            }
        }
    }

    info.source_roots = if !sourcepath_roots.is_empty() {
        sourcepath_roots.into_iter().collect()
    } else {
        java_file_roots.into_iter().collect()
    };
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

    // On Unix, `javac` uses `:` to separate path entries. On Windows, `javac` uses `;`, but Bazel
    // outputs can still contain `C:\...` / `C:/...` drive-letter paths in places where `:` is used
    // as the separator (e.g. cross-platform fixtures, remote execution metadata).
    //
    // We split on `:` only when it is *not* a drive-letter prefix.
    let bytes = value.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let is_drive_letter = i > start
                && i - start == 1
                && bytes[i - 1].is_ascii_alphabetic()
                && matches!(bytes.get(i + 1).copied(), Some(b'\\') | Some(b'/'));
            if !is_drive_letter {
                let part = &value[start..i];
                if !part.is_empty() {
                    parts.push(part.to_string());
                }
                start = i + 1;
            }
        }
        i += 1;
    }
    let tail = &value[start..];
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

// NOTE: this module intentionally avoids building a full in-memory representation of the
// `bazel aquery` output. The consumer (e.g. `BazelWorkspace`) should scan the stream and retain
// only the actions it needs.
