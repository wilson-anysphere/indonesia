use anyhow::{Context, Result};
use nova_workspace::{Diagnostic, DiagnosticsReport, Severity};
use serde::Serialize;
use std::path::Path;
/// Output formats supported by `nova diagnostics`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticsFormat {
    /// Human-readable text (default).
    Human,
    /// The raw `DiagnosticsReport` JSON.
    Json,
    /// GitHub Actions workflow commands (`::error ...::...` / `::warning ...::...`).
    Github,
    /// SARIF 2.1.0 JSON.
    Sarif,
}

pub fn print_github_annotations(report: &DiagnosticsReport) {
    for diagnostic in &report.diagnostics {
        let kind = match diagnostic.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };

        let mut message = String::new();
        if let Some(code) = diagnostic.code.as_ref() {
            message.push('[');
            message.push_str(code);
            message.push_str("] ");
        }
        message.push_str(&diagnostic.message);

        let file = github_escape_property(&diagnostic.file.to_string_lossy());
        let message = github_escape_data(&message);

        println!(
            "::{kind} file={file},line={},col={}::{message}",
            diagnostic.line, diagnostic.column
        );
    }
}

pub fn print_sarif(report: &DiagnosticsReport) -> Result<()> {
    let sarif = sarif_log(report);
    let out = serde_json::to_string_pretty(&sarif)?;
    println!("{out}");
    Ok(())
}

pub fn write_sarif(report: &DiagnosticsReport, out_path: &Path) -> Result<()> {
    let sarif = sarif_log(report);
    let bytes = serde_json::to_vec_pretty(&sarif)?;
    nova_cache::atomic_write(out_path, &bytes)
        .with_context(|| format!("failed to write SARIF to {}", out_path.display()))?;
    Ok(())
}

fn github_escape_data(input: &str) -> String {
    input
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn github_escape_property(input: &str) -> String {
    github_escape_data(input).replace(':', "%3A").replace(',', "%2C")
}

fn sarif_log(report: &DiagnosticsReport) -> SarifLog {
    SarifLog {
        schema: "https://json.schemastore.org/sarif-2.1.0.json",
        version: "2.1.0",
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver {
                    name: "Nova",
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            },
            results: report
                .diagnostics
                .iter()
                .map(sarif_result_from_diagnostic)
                .collect(),
        }],
    }
}

fn sarif_result_from_diagnostic(diagnostic: &Diagnostic) -> SarifResult {
    let rule_id = diagnostic
        .code
        .clone()
        .unwrap_or_else(|| "NOVA".to_string());

    let level = match diagnostic.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };

    let uri = diagnostic.file.to_string_lossy().replace('\\', "/");

    SarifResult {
        rule_id,
        level,
        message: SarifMessage {
            text: diagnostic.message.clone(),
        },
        locations: vec![SarifLocation {
            physical_location: SarifPhysicalLocation {
                artifact_location: SarifArtifactLocation { uri },
                region: SarifRegion {
                    start_line: diagnostic.line,
                    start_column: diagnostic.column,
                },
            },
        }],
    }
}

#[derive(Debug, Serialize)]
struct SarifLog {
    #[serde(rename = "$schema")]
    schema: &'static str,
    version: &'static str,
    runs: Vec<SarifRun>,
}

#[derive(Debug, Serialize)]
struct SarifRun {
    tool: SarifTool,
    results: Vec<SarifResult>,
}

#[derive(Debug, Serialize)]
struct SarifTool {
    driver: SarifDriver,
}

#[derive(Debug, Serialize)]
struct SarifDriver {
    name: &'static str,
    version: String,
}

#[derive(Debug, Serialize)]
struct SarifResult {
    #[serde(rename = "ruleId")]
    rule_id: String,
    level: &'static str,
    message: SarifMessage,
    locations: Vec<SarifLocation>,
}

#[derive(Debug, Serialize)]
struct SarifMessage {
    text: String,
}

#[derive(Debug, Serialize)]
struct SarifLocation {
    #[serde(rename = "physicalLocation")]
    physical_location: SarifPhysicalLocation,
}

#[derive(Debug, Serialize)]
struct SarifPhysicalLocation {
    #[serde(rename = "artifactLocation")]
    artifact_location: SarifArtifactLocation,
    region: SarifRegion,
}

#[derive(Debug, Serialize)]
struct SarifArtifactLocation {
    uri: String,
}

#[derive(Debug, Serialize)]
struct SarifRegion {
    #[serde(rename = "startLine")]
    start_line: usize,
    #[serde(rename = "startColumn")]
    start_column: usize,
}

