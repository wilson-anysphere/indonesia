use nova_build_model::AnnotationProcessingConfig;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io::BufRead;
use std::path::PathBuf;

use crate::command::read_line_limited;

// Bazel `aquery` output can contain very large single-line arguments (e.g. long classpaths).
// Avoid unbounded `read_line` buffering by enforcing a maximum line size, and cap the retained
// buffer capacity so we don't permanently hold onto multi-megabyte allocations.
const MAX_LINE_BUF_CAPACITY_BYTES: usize = 1024 * 1024;
const MAX_AQUERY_LINE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

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
    #[serde(default, alias = "output_directory")]
    pub output_dir: Option<String>,
    /// Whether `--enable-preview` is passed.
    #[serde(default, alias = "enable_preview")]
    pub preview: bool,

    /// Annotation processing configuration extracted from `javac` flags (`-processorpath`, `-A...`,
    /// `-s`, etc).
    #[serde(default)]
    pub annotation_processing: Option<AnnotationProcessingConfig>,
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
    line_buf: Vec<u8>,
    max_line_bytes: usize,
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
            line_buf: Vec::new(),
            max_line_bytes: MAX_AQUERY_LINE_BYTES,
            in_action: false,
            depth: 0,
            is_javac: None,
            owner: None,
            arguments: Vec::new(),
            collect_arguments: true,
            done: false,
        }
    }

    #[cfg(test)]
    fn new_with_max_line_bytes(reader: R, max_line_bytes: usize) -> Self {
        let mut parser = Self::new(reader);
        parser.max_line_bytes = max_line_bytes;
        parser
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
            if self.line_buf.capacity() > MAX_LINE_BUF_CAPACITY_BYTES {
                self.line_buf.shrink_to(MAX_LINE_BUF_CAPACITY_BYTES);
            }
            let bytes = match read_line_limited(
                &mut self.reader,
                &mut self.line_buf,
                self.max_line_bytes,
                "bazel aquery",
            ) {
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

            let text = match std::str::from_utf8(&self.line_buf) {
                Ok(text) => text,
                Err(_) => {
                    self.done = true;
                    return None;
                }
            };
            let line = text.trim_end_matches(['\n', '\r']);
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
    ProcessorPath,
    Processor,
    GeneratedSourcesDir,
}

#[derive(Debug, Default)]
struct AptState {
    config: AnnotationProcessingConfig,
    saw_flag: bool,
    proc_mode: Option<String>,
    compiler_args: Vec<String>,
}

struct AqueryTextprotoStreamingJavacInfoParser<R: BufRead> {
    reader: R,
    line_buf: Vec<u8>,
    max_line_bytes: usize,
    in_action: bool,
    depth: i32,
    is_javac: Option<bool>,
    owner: Option<String>,
    info: JavaCompileInfo,
    sourcepath_roots: BTreeSet<String>,
    java_file_roots: BTreeSet<String>,
    apt: AptState,
    pending: Option<PendingJavacArg>,
    done: bool,
}

impl<R: BufRead> AqueryTextprotoStreamingJavacInfoParser<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line_buf: Vec::new(),
            max_line_bytes: MAX_AQUERY_LINE_BYTES,
            in_action: false,
            depth: 0,
            is_javac: None,
            owner: None,
            info: JavaCompileInfo::default(),
            sourcepath_roots: BTreeSet::new(),
            java_file_roots: BTreeSet::new(),
            apt: AptState::default(),
            pending: None,
            done: false,
        }
    }

    #[cfg(test)]
    fn new_with_max_line_bytes(reader: R, max_line_bytes: usize) -> Self {
        let mut parser = Self::new(reader);
        parser.max_line_bytes = max_line_bytes;
        parser
    }
}

