use std::collections::{BTreeMap, BTreeSet, HashMap};

use async_trait::async_trait;
use nova_ai::patch::{parse_structured_patch, Patch, PatchParseError};
use nova_ai::safety::{
    enforce_no_new_imports, enforce_patch_safety, PatchSafetyConfig, SafetyError,
};
use nova_ai::workspace::{AppliedPatch, PatchApplyConfig, PatchApplyError, VirtualWorkspace};
use nova_ai::CancellationToken;
use nova_ai::{enforce_code_edit_policy, CodeEditPolicyError};
use nova_config::AiPrivacyConfig;
use nova_core::{LineIndex, TextRange};
use nova_ide::diagnostics::{Diagnostic, DiagnosticKind, DiagnosticSeverity, DiagnosticsEngine};
use nova_ide::format::Formatter;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ValidationConfig {
    pub max_new_syntax_errors: usize,
    pub max_new_type_errors: usize,
    pub context_lines: usize,
    pub format: bool,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            max_new_syntax_errors: 0,
            max_new_type_errors: 0,
            context_lines: 2,
            format: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodeGenerationConfig {
    pub safety: PatchSafetyConfig,
    pub validation: ValidationConfig,
    pub max_repair_attempts: usize,
    pub allow_repair: bool,
}

impl Default for CodeGenerationConfig {
    fn default() -> Self {
        Self {
            safety: PatchSafetyConfig::default(),
            validation: ValidationConfig::default(),
            max_repair_attempts: 2,
            allow_repair: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodeGenerationResult {
    pub patch: Patch,
    pub applied: AppliedPatch,
    pub formatted_workspace: VirtualWorkspace,
}

#[derive(Debug, Clone)]
pub enum ErrorFeedback {
    PatchFormat(String),
    PatchApply(String),
    Validation(ErrorReport),
}

#[derive(Debug, Clone)]
pub struct ErrorReport {
    pub new_diagnostics: Vec<DiagnosticWithContext>,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct DiagnosticWithContext {
    pub file: String,
    pub diagnostic: Diagnostic,
    pub position: nova_core::Position,
    pub context: String,
}

impl ErrorReport {
    pub fn to_prompt_block(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.summary);
        out.push('\n');
        for diag in &self.new_diagnostics {
            out.push_str(&format!(
                "{}:{}:{}: {}: {}\n",
                diag.file,
                diag.position.line + 1,
                diag.position.character + 1,
                match diag.diagnostic.severity {
                    DiagnosticSeverity::Error => "error",
                    DiagnosticSeverity::Warning => "warning",
                    DiagnosticSeverity::Information => "info",
                    DiagnosticSeverity::Hint => "hint",
                },
                diag.diagnostic.message
            ));
            out.push_str(&diag.context);
            out.push('\n');
        }
        out
    }
}

#[derive(Debug, Clone)]
pub enum CodegenProgressStage {
    /// Building the full prompt (base context + safety constraints + error feedback).
    BuildingPrompt,
    /// Calling the model to get the next candidate patch.
    ModelCall,
    ParsingPatch,
    ApplyingPatch,
    Formatting,
    Validating,
    /// Reported at the start of each attempt (0 = initial attempt).
    RepairAttempt,
}

#[derive(Debug, Clone)]
pub struct CodegenProgressEvent {
    pub stage: CodegenProgressStage,
    /// 0-indexed attempt counter (0 = initial attempt).
    pub attempt: usize,
    pub message: String,
}

pub trait CodegenProgressReporter: Send + Sync {
    fn report(&self, event: CodegenProgressEvent);
}

#[derive(Debug, Error, Clone)]
pub enum PromptCompletionError {
    #[error("request cancelled")]
    Cancelled,
    #[error("provider error: {0}")]
    Provider(String),
}

#[async_trait]
pub trait PromptCompletionProvider: Send + Sync {
    async fn complete(
        &self,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<String, PromptCompletionError>;
}

#[async_trait]
impl PromptCompletionProvider for nova_ai::AiClient {
    async fn complete(
        &self,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<String, PromptCompletionError> {
        let request = nova_ai::ChatRequest {
            messages: vec![nova_ai::ChatMessage::user(prompt.to_string())],
            max_tokens: None,
        };
        self.chat(request, cancel.clone())
            .await
            .map_err(|err| match err {
                nova_ai::AiError::Cancelled => PromptCompletionError::Cancelled,
                other => PromptCompletionError::Provider(other.to_string()),
            })
    }
}

#[async_trait]
impl PromptCompletionProvider for nova_ai::CloudLlmClient {
    async fn complete(
        &self,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<String, PromptCompletionError> {
        const DEFAULT_MAX_TOKENS: u32 = 1024;
        const DEFAULT_TEMPERATURE: f32 = 0.2;

        let request = nova_ai::cloud::GenerateRequest {
            prompt: prompt.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: DEFAULT_TEMPERATURE,
        };

        self.generate(request, cancel.clone())
            .await
            .map_err(|err| match err {
                nova_ai::cloud::CloudLlmError::Cancelled => PromptCompletionError::Cancelled,
                other => PromptCompletionError::Provider(other.to_string()),
            })
    }
}

#[derive(Debug, Error)]
pub enum CodeGenerationError {
    #[error("operation cancelled")]
    Cancelled,
    #[error(transparent)]
    Policy(#[from] CodeEditPolicyError),
    #[error(transparent)]
    Provider(#[from] PromptCompletionError),
    #[error(transparent)]
    PatchParse(#[from] PatchParseError),
    #[error(transparent)]
    Safety(#[from] SafetyError),
    #[error(transparent)]
    Apply(#[from] PatchApplyError),
    #[error("validation failed: {report:?}")]
    ValidationFailed { report: ErrorReport },
}

pub async fn generate_patch(
    provider: &dyn PromptCompletionProvider,
    workspace: &VirtualWorkspace,
    base_prompt: &str,
    config: &CodeGenerationConfig,
    privacy: &AiPrivacyConfig,
    cancel: &CancellationToken,
    progress: Option<&dyn CodegenProgressReporter>,
) -> Result<CodeGenerationResult, CodeGenerationError> {
    enforce_code_edit_policy(privacy)?;

    let engine = DiagnosticsEngine::new();
    let formatter = Formatter::default();

    let mut attempt = 0usize;
    let mut feedback: Option<ErrorFeedback> = None;

    loop {
        if cancel.is_cancelled() {
            return Err(CodeGenerationError::Cancelled);
        }

        if let Some(progress) = progress {
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::RepairAttempt,
                attempt,
                message: format!("Attempt {}", attempt + 1),
            });
        }
        if cancel.is_cancelled() {
            return Err(CodeGenerationError::Cancelled);
        }

        let prompt = build_prompt(base_prompt, config, feedback.as_ref());
        if let Some(progress) = progress {
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::BuildingPrompt,
                attempt,
                message: "Built prompt".to_string(),
            });
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::ModelCall,
                attempt,
                message: "Calling model".to_string(),
            });
        }
        if cancel.is_cancelled() {
            return Err(CodeGenerationError::Cancelled);
        }

        let response = match provider.complete(&prompt, cancel).await {
            Ok(response) => response,
            Err(PromptCompletionError::Cancelled) => return Err(CodeGenerationError::Cancelled),
            Err(err) => return Err(err.into()),
        };
        if cancel.is_cancelled() {
            return Err(CodeGenerationError::Cancelled);
        }

        if let Some(progress) = progress {
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::ParsingPatch,
                attempt,
                message: "Parsing structured patch".to_string(),
            });
        }

        let patch = match parse_structured_patch(&response) {
            Ok(patch) => patch,
            Err(err) => {
                if config.allow_repair && attempt < config.max_repair_attempts {
                    feedback = Some(ErrorFeedback::PatchFormat(err.to_string()));
                    attempt += 1;
                    continue;
                }
                return Err(err.into());
            }
        };

        enforce_patch_safety(&patch, workspace, &config.safety)?;

        if let Some(progress) = progress {
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::ApplyingPatch,
                attempt,
                message: "Applying patch".to_string(),
            });
        }

        if cancel.is_cancelled() {
            return Err(CodeGenerationError::Cancelled);
        }

        let applied = match workspace.apply_patch_with_config(
            &patch,
            &PatchApplyConfig {
                allow_new_files: config.safety.allow_new_files,
            },
        ) {
            Ok(applied) => applied,
            Err(err) => {
                if config.allow_repair && attempt < config.max_repair_attempts {
                    feedback = Some(ErrorFeedback::PatchApply(err.to_string()));
                    attempt += 1;
                    continue;
                }
                return Err(err.into());
            }
        };

        if let Some(progress) = progress {
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::Formatting,
                attempt,
                message: "Formatting touched files".to_string(),
            });
        }

        let formatted_workspace = format_workspace(&formatter, &applied, config);

        if config.safety.no_new_imports {
            enforce_no_new_imports(workspace, &formatted_workspace, &applied)?;
        }

        if let Some(progress) = progress {
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::Validating,
                attempt,
                message: "Validating diagnostics".to_string(),
            });
        }

        match validate_patch(
            workspace,
            &formatted_workspace,
            &applied,
            &engine,
            &config.validation,
        ) {
            Ok(()) => {
                return Ok(CodeGenerationResult {
                    patch,
                    applied,
                    formatted_workspace,
                })
            }
            Err(report) => {
                if config.allow_repair && attempt < config.max_repair_attempts {
                    feedback = Some(ErrorFeedback::Validation(report));
                    attempt += 1;
                    continue;
                }
                return Err(CodeGenerationError::ValidationFailed { report });
            }
        }
    }
}

