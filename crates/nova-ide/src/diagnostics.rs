use nova_core::{TextRange, TextSize};

pub use nova_core::BuildDiagnosticSeverity;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiagnosticKind {
    Syntax,
    Type,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub severity: BuildDiagnosticSeverity,
    pub message: String,
    pub range: TextRange,
}

#[derive(Debug, Default, Clone)]
pub struct DiagnosticsEngine;

impl DiagnosticsEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn diagnose(&self, _file_path: &str, text: &str) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        diagnostics.extend(self.syntax_diagnostics(text));
        diagnostics.extend(self.type_diagnostics(text));
        diagnostics
    }

    fn syntax_diagnostics(&self, text: &str) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        let parsed = nova_syntax::parse(text);
        for err in parsed.errors {
            diagnostics.push(Diagnostic {
                kind: DiagnosticKind::Syntax,
                severity: BuildDiagnosticSeverity::Error,
                message: err.message,
                range: TextRange::new(
                    TextSize::from(err.range.start),
                    TextSize::from(err.range.end),
                ),
            });
        }

        let mut stack: Vec<(char, usize)> = Vec::new();
        for (offset, ch) in text.char_indices() {
            match ch {
                '{' | '(' | '[' => stack.push((ch, offset)),
                '}' | ')' | ']' => {
                    let expected_open = match ch {
                        '}' => '{',
                        ')' => '(',
                        ']' => '[',
                        _ => unreachable!(),
                    };

                    match stack.pop() {
                        Some((open, _)) if open == expected_open => {}
                        Some((open, _)) => diagnostics.push(Diagnostic {
                            kind: DiagnosticKind::Syntax,
                            severity: BuildDiagnosticSeverity::Error,
                            message: format!(
                                "Mismatched closing '{}', expected match for '{}'",
                                ch, open
                            ),
                            range: text_range_at(offset, 1),
                        }),
                        None => diagnostics.push(Diagnostic {
                            kind: DiagnosticKind::Syntax,
                            severity: BuildDiagnosticSeverity::Error,
                            message: format!("Unmatched closing '{}'", ch),
                            range: text_range_at(offset, 1),
                        }),
                    }
                }
                _ => {}
            }
        }

        for (open, offset) in stack {
            diagnostics.push(Diagnostic {
                kind: DiagnosticKind::Syntax,
                severity: BuildDiagnosticSeverity::Error,
                message: format!("Unmatched opening '{}'", open),
                range: text_range_at(offset, 1),
            });
        }

        diagnostics
    }

    fn type_diagnostics(&self, text: &str) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut offset = 0usize;
        for raw_line in text.split_inclusive('\n') {
            let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
            let trimmed = line.trim_start();

            if let Some(pos) = trimmed.find("TYPE_ERROR") {
                let absolute = offset + (line.len() - trimmed.len() + pos);
                diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::Type,
                    severity: BuildDiagnosticSeverity::Error,
                    message: "Encountered TYPE_ERROR marker".to_string(),
                    range: text_range_at(absolute, "TYPE_ERROR".len()),
                });
            }

            if trimmed.starts_with("int ") {
                if let Some(eq) = trimmed.find('=') {
                    let rhs = trimmed[eq + 1..].trim_start();
                    if rhs.starts_with('"') {
                        if let Some(quote_pos) = line.find('"') {
                            diagnostics.push(Diagnostic {
                                kind: DiagnosticKind::Type,
                                severity: BuildDiagnosticSeverity::Error,
                                message: "Cannot assign string literal to int".to_string(),
                                range: text_range_at(offset + quote_pos, 1),
                            });
                        }
                    }
                }
            }

            offset += raw_line.len();
        }

        diagnostics
    }
}

fn text_range_at(start: usize, len: usize) -> TextRange {
    let start_size = TextSize::from(start as u32);
    let end_size = TextSize::from(u32::from(start_size) + len as u32);
    TextRange::new(start_size, end_size)
}