fn apply_javac_argument(
    pending: &mut Option<PendingJavacArg>,
    info: &mut JavaCompileInfo,
    sourcepath_roots: &mut BTreeSet<String>,
    java_file_roots: &mut BTreeSet<String>,
    apt: &mut AptState,
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
                // Once a `-sourcepath` is provided we no longer need to track source roots from
                // individual `.java` arguments.
                java_file_roots.clear();
                for root in split_path_list(arg) {
                    if !root.is_empty() {
                        sourcepath_roots.insert(root);
                    }
                }
            }
            PendingJavacArg::ProcessorPath => {
                apt.saw_flag = true;
                apt.compiler_args.push(arg.to_string());
                apt.config.processor_path.extend(
                    split_path_list(arg)
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .map(PathBuf::from),
                );
            }
            PendingJavacArg::Processor => {
                apt.saw_flag = true;
                apt.compiler_args.push(arg.to_string());
                apt.config.processors.extend(
                    arg.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
            PendingJavacArg::GeneratedSourcesDir => {
                apt.saw_flag = true;
                apt.compiler_args.push(arg.to_string());
                if !arg.is_empty() {
                    apt.config.generated_sources_dir = Some(PathBuf::from(arg));
                }
            }
        }
        return;
    }

    match arg {
        "-classpath" | "--class-path" | "-cp" => *pending = Some(PendingJavacArg::Classpath),
        "--module-path" | "-p" => *pending = Some(PendingJavacArg::ModulePath),
        "--release" => *pending = Some(PendingJavacArg::Release),
        "--source" | "-source" => *pending = Some(PendingJavacArg::Source),
        "--target" | "-target" => *pending = Some(PendingJavacArg::Target),
        "-d" => *pending = Some(PendingJavacArg::OutputDir),
        "-processorpath" | "--processor-path" => {
            apt.saw_flag = true;
            apt.compiler_args.push(arg.to_string());
            *pending = Some(PendingJavacArg::ProcessorPath);
        }
        "-processor" => {
            apt.saw_flag = true;
            apt.compiler_args.push(arg.to_string());
            *pending = Some(PendingJavacArg::Processor);
        }
        "-s" => {
            apt.saw_flag = true;
            apt.compiler_args.push(arg.to_string());
            *pending = Some(PendingJavacArg::GeneratedSourcesDir);
        }
        "--enable-preview" => {
            info.preview = true;
        }
        "-sourcepath" | "--source-path" => *pending = Some(PendingJavacArg::SourcePath),
        other => {
            if let Some(classpath) = other
                .strip_prefix("-classpath=")
                .or_else(|| other.strip_prefix("--class-path="))
                .or_else(|| other.strip_prefix("-cp="))
            {
                info.classpath = split_path_list(classpath);
                return;
            }

            if let Some(module_path) = other
                .strip_prefix("--module-path=")
                .or_else(|| other.strip_prefix("-p="))
            {
                info.module_path = split_path_list(module_path);
                return;
            }

            if other.starts_with("-proc:") {
                apt.saw_flag = true;
                apt.proc_mode = Some(other.trim_start_matches("-proc:").to_string());
                apt.compiler_args.push(other.to_string());
                return;
            }

            if other.starts_with("-A") {
                apt.saw_flag = true;
                apt.compiler_args.push(other.to_string());
                let rest = other.trim_start_matches("-A");
                let (k, v) = rest.split_once('=').unwrap_or((rest, ""));
                if !k.is_empty() {
                    apt.config.options.insert(k.to_string(), v.to_string());
                }
                return;
            }

            if let Some(release) = other.strip_prefix("--release=") {
                let release = release.to_string();
                info.release = Some(release.clone());
                info.source = Some(release.clone());
                info.target = Some(release);
                return;
            }

            if let Some(source) = other
                .strip_prefix("--source=")
                .or_else(|| other.strip_prefix("-source="))
            {
                if info.release.is_none() {
                    info.source = Some(source.to_string());
                }
                return;
            }

            if let Some(target) = other
                .strip_prefix("--target=")
                .or_else(|| other.strip_prefix("-target="))
            {
                if info.release.is_none() {
                    info.target = Some(target.to_string());
                }
                return;
            }

            if let Some(output_dir) = other.strip_prefix("-d=") {
                info.output_dir = Some(output_dir.to_string());
                return;
            }

            if let Some(sourcepath) = other
                .strip_prefix("-sourcepath=")
                .or_else(|| other.strip_prefix("--source-path="))
            {
                // Once a sourcepath is provided we no longer need to track source roots from
                // individual `.java` arguments.
                java_file_roots.clear();
                for root in split_path_list(sourcepath) {
                    if !root.is_empty() {
                        sourcepath_roots.insert(root);
                    }
                }
                return;
            }

            if other.ends_with(".java") {
                if !sourcepath_roots.is_empty() {
                    return;
                }
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
            if self.line_buf.capacity() > MAX_LINE_BUF_CAPACITY_BYTES {
                self.line_buf.shrink_to(MAX_LINE_BUF_CAPACITY_BYTES);
            }
            let bytes = match read_line_limited(
                &mut self.reader,
                &mut self.line_buf,
                self.max_line_bytes,
                "bazel aquery",
            ) {
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

            let text = match std::str::from_utf8(&self.line_buf) {
                Ok(text) => text,
                Err(_) => {
                    self.done = true;
                    return None;
                }
            };
            let line = text.trim_end_matches(['\n', '\r']);
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
                    self.apt = AptState::default();
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
                        self.apt = AptState::default();
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
                            &mut self.apt,
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

                    let apt = std::mem::take(&mut self.apt);
                    let has_apt = apt.saw_flag
                        || apt.proc_mode.is_some()
                        || !apt.config.processor_path.is_empty()
                        || !apt.config.processors.is_empty()
                        || !apt.config.options.is_empty()
                        || apt.config.generated_sources_dir.is_some();
                    if has_apt {
                        let mut cfg = apt.config;
                        cfg.compiler_args = apt.compiler_args;
                        cfg.enabled = match apt.proc_mode.as_deref() {
                            Some("none") => false,
                            Some(_) => true,
                            None => true,
                        };

                        let mut seen_processors = std::collections::HashSet::new();
                        cfg.processors.retain(|p| seen_processors.insert(p.clone()));
                        let mut seen_paths = std::collections::HashSet::new();
                        cfg.processor_path.retain(|p| seen_paths.insert(p.clone()));

                        info.annotation_processing = Some(cfg);
                    }

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
                self.apt = AptState::default();
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
    extract_java_compile_info_from_args(&action.arguments)
}

pub(crate) fn extract_java_compile_info_from_args(args: &[String]) -> JavaCompileInfo {
    let mut info = JavaCompileInfo::default();
    let mut sourcepath_roots = BTreeSet::<String>::new();
    let mut java_file_roots = BTreeSet::<String>::new();
    let mut apt_config = AnnotationProcessingConfig::default();
    let mut saw_apt_flag = false;
    let mut proc_mode = None::<String>;
    let mut apt_args = Vec::<String>::new();

    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-classpath" | "--class-path" | "-cp" => {
                if let Some(cp) = it.next() {
                    info.classpath = split_path_list(cp);
                }
            }
            "--module-path" | "-p" => {
                if let Some(mp) = it.next() {
                    info.module_path = split_path_list(mp);
                }
            }
            "--release" => {
                if let Some(v) = it.next() {
                    let release = v.clone();
                    info.release = Some(release.clone());
                    info.source = Some(release.clone());
                    info.target = Some(release);
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
            "-processorpath" | "--processor-path" => {
                if let Some(value) = it.next() {
                    apt_config
                        .processor_path
                        .extend(split_path_list(value).into_iter().map(PathBuf::from));
                    saw_apt_flag = true;
                    apt_args.push(arg.clone());
                    apt_args.push(value.clone());
                }
            }
            "-processor" => {
                if let Some(value) = it.next() {
                    apt_config.processors.extend(
                        value
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                    saw_apt_flag = true;
                    apt_args.push(arg.clone());
                    apt_args.push(value.clone());
                }
            }
            "-s" => {
                if let Some(value) = it.next() {
                    apt_config.generated_sources_dir = Some(PathBuf::from(value));
                    saw_apt_flag = true;
                    apt_args.push(arg.clone());
                    apt_args.push(value.clone());
                }
            }
            "--enable-preview" => {
                info.preview = true;
            }
            "-sourcepath" | "--source-path" => {
                if let Some(v) = it.next() {
                    // `-sourcepath` overrides implicit source roots derived from `.java` arguments.
                    java_file_roots.clear();
                    for root in split_path_list(v) {
                        if !root.is_empty() {
                            sourcepath_roots.insert(root);
                        }
                    }
                }
            }
            other => {
                if let Some(classpath) = other
                    .strip_prefix("-classpath=")
                    .or_else(|| other.strip_prefix("--class-path="))
                    .or_else(|| other.strip_prefix("-cp="))
                {
                    info.classpath = split_path_list(classpath);
                    continue;
                }

                if let Some(module_path) = other
                    .strip_prefix("--module-path=")
                    .or_else(|| other.strip_prefix("-p="))
                {
                    info.module_path = split_path_list(module_path);
                    continue;
                }
                if other.starts_with("-proc:") {
                    proc_mode = Some(other.trim_start_matches("-proc:").to_string());
                    saw_apt_flag = true;
                    apt_args.push(arg.clone());
                    continue;
                }

                if other.starts_with("-A") {
                    let rest = other.trim_start_matches("-A");
                    let (k, v) = rest.split_once('=').unwrap_or((rest, ""));
                    if !k.is_empty() {
                        apt_config.options.insert(k.to_string(), v.to_string());
                    }
                    saw_apt_flag = true;
                    apt_args.push(arg.clone());
                    continue;
                }

                if let Some(release) = other.strip_prefix("--release=") {
                    let release = release.to_string();
                    info.release = Some(release.clone());
                    info.source = Some(release.clone());
                    info.target = Some(release);
                    continue;
                }

                if let Some(source) = other
                    .strip_prefix("--source=")
                    .or_else(|| other.strip_prefix("-source="))
                {
                    if info.release.is_none() {
                        info.source = Some(source.to_string());
                    }
                    continue;
                }

                if let Some(target) = other
                    .strip_prefix("--target=")
                    .or_else(|| other.strip_prefix("-target="))
                {
                    if info.release.is_none() {
                        info.target = Some(target.to_string());
                    }
                    continue;
                }

                if let Some(output_dir) = other.strip_prefix("-d=") {
                    info.output_dir = Some(output_dir.to_string());
                    continue;
                }

                if let Some(sourcepath) = other
                    .strip_prefix("-sourcepath=")
                    .or_else(|| other.strip_prefix("--source-path="))
                {
                    // Once a sourcepath is provided we no longer need to track source roots from
                    // individual `.java` arguments.
                    java_file_roots.clear();
                    for root in split_path_list(sourcepath) {
                        if !root.is_empty() {
                            sourcepath_roots.insert(root);
                        }
                    }
                    continue;
                }

                if other.ends_with(".java") {
                    if !sourcepath_roots.is_empty() {
                        continue;
                    }
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

    let has_apt = saw_apt_flag
        || proc_mode.is_some()
        || !apt_config.processor_path.is_empty()
        || !apt_config.processors.is_empty()
        || !apt_config.options.is_empty()
        || apt_config.generated_sources_dir.is_some();
    if has_apt {
        apt_config.compiler_args = apt_args;
        apt_config.enabled = match proc_mode.as_deref() {
            Some("none") => false,
            Some(_) => true,
            None => true,
        };

        let mut seen_processors = std::collections::HashSet::new();
        apt_config
            .processors
            .retain(|p| seen_processors.insert(p.clone()));
        let mut seen_paths = std::collections::HashSet::new();
        apt_config
            .processor_path
            .retain(|p| seen_paths.insert(p.clone()));
        info.annotation_processing = Some(apt_config);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_javac_options_into_compile_info() {
        let args = vec![
            "--module-path".to_string(),
            "mods/a:mods/b".to_string(),
            "--release".to_string(),
            "21".to_string(),
            "--enable-preview".to_string(),
            "-sourcepath".to_string(),
            "src/main/java:src/test/java".to_string(),
            "-d".to_string(),
            "out/classes".to_string(),
        ];

        let info = extract_java_compile_info_from_args(&args);
        assert_eq!(
            info.module_path,
            vec!["mods/a".to_string(), "mods/b".to_string()]
        );
        assert_eq!(info.release.as_deref(), Some("21"));
        assert_eq!(info.source.as_deref(), Some("21"));
        assert_eq!(info.target.as_deref(), Some("21"));
        assert!(info.preview);
        assert_eq!(
            info.source_roots,
            vec!["src/main/java".to_string(), "src/test/java".to_string()]
        );
        assert_eq!(info.output_dir.as_deref(), Some("out/classes"));
    }

    #[test]
    fn java_compile_info_json_aliases_are_supported() {
        // Cache files historically used `output_directory` / `enable_preview` field names.
        let json = r#"
        {
            "classpath": ["cp.jar"],
            "output_directory": "out/classes",
            "enable_preview": true
        }
        "#;

        let info: JavaCompileInfo =
            serde_json::from_str(json).expect("deserialize JavaCompileInfo");
        assert_eq!(info.output_dir.as_deref(), Some("out/classes"));
        assert!(info.preview);
    }

    #[test]
    fn java_compile_info_json_roundtrip_preserves_annotation_processing() {
        let apt = AnnotationProcessingConfig {
            enabled: true,
            generated_sources_dir: Some(std::path::PathBuf::from("gen")),
            processor_path: vec![std::path::PathBuf::from("proc.jar")],
            processors: vec!["com.example.Proc".to_string()],
            options: std::collections::BTreeMap::from([(
                "key".to_string(),
                "value".to_string(),
            )]),
            compiler_args: vec![
                "-processorpath".to_string(),
                "proc.jar".to_string(),
                "-Akey=value".to_string(),
                "-s".to_string(),
                "gen".to_string(),
            ],
        };

        let info = JavaCompileInfo {
            classpath: vec!["cp.jar".to_string()],
            module_path: Vec::new(),
            source_roots: Vec::new(),
            source: Some("21".to_string()),
            target: Some("21".to_string()),
            release: Some("21".to_string()),
            output_dir: Some("out/classes".to_string()),
            preview: true,
            annotation_processing: Some(apt),
        };

        let json = serde_json::to_string(&info).expect("serialize JavaCompileInfo");
        let decoded: JavaCompileInfo =
            serde_json::from_str(&json).expect("deserialize JavaCompileInfo");
        assert_eq!(decoded, info);
    }

    #[test]
    fn streaming_parsers_stop_on_overlong_lines() {
        // Use a small per-line limit so this test doesn't need to allocate a huge
        // synthetic aquery output. The production limit is intentionally much larger.
        let line_limit = 64;

        let long_arg = "A".repeat(512);
        let output = format!("action {{\n  mnemonic: \"Javac\"\n  arguments: \"{long_arg}\"\n}}\n");

        let reader = std::io::BufReader::new(std::io::Cursor::new(output.as_bytes()));
        let mut actions =
            AqueryTextprotoStreamingParser::new_with_max_line_bytes(reader, line_limit);
        assert!(actions.next().is_none());

        let reader = std::io::BufReader::new(std::io::Cursor::new(output.as_bytes()));
        let mut infos =
            AqueryTextprotoStreamingJavacInfoParser::new_with_max_line_bytes(reader, line_limit);
        assert!(infos.next().is_none());
    }
}