fn build_prompt(base: &str, config: &CodeGenerationConfig, feedback: Option<&ErrorFeedback>) -> String {
    let mut out = String::new();
    out.push_str(base);
    out.push_str("\n\nReturn ONLY a structured patch.\n");
    out.push_str("JSON schema:\n");
    out.push_str("{\"edits\":[{\"file\":\"path\",\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":0}},\"text\":\"...\"}],\"ops\":[{\"op\":\"create\",\"file\":\"path\",\"text\":\"...\"},{\"op\":\"delete\",\"file\":\"path\"},{\"op\":\"rename\",\"from\":\"old\",\"to\":\"new\"}]}\n");
    out.push_str("Alternatively you may return a unified diff starting with \"---\"/\"+++\" and \"@@\" hunks.\n");
    out.push_str("Paths must be workspace-relative and use forward slashes (/).\n");

    out.push_str(&format!(
        "\nSafety limits: max_files={}, max_total_inserted_chars={}, max_total_deleted_chars={}, max_hunks_per_file={}, max_edit_span_chars={}.\n",
        config.safety.max_files,
        config.safety.max_total_inserted_chars,
        config.safety.max_total_deleted_chars,
        config.safety.max_hunks_per_file,
        config.safety.max_edit_span_chars,
    ));
    if !config.safety.allowed_path_prefixes.is_empty() {
        out.push_str("Allowed path prefixes:\n");
        for prefix in &config.safety.allowed_path_prefixes {
            out.push_str(&format!("- {prefix}\n"));
        }
    }

    if !config.safety.allowed_file_extensions.is_empty() {
        out.push_str("Allowed file extensions:\n");
        for ext in &config.safety.allowed_file_extensions {
            out.push_str(&format!("- {ext}\n"));
        }
    }
    if !config.safety.denied_file_extensions.is_empty() {
        out.push_str("Denied file extensions:\n");
        for ext in &config.safety.denied_file_extensions {
            out.push_str(&format!("- {ext}\n"));
        }
    }
    if !config.safety.excluded_path_prefixes.is_empty() {
        out.push_str("Excluded path prefixes:\n");
        for prefix in &config.safety.excluded_path_prefixes {
            out.push_str(&format!("- {prefix}\n"));
        }
    }
    if !config.safety.excluded_path_globs.is_empty() {
        out.push_str("Excluded path globs:\n");
        for pattern in &config.safety.excluded_path_globs {
            out.push_str(&format!("- {pattern}\n"));
        }
    }
    if !config.safety.allow_new_files {
        out.push_str(
            "Do not create new files; only edit files that already exist in the workspace.\n",
        );
    }
    if config.safety.no_new_imports {
        out.push_str("Do not add new import statements.\n");
    }

    if let Some(feedback) = feedback {
        out.push_str("\nPrevious output could not be applied:\n");
        match feedback {
            ErrorFeedback::PatchFormat(message) => {
                out.push_str("Patch format error:\n");
                out.push_str(message);
                out.push('\n');
            }
            ErrorFeedback::PatchApply(message) => {
                out.push_str("Patch apply error:\n");
                out.push_str(message);
                out.push('\n');
            }
            ErrorFeedback::Validation(report) => {
                out.push_str("Validation errors:\n");
                out.push_str(&report.to_prompt_block());
            }
        }
    }

    out
}

