use anyhow::Context as _;
use serde::Serialize;

pub const JSON_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticLevel {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

impl Diagnostic {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Error,
            code: code.into(),
            message: message.into(),
            file: None,
            line: None,
            suggestion: None,
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Warning,
            code: code.into(),
            message: message.into(),
            file: None,
            line: None,
            suggestion: None,
        }
    }

    pub fn with_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    pub fn with_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }
}

#[derive(Debug, Serialize)]
pub struct JsonReport {
    pub schema_version: u32,
    pub command: String,
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
}

impl JsonReport {
    pub fn new(command: impl Into<String>, ok: bool, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            schema_version: JSON_SCHEMA_VERSION,
            command: command.into(),
            ok,
            diagnostics,
        }
    }
}

pub fn print_human(command: &str, ok: bool, diagnostics: &[Diagnostic]) {
    print_diagnostics(diagnostics);

    if ok {
        println!("{command}: ok");
    } else {
        eprintln!("{command}: failed");
    }
}

pub fn print_diagnostics(diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        let loc = match (&diag.file, diag.line) {
            (Some(file), Some(line)) => format!("{file}:{line}"),
            (Some(file), None) => file.clone(),
            _ => "<unknown>".to_string(),
        };
        let level = match diag.level {
            DiagnosticLevel::Error => "error",
            DiagnosticLevel::Warning => "warning",
        };

        eprintln!("{level}[{}]: {}", diag.code, diag.message);
        if diag.file.is_some() || diag.line.is_some() {
            eprintln!("  at: {loc}");
        }
        if let Some(suggestion) = &diag.suggestion {
            eprintln!("  suggestion:\n{suggestion}");
        }
    }
}

pub fn print_json(report: &JsonReport) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(report).context("failed to serialize JSON output")?;
    println!("{json}");
    Ok(())
}
