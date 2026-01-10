use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use walkdir::WalkDir;

/// A minimal, library-first backend for the `nova` CLI.
///
/// This is intentionally lightweight: it provides basic project loading,
/// indexing, diagnostics, and cache management without requiring an editor or
/// LSP transport.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Open a workspace rooted at `path`.
    ///
    /// If `path` is a file, its parent directory is treated as the workspace root.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let meta = fs::metadata(path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        let root = if meta.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(|p| p.to_path_buf())
                .context("file path has no parent directory")?
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join(".nova-cache")
    }

    fn java_files_in(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in WalkDir::new(root).follow_links(true) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("java") {
                files.push(path.to_path_buf());
            }
        }
        files.sort();
        Ok(files)
    }

    pub fn index(&self) -> Result<IndexReport> {
        self.build_index(self.root.as_path())
    }

    /// Index a project and persist the resulting artifacts into `.nova-cache`.
    pub fn index_and_write_cache(&self) -> Result<IndexReport> {
        let report = self.index()?;
        self.write_cache_index(&report.index)?;
        self.write_cache_perf(&report.metrics)?;
        Ok(report)
    }

    fn build_index(&self, root: &Path) -> Result<IndexReport> {
        let start = Instant::now();

        let type_re = Regex::new(
            r"(?m)^\s*(?:public|protected|private)?\s*(?:abstract\s+|final\s+)?(class|interface|enum|record)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )?;
        let method_re = Regex::new(
            r"(?m)^\s*(?:public|protected|private)?\s*(?:static\s+)?(?:final\s+)?(?:synchronized\s+)?[A-Za-z0-9_<>,\[\]\s]+\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
        )?;

        let mut symbols = Vec::new();
        let mut files_scanned = 0usize;
        let mut bytes_scanned = 0u64;

        for file in self.java_files_in(root)? {
            files_scanned += 1;
            let content = fs::read_to_string(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            bytes_scanned += content.len() as u64;
            for cap in type_re.captures_iter(&content) {
                let kind = match cap.get(1).map(|m| m.as_str()) {
                    Some("class") => SymbolKind::Class,
                    Some("interface") => SymbolKind::Interface,
                    Some("enum") => SymbolKind::Enum,
                    Some("record") => SymbolKind::Record,
                    _ => SymbolKind::Unknown,
                };
                let m = cap.get(2).expect("regex capture");
                let (line, column) = line_col_at(&content, m.start());
                symbols.push(Symbol {
                    name: m.as_str().to_string(),
                    kind,
                    file: file.clone(),
                    line,
                    column,
                });
            }
            for cap in method_re.captures_iter(&content) {
                let m = cap.get(1).expect("regex capture");
                let (line, column) = line_col_at(&content, m.start());
                symbols.push(Symbol {
                    name: m.as_str().to_string(),
                    kind: SymbolKind::Method,
                    file: file.clone(),
                    line,
                    column,
                });
            }
        }

        let elapsed = start.elapsed();
        let index = Index { symbols };
        let metrics = PerfMetrics {
            files_scanned,
            bytes_scanned,
            symbols_indexed: index.symbols.len(),
            elapsed_ms: elapsed.as_millis(),
        };
        Ok(IndexReport {
            root: root.to_path_buf(),
            index,
            metrics,
        })
    }

    pub fn diagnostics(&self, path: impl AsRef<Path>) -> Result<DiagnosticsReport> {
        let path = path.as_ref();
        let meta = fs::metadata(path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        let mut diagnostics = Vec::new();

        let files = if meta.is_dir() {
            self.java_files_in(path)?
        } else {
            vec![path.to_path_buf()]
        };

        for file in files {
            let content = fs::read_to_string(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;

            // Parse-like delimiter checks (cheap but useful in CI smoke tests).
            let parse = parse_brace_tree(&content);
            for err in parse.errors {
                diagnostics.push(Diagnostic {
                    file: file.clone(),
                    line: err.line,
                    column: err.column,
                    severity: Severity::Error,
                    code: Some("PARSE".to_string()),
                    message: err.message,
                });
            }

            // Heuristic diagnostics: TODO/FIXME markers.
            for (needle, sev, code) in [
                ("TODO", Severity::Warning, "TODO"),
                ("FIXME", Severity::Warning, "FIXME"),
            ] {
                for (line_idx, line) in content.lines().enumerate() {
                    if let Some(col) = line.find(needle) {
                        diagnostics.push(Diagnostic {
                            file: file.clone(),
                            line: line_idx + 1,
                            column: col + 1,
                            severity: sev,
                            code: Some(code.to_string()),
                            message: format!("found {}", needle),
                        });
                    }
                }
            }
        }

        let summary = DiagnosticsSummary::from_diagnostics(&diagnostics);
        Ok(DiagnosticsReport {
            root: self.root.clone(),
            diagnostics,
            summary,
        })
    }

    pub fn workspace_symbols(&self, query: &str) -> Result<Vec<Symbol>> {
        // Prefer cached index if it exists, falling back to an in-memory scan.
        let index = if self.cache_index_path().exists() {
            match fs::read_to_string(self.cache_index_path()) {
                Ok(data) => serde_json::from_str::<Index>(&data).unwrap_or_else(|_| Index {
                    symbols: Vec::new(),
                }),
                Err(_) => Index {
                    symbols: Vec::new(),
                },
            }
        } else {
            self.index()?.index
        };

        let q = query.to_lowercase();
        Ok(index
            .symbols
            .into_iter()
            .filter(|s| s.name.to_lowercase().contains(&q))
            .collect())
    }

    pub fn parse_file(&self, file: impl AsRef<Path>) -> Result<ParseResult> {
        let file = file.as_ref();
        let content = fs::read_to_string(file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        Ok(parse_brace_tree(&content))
    }

    pub fn cache_status(&self) -> Result<CacheStatus> {
        let cache_dir = self.cache_dir();
        let index_path = self.cache_index_path();
        let perf_path = self.cache_perf_path();

        let mut status = CacheStatus {
            cache_dir,
            exists: false,
            index_path,
            index_bytes: None,
            symbols_indexed: None,
            perf_path,
            perf_bytes: None,
            last_perf: None,
        };

        status.exists = status.cache_dir.exists();

        if let Ok(meta) = fs::metadata(&status.index_path) {
            status.index_bytes = Some(meta.len());
            if let Ok(data) = fs::read_to_string(&status.index_path) {
                if let Ok(index) = serde_json::from_str::<Index>(&data) {
                    status.symbols_indexed = Some(index.symbols.len());
                }
            }
        }

        if let Ok(meta) = fs::metadata(&status.perf_path) {
            status.perf_bytes = Some(meta.len());
            if let Ok(data) = fs::read_to_string(&status.perf_path) {
                if let Ok(perf) = serde_json::from_str::<PerfMetrics>(&data) {
                    status.last_perf = Some(perf);
                }
            }
        }

        Ok(status)
    }

    pub fn cache_clean(&self) -> Result<()> {
        let dir = self.cache_dir();
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to remove {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn cache_warm(&self) -> Result<IndexReport> {
        fs::create_dir_all(self.cache_dir())
            .with_context(|| format!("failed to create {}", self.cache_dir().display()))?;
        self.index_and_write_cache()
    }

    pub fn perf_report(&self) -> Result<Option<PerfMetrics>> {
        let path = self.cache_perf_path();
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(Some(
            serde_json::from_str::<PerfMetrics>(&data)
                .with_context(|| format!("failed to parse {}", path.display()))?,
        ))
    }

    fn cache_index_path(&self) -> PathBuf {
        self.cache_dir().join("index.json")
    }

    fn cache_perf_path(&self) -> PathBuf {
        self.cache_dir().join("perf.json")
    }

    fn write_cache_index(&self, index: &Index) -> Result<()> {
        fs::create_dir_all(self.cache_dir())
            .with_context(|| format!("failed to create {}", self.cache_dir().display()))?;
        let path = self.cache_index_path();
        let json = serde_json::to_string_pretty(index)?;
        fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    fn write_cache_perf(&self, metrics: &PerfMetrics) -> Result<()> {
        fs::create_dir_all(self.cache_dir())
            .with_context(|| format!("failed to create {}", self.cache_dir().display()))?;
        let path = self.cache_perf_path();
        let json = serde_json::to_string_pretty(metrics)?;
        fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    pub root: PathBuf,
    pub index: Index,
    pub metrics: PerfMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfMetrics {
    pub files_scanned: usize,
    pub bytes_scanned: u64,
    pub symbols_indexed: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Class,
    Interface,
    Enum,
    Record,
    Method,
    Unknown,
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Enum => "enum",
            SymbolKind::Record => "record",
            SymbolKind::Method => "method",
            SymbolKind::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsReport {
    pub root: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
    pub summary: DiagnosticsSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsSummary {
    pub errors: usize,
    pub warnings: usize,
}

impl DiagnosticsSummary {
    fn from_diagnostics(diagnostics: &[Diagnostic]) -> Self {
        let mut errors = 0usize;
        let mut warnings = 0usize;
        for d in diagnostics {
            match d.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
            }
        }
        Self { errors, warnings }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStatus {
    pub cache_dir: PathBuf,
    pub exists: bool,
    pub index_path: PathBuf,
    pub index_bytes: Option<u64>,
    pub symbols_indexed: Option<usize>,
    pub perf_path: PathBuf,
    pub perf_bytes: Option<u64>,
    pub last_perf: Option<PerfMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseResult {
    pub tree: String,
    pub errors: Vec<ParseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    LineComment,
    BlockComment,
    String,
    Char,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delim {
    Brace,
    Paren,
    Bracket,
}

fn delim_for_open(c: char) -> Option<Delim> {
    match c {
        '{' => Some(Delim::Brace),
        '(' => Some(Delim::Paren),
        '[' => Some(Delim::Bracket),
        _ => None,
    }
}

fn delim_for_close(c: char) -> Option<Delim> {
    match c {
        '}' => Some(Delim::Brace),
        ')' => Some(Delim::Paren),
        ']' => Some(Delim::Bracket),
        _ => None,
    }
}

fn delim_name(d: Delim) -> &'static str {
    match d {
        Delim::Brace => "brace",
        Delim::Paren => "paren",
        Delim::Bracket => "bracket",
    }
}

fn parse_brace_tree(text: &str) -> ParseResult {
    let mut mode = Mode::Normal;
    let mut stack: Vec<(Delim, usize)> = Vec::new();
    let mut events = Vec::new();
    let mut errors = Vec::new();

    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let c = b as char;

        match mode {
            Mode::Normal => {
                if c == '/' && i + 1 < bytes.len() {
                    let next = bytes[i + 1] as char;
                    if next == '/' {
                        mode = Mode::LineComment;
                        i += 2;
                        continue;
                    }
                    if next == '*' {
                        mode = Mode::BlockComment;
                        i += 2;
                        continue;
                    }
                }

                if c == '"' {
                    mode = Mode::String;
                    i += 1;
                    continue;
                }
                if c == '\'' {
                    mode = Mode::Char;
                    i += 1;
                    continue;
                }

                if let Some(d) = delim_for_open(c) {
                    let depth = stack.len();
                    stack.push((d, i));
                    let (line, col) = line_col_at(text, i);
                    events.push(format!(
                        "{:indent$}open {} @ {}:{}",
                        "",
                        delim_name(d),
                        line,
                        col,
                        indent = depth * 2
                    ));
                } else if let Some(close) = delim_for_close(c) {
                    let depth = stack.len();
                    match stack.pop() {
                        Some((open, open_idx)) if open == close => {
                            let (line, col) = line_col_at(text, i);
                            events.push(format!(
                                "{:indent$}close {} @ {}:{}",
                                "",
                                delim_name(close),
                                line,
                                col,
                                indent = (depth.saturating_sub(1)) * 2
                            ));
                        }
                        Some((open, open_idx)) => {
                            // Put the opener back and continue (best-effort recovery).
                            stack.push((open, open_idx));
                            let (line, col) = line_col_at(text, i);
                            errors.push(ParseError {
                                message: format!(
                                    "mismatched closing {}",
                                    delim_name(close)
                                ),
                                line,
                                column: col,
                            });
                        }
                        None => {
                            let (line, col) = line_col_at(text, i);
                            errors.push(ParseError {
                                message: format!("unmatched closing {}", delim_name(close)),
                                line,
                                column: col,
                            });
                        }
                    }
                }
            }
            Mode::LineComment => {
                if c == '\n' {
                    mode = Mode::Normal;
                }
            }
            Mode::BlockComment => {
                if c == '*' && i + 1 < bytes.len() && bytes[i + 1] as char == '/' {
                    mode = Mode::Normal;
                    i += 2;
                    continue;
                }
            }
            Mode::String => {
                if c == '\\' && i + 1 < bytes.len() {
                    // Skip escaped character.
                    i += 2;
                    continue;
                }
                if c == '"' {
                    mode = Mode::Normal;
                }
            }
            Mode::Char => {
                if c == '\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    mode = Mode::Normal;
                }
            }
        }

        i += 1;
    }

    for (d, idx) in stack.into_iter().rev() {
        let (line, col) = line_col_at(text, idx);
        errors.push(ParseError {
            message: format!("unclosed {}", delim_name(d)),
            line,
            column: col,
        });
    }

    let mut tree = String::new();
    tree.push_str("delimiters:\n");
    for e in events {
        tree.push_str(&e);
        tree.push('\n');
    }

    ParseResult { tree, errors }
}

fn line_col_at(text: &str, byte_idx: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    let mut idx = 0usize;

    for ch in text.chars() {
        if idx >= byte_idx {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
        idx += ch.len_utf8();
    }

    (line, col)
}