fn format_workspace(
    formatter: &Formatter,
    applied: &AppliedPatch,
    config: &CodeGenerationConfig,
) -> VirtualWorkspace {
    if !config.validation.format {
        return applied.workspace.clone();
    }

    let mut out = applied.workspace.clone();
    for file in applied.touched_ranges.keys() {
        if let Some(text) = out.get(file).map(str::to_string) {
            out.insert(file.clone(), formatter.format_java(&text));
        }
    }
    out
}

fn validate_patch(
    before: &VirtualWorkspace,
    after: &VirtualWorkspace,
    applied: &AppliedPatch,
    engine: &DiagnosticsEngine,
    config: &ValidationConfig,
) -> Result<(), ErrorReport> {
    let mut new_diagnostics = Vec::new();
    let mut new_syntax_errors = 0usize;
    let mut new_type_errors = 0usize;

    for (file, touched) in &applied.touched_ranges {
        let before_path = resolve_before_path(file, &applied.renamed_files);
        let before_text = before.get(&before_path).unwrap_or("");
        let after_text = after.get(file).unwrap_or("");

        let before_diags = engine.diagnose(file, before_text);
        let after_diags = engine.diagnose(file, after_text);

        let introduced = diff_diagnostics(&before_diags, &after_diags);
        for diag in introduced {
            if diag.severity != DiagnosticSeverity::Error {
                continue;
            }

            match diag.kind {
                DiagnosticKind::Syntax => {
                    new_syntax_errors += 1;
                    let position =
                        LineIndex::new(after_text).position(after_text, diag.range.start());
                    new_diagnostics.push(DiagnosticWithContext {
                        file: file.clone(),
                        context: render_context(after_text, diag.range, config.context_lines),
                        position,
                        diagnostic: diag,
                    });
                }
                DiagnosticKind::Type => {
                    if touched.iter().any(|range| ranges_intersect(*range, diag.range)) {
                        new_type_errors += 1;
                        let position =
                            LineIndex::new(after_text).position(after_text, diag.range.start());
                        new_diagnostics.push(DiagnosticWithContext {
                            file: file.clone(),
                            context: render_context(after_text, diag.range, config.context_lines),
                            position,
                            diagnostic: diag,
                        });
                    }
                }
            }
        }
    }

    new_diagnostics.sort_by(|a, b| {
        (
            a.file.as_str(),
            u32::from(a.diagnostic.range.start()),
            u32::from(a.diagnostic.range.end()),
            a.diagnostic.kind as u8,
            a.diagnostic.message.as_str(),
        )
            .cmp(&(
                b.file.as_str(),
                u32::from(b.diagnostic.range.start()),
                u32::from(b.diagnostic.range.end()),
                b.diagnostic.kind as u8,
                b.diagnostic.message.as_str(),
            ))
    });

    if new_syntax_errors > config.max_new_syntax_errors || new_type_errors > config.max_new_type_errors
    {
        return Err(ErrorReport {
            summary: format!(
                "Introduced {new_syntax_errors} syntax errors and {new_type_errors} type errors.",
            ),
            new_diagnostics,
        });
    }

    Ok(())
}

