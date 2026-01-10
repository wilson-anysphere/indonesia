//! Diagnostics primitives shared across Nova.

use crate::{FileId, TextRange};

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct Location {
    pub file: FileId,
    pub range: TextRange,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RelatedDiagnostic {
    pub location: Location,
    pub message: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Diagnostic {
    pub range: TextRange,
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
    pub related: Vec<RelatedDiagnostic>,
}

impl Diagnostic {
    pub fn new(range: TextRange, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            range,
            severity,
            code: None,
            message: message.into(),
            related: Vec::new(),
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn add_related(&mut self, location: Location, message: impl Into<String>) {
        self.related.push(RelatedDiagnostic {
            location,
            message: message.into(),
        });
    }
}