fn resolve_before_path(path: &str, renames: &BTreeMap<String, String>) -> String {
    let mut current = path;
    let mut visited = BTreeSet::new();
    while let Some(prev) = renames.get(current) {
        if !visited.insert(current.to_string()) {
            break;
        }
        current = prev;
    }
    current.to_string()
}

#[derive(Debug, Hash, Eq, PartialEq)]
struct DiagnosticFingerprint {
    kind: DiagnosticKind,
    severity: DiagnosticSeverity,
    message: String,
}

fn diff_diagnostics(before: &[Diagnostic], after: &[Diagnostic]) -> Vec<Diagnostic> {
    let mut counts: HashMap<DiagnosticFingerprint, usize> = HashMap::new();
    for diag in before {
        let fp = DiagnosticFingerprint {
            kind: diag.kind,
            severity: diag.severity,
            message: diag.message.clone(),
        };
        *counts.entry(fp).or_default() += 1;
    }

    let mut introduced = Vec::new();
    for diag in after {
        let fp = DiagnosticFingerprint {
            kind: diag.kind,
            severity: diag.severity,
            message: diag.message.clone(),
        };
        match counts.get_mut(&fp) {
            Some(count) if *count > 0 => {
                *count -= 1;
            }
            _ => introduced.push(diag.clone()),
        }
    }

    introduced
}

fn ranges_intersect(a: TextRange, b: TextRange) -> bool {
    a.start() < b.end() && b.start() < a.end()
}

fn render_context(source: &str, range: TextRange, context_lines: usize) -> String {
    let index = LineIndex::new(source);
    let pos = index.position(source, range.start());
    let lines: Vec<&str> = source.lines().collect();
    let start_line = pos.line as usize;
    if lines.is_empty() || start_line >= lines.len() {
        return String::new();
    }

    let from = start_line.saturating_sub(context_lines);
    let to = (start_line + context_lines + 1).min(lines.len());

    let mut out = String::new();
    for idx in from..to {
        out.push_str(&format!("{:>4} | {}\n", idx + 1, lines[idx]));
        if idx == start_line {
            let mut caret = String::new();
            caret.push_str("     | ");
            caret.push_str(&" ".repeat(pos.character as usize));
            caret.push('^');
            out.push_str(&caret);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    };
    use std::time::Duration;

    struct BlockingProvider {
        started_tx: Mutex<Option<mpsc::Sender<()>>>,
        resume_rx: Mutex<Option<mpsc::Receiver<()>>>,
        calls: AtomicUsize,
    }

    impl BlockingProvider {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl AiProvider for BlockingProvider {
        fn complete(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, AiProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);

            if let Some(tx) = self.started_tx.lock().expect("poisoned mutex").take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.resume_rx.lock().expect("poisoned mutex").take() {
                let _ = rx.recv();
            }

            Ok(r#"{"edits":[]}"#.to_string())
        }
    }

    #[test]
    fn cancelled_token_stops_code_generation_quickly() {
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (resume_tx, resume_rx) = mpsc::channel::<()>();
        let provider = Arc::new(BlockingProvider {
            started_tx: Mutex::new(Some(started_tx)),
            resume_rx: Mutex::new(Some(resume_rx)),
            calls: AtomicUsize::new(0),
        });

        let workspace = VirtualWorkspace::new([(
            "Example.java".to_string(),
            "public class Example {}".to_string(),
        )]);

        let config = CodeGenerationConfig::default();
        let cancel = CancellationToken::new();
        let cancel_for_thread = cancel.clone();

        let (result_tx, result_rx) = mpsc::channel();
        let handle = std::thread::spawn({
            let provider = Arc::clone(&provider);
            let workspace = workspace.clone();
            move || {
                let result = run_code_generation(
                    provider.as_ref(),
                    &workspace,
                    "Generate a patch.",
                    &config,
                    &AiPrivacyConfig::default(),
                    &cancel_for_thread,
                );
                let _ = result_tx.send(result);
            }
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("provider should start");

        cancel.cancel();
        resume_tx.send(()).expect("resume provider");

        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("codegen should return quickly after cancellation");
        assert!(matches!(result, Err(CodeGenerationError::Cancelled)));
        assert_eq!(provider.calls(), 1);

        handle.join().expect("codegen thread panicked");
    }
}
