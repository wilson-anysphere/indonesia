use std::collections::{BTreeMap, BTreeSet, HashMap};

use async_trait::async_trait;
use nova_ai::patch::{parse_structured_patch, Patch, PatchParseError};
use nova_ai::safety::{
    enforce_no_new_imports, enforce_patch_safety, PatchSafetyConfig, SafetyError,
};
use nova_ai::workspace::{AppliedPatch, PatchApplyConfig, PatchApplyError, VirtualWorkspace};
use nova_ai::CancellationToken;
use nova_ai::{enforce_code_edit_policy, CodeEditPolicyError, ExcludedPathMatcher};
use nova_config::AiPrivacyConfig;
use nova_core::{LineIndex, TextRange, TextSize};
use nova_db::Database;
use nova_db::InMemoryFileStore;
use nova_format::FormatConfig;
use nova_types::{Diagnostic as NovaDiagnostic, Severity as NovaSeverity};
use thiserror::Error;

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

impl ValidationConfig {
    /// Default maximum number of *new* type-checking errors permitted by
    /// [`ValidationConfig::relaxed_for_tests`].
    ///
    /// This is intended for AI test generation in environments where the
    /// type-checker doesn't have access to test dependencies (e.g. JUnit), which
    /// can otherwise cause false-negative validation failures.
    pub const RELAXED_TEST_MAX_NEW_TYPE_ERRORS: usize = 25;

    /// Validation preset suitable for AI-generated test files.
    ///
    /// - Syntax must remain clean (`max_new_syntax_errors = 0`).
    /// - Type-checking errors are allowed up to [`Self::RELAXED_TEST_MAX_NEW_TYPE_ERRORS`]
    ///   to tolerate missing test-classpath dependencies (e.g. JUnit).
    #[must_use]
    pub fn relaxed_for_tests() -> Self {
        Self::relaxed_for_tests_with_max_new_type_errors(Self::RELAXED_TEST_MAX_NEW_TYPE_ERRORS)
    }

    /// Like [`ValidationConfig::relaxed_for_tests`], but with a custom type error limit.
    #[must_use]
    pub fn relaxed_for_tests_with_max_new_type_errors(max_new_type_errors: usize) -> Self {
        Self {
            max_new_type_errors,
            ..Self::default()
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
    pub diagnostic: NovaDiagnostic,
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
                "{}:{}:{}: {} [{}]: {}\n",
                diag.file,
                diag.position.line + 1,
                diag.position.character + 1,
                match diag.diagnostic.severity {
                    NovaSeverity::Error => "error",
                    NovaSeverity::Warning => "warning",
                    NovaSeverity::Info => "info",
                },
                diag.diagnostic.code,
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
impl<T> PromptCompletionProvider for T
where
    T: nova_ai::LlmClient + Send + Sync + ?Sized,
{
    async fn complete(
        &self,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<String, PromptCompletionError> {
        let request = nova_ai::ChatRequest {
            messages: vec![nova_ai::ChatMessage::user(prompt.to_string())],
            max_tokens: None,
            temperature: None,
        };
        self.chat(request, cancel.clone())
            .await
            .map_err(|err| match err {
                nova_ai::AiError::Cancelled => PromptCompletionError::Cancelled,
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
    #[error("invalid ai privacy configuration: {0}")]
    InvalidPrivacyConfig(String),
    #[error(
        "AI code edits are blocked because the workspace contains files matching ai.privacy.excluded_paths: {paths:?}. \
Those files must never be sent to an LLM. Remove them from the workspace snapshot or update ai.privacy.excluded_paths."
    )]
    WorkspaceContainsExcludedPaths { paths: Vec<String> },
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
    enforce_no_privacy_excluded_paths_in_workspace(workspace, privacy)?;

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

        let formatted_workspace = format_workspace(&applied, config);

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

fn enforce_no_privacy_excluded_paths_in_workspace(
    workspace: &VirtualWorkspace,
    privacy: &AiPrivacyConfig,
) -> Result<(), CodeGenerationError> {
    if privacy.excluded_paths.is_empty() {
        return Ok(());
    }

    let matcher = ExcludedPathMatcher::from_config(privacy)
        .map_err(|err| CodeGenerationError::InvalidPrivacyConfig(err.to_string()))?;

    let mut excluded = Vec::new();
    for (path, _contents) in workspace.files() {
        if matcher.is_match(std::path::Path::new(path)) {
            excluded.push(path.clone());
        }
    }

    if excluded.is_empty() {
        return Ok(());
    }

    excluded.sort();
    Err(CodeGenerationError::WorkspaceContainsExcludedPaths { paths: excluded })
}

fn build_prompt(
    base: &str,
    config: &CodeGenerationConfig,
    feedback: Option<&ErrorFeedback>,
) -> String {
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
    if !config.safety.allow_delete_files {
        out.push_str("Do not delete files.\n");
    }
    if !config.safety.allow_rename_files {
        out.push_str("Do not rename or move files.\n");
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

fn format_workspace(applied: &AppliedPatch, config: &CodeGenerationConfig) -> VirtualWorkspace {
    if !config.validation.format {
        return applied.workspace.clone();
    }

    let mut out = applied.workspace.clone();
    for file in applied.touched_ranges.keys() {
        let path = std::path::Path::new(file);
        if path.extension().and_then(|ext| ext.to_str()) != Some("java") {
            continue;
        }
        let Some(text) = out.get(file).map(str::to_string) else {
            continue;
        };

        let tree = nova_syntax::parse(&text);
        let formatted = nova_format::format_java(&tree, &text, &FormatConfig::default());
        out.insert(file.clone(), formatted);
    }
    out
}

fn validate_patch(
    before: &VirtualWorkspace,
    after: &VirtualWorkspace,
    applied: &AppliedPatch,
    config: &ValidationConfig,
) -> Result<(), ErrorReport> {
    let before_db = diagnostics_db_from_workspace(before);
    let after_db = diagnostics_db_from_workspace(after);

    let mut new_diagnostics = Vec::new();
    let mut new_syntax_errors = 0usize;
    let mut new_type_errors = 0usize;

    for (file, touched) in &applied.touched_ranges {
        let before_path = resolve_before_path(file, &applied.renamed_files);
        let after_text = after.get(file).ok_or_else(|| ErrorReport {
            summary: format!(
                "internal error: applied patch references `{file}`, but it is missing from the updated workspace"
            ),
            new_diagnostics: Vec::new(),
        })?;

        let before_diags = diagnostics_for_path(&before_db, &before_path);
        let after_diags = diagnostics_for_path(&after_db, file);

        let introduced = diff_diagnostics(&before_diags, &after_diags);
        for diag in introduced {
            if diag.severity != NovaSeverity::Error {
                continue;
            }

            match diagnostic_bucket(&diag) {
                ValidationBucket::Syntax => {
                    new_syntax_errors += 1;
                    let (position, range) = diagnostic_position_and_range(after_text, &diag);
                    new_diagnostics.push(DiagnosticWithContext {
                        file: file.clone(),
                        context: render_context(after_text, range, config.context_lines),
                        position,
                        diagnostic: diag,
                    });
                }
                ValidationBucket::Type => {
                    let (position, range) = diagnostic_position_and_range(after_text, &diag);
                    let intersects = match diag.span {
                        Some(span) => touched.iter().any(|t| {
                            ranges_intersect(
                                *t,
                                TextRange::new(
                                    TextSize::from(span.start.min(u32::MAX as usize) as u32),
                                    TextSize::from(span.end.min(u32::MAX as usize) as u32),
                                ),
                            )
                        }),
                        None => true,
                    };

                    if intersects {
                        new_type_errors += 1;
                        new_diagnostics.push(DiagnosticWithContext {
                            file: file.clone(),
                            context: render_context(after_text, range, config.context_lines),
                            position,
                            diagnostic: diag,
                        });
                    }
                }
            }
        }
    }

    new_diagnostics.sort_by(|a, b| {
        let (a_start, a_end) = diagnostic_span_bounds(&a.diagnostic);
        let (b_start, b_end) = diagnostic_span_bounds(&b.diagnostic);
        (
            a.file.as_str(),
            a_start,
            a_end,
            a.diagnostic.code.as_ref(),
            a.diagnostic.message.as_str(),
        )
            .cmp(&(
                b.file.as_str(),
                b_start,
                b_end,
                b.diagnostic.code.as_ref(),
                b.diagnostic.message.as_str(),
            ))
    });

    if new_syntax_errors > config.max_new_syntax_errors
        || new_type_errors > config.max_new_type_errors
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValidationBucket {
    Syntax,
    Type,
}

fn diagnostic_bucket(diag: &NovaDiagnostic) -> ValidationBucket {
    let code = diag.code.as_ref();
    if code == "SYNTAX" || code.starts_with("JAVA_FEATURE_") {
        ValidationBucket::Syntax
    } else {
        ValidationBucket::Type
    }
}

fn diagnostic_span_bounds(diag: &NovaDiagnostic) -> (usize, usize) {
    diag.span
        .map(|span| (span.start, span.end))
        .unwrap_or((0, 0))
}

fn diagnostic_position_and_range(
    text: &str,
    diag: &NovaDiagnostic,
) -> (nova_core::Position, TextRange) {
    let span = diag.span.unwrap_or(nova_types::Span { start: 0, end: 0 });
    let start = span.start.min(text.len()).min(u32::MAX as usize) as u32;
    let end = span.end.min(text.len()).min(u32::MAX as usize) as u32;
    let range = TextRange::new(TextSize::from(start), TextSize::from(end.max(start)));

    let index = LineIndex::new(text);
    let position = index.position(text, TextSize::from(start));
    (position, range)
}

fn diagnostics_db_from_workspace(workspace: &VirtualWorkspace) -> InMemoryFileStore {
    let mut db = InMemoryFileStore::new();
    for (path, text) in workspace.files() {
        let file_id = db.file_id_for_path(path);
        db.set_file_text(file_id, text.to_string());
    }
    db
}

fn diagnostics_for_path(db: &InMemoryFileStore, path: &str) -> Vec<NovaDiagnostic> {
    let Some(file_id) = db.file_id(std::path::Path::new(path)) else {
        return Vec::new();
    };
    nova_ide::code_intelligence::file_diagnostics(db, file_id)
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
    severity: u8,
    code: String,
    message: String,
}

fn diff_diagnostics(before: &[NovaDiagnostic], after: &[NovaDiagnostic]) -> Vec<NovaDiagnostic> {
    let mut counts: HashMap<DiagnosticFingerprint, usize> = HashMap::new();
    for diag in before {
        let fp = DiagnosticFingerprint {
            severity: severity_fingerprint(diag.severity),
            code: diag.code.to_string(),
            message: diag.message.clone(),
        };
        *counts.entry(fp).or_default() += 1;
    }

    let mut introduced = Vec::new();
    for diag in after {
        let fp = DiagnosticFingerprint {
            severity: severity_fingerprint(diag.severity),
            code: diag.code.to_string(),
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

fn severity_fingerprint(severity: NovaSeverity) -> u8 {
    match severity {
        NovaSeverity::Error => 0,
        NovaSeverity::Warning => 1,
        NovaSeverity::Info => 2,
    }
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
    use futures::executor::block_on;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    };
    use std::time::Duration;

    struct StaticProvider {
        response: String,
    }

    #[async_trait]
    impl PromptCompletionProvider for StaticProvider {
        async fn complete(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, PromptCompletionError> {
            Ok(self.response.clone())
        }
    }

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

    #[async_trait]
    impl PromptCompletionProvider for BlockingProvider {
        async fn complete(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, PromptCompletionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);

            if let Some(tx) = self
                .started_tx
                .lock()
                .expect("started_tx mutex poisoned")
                .take()
            {
                let _ = tx.send(());
            }
            if let Some(rx) = self
                .resume_rx
                .lock()
                .expect("resume_rx mutex poisoned")
                .take()
            {
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
                let result = block_on(generate_patch(
                    provider.as_ref(),
                    &workspace,
                    "Generate a patch.",
                    &config,
                    &AiPrivacyConfig::default(),
                    &cancel_for_thread,
                    None,
                ));
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

    #[test]
    fn generates_formats_and_validates_patch_against_workspace() {
        let provider = StaticProvider {
            response: r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 48 }, "end": { "line": 0, "character": 50 } },
      "text": "42"
    }
  ]
}"#
            .to_string(),
        };

        let before = "public class Example{public int answer(){return 41;}}";
        let workspace = VirtualWorkspace::new([("Example.java".to_string(), before.to_string())]);

        let config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };

        let cancel = CancellationToken::new();
        let result = block_on(generate_patch(
            &provider,
            &workspace,
            "Change the answer to 42.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect("codegen success");

        let applied = result
            .applied
            .workspace
            .get("Example.java")
            .expect("patched file");
        assert!(applied.contains("return 42;"), "{applied}");

        let expected = nova_format::format_java(
            &nova_syntax::parse(applied),
            applied,
            &FormatConfig::default(),
        );
        let formatted = result
            .formatted_workspace
            .get("Example.java")
            .expect("formatted file");
        assert_eq!(formatted, expected);
    }

    #[test]
    fn validation_rejects_introduced_unresolved_reference() {
        let provider = StaticProvider {
            response: r#"diff --git a/Example.java b/Example.java
--- a/Example.java
+++ b/Example.java
@@ -1,3 +1,3 @@
 public class Example{
-    public int answer(){return 41;}
+    public int answer(){missing();return 41;}
 }
"#
            .to_string(),
        };

        let before = "public class Example{\n    public int answer(){return 41;}\n}\n";
        let workspace = VirtualWorkspace::new([("Example.java".to_string(), before.to_string())]);

        let config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };

        let cancel = CancellationToken::new();
        let err = block_on(generate_patch(
            &provider,
            &workspace,
            "Insert a call to missing().",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect_err("should fail validation");

        let CodeGenerationError::ValidationFailed { report } = err else {
            panic!("expected ValidationFailed, got {err:?}");
        };

        assert!(
            report
                .new_diagnostics
                .iter()
                .any(|diag| diag.diagnostic.code.as_ref() == "UNRESOLVED_REFERENCE"),
            "expected unresolved reference diagnostic, got: {report:?}"
        );
    }

    #[test]
    fn strict_validation_rejects_new_junit_test_file_without_classpath() {
        // In minimal/no-classpath environments we can't resolve JUnit types, so a
        // syntactically-correct test file can still produce type diagnostics.
        let provider = StaticProvider {
            response: r#"{
  "ops": [
    {
      "op": "create",
      "file": "ExampleTest.java",
      "text": "import org.junit.jupiter.api.Assertions;\nimport org.junit.jupiter.api.Test;\n\npublic class ExampleTest {\n    @Test\n    void itWorks() {\n        Assertions.assertEquals(42, 42);\n    }\n}\n"
    }
  ]
}"#
            .to_string(),
        };

        let workspace = VirtualWorkspace::default();

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.safety.allow_new_files = true;

        let cancel = CancellationToken::new();
        let err = block_on(generate_patch(
            &provider,
            &workspace,
            "Generate a new JUnit test file.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect_err("strict validation should fail due to unresolved JUnit types");

        let CodeGenerationError::ValidationFailed { report } = err else {
            panic!("expected ValidationFailed, got {err:?}");
        };

        assert!(
            report.summary.contains("Introduced 0 syntax errors"),
            "expected syntactically valid file, got: {report:?}"
        );
        assert!(
            !report.new_diagnostics.is_empty(),
            "expected at least one new diagnostic, got: {report:?}"
        );
    }

    #[test]
    fn relaxed_validation_allows_new_junit_test_file_without_classpath() {
        let provider = StaticProvider {
            response: r#"{
  "ops": [
    {
      "op": "create",
      "file": "ExampleTest.java",
      "text": "import org.junit.jupiter.api.Assertions;\nimport org.junit.jupiter.api.Test;\n\npublic class ExampleTest {\n    @Test\n    void itWorks() {\n        Assertions.assertEquals(42, 42);\n    }\n}\n"
    }
  ]
}"#
            .to_string(),
        };

        let workspace = VirtualWorkspace::default();

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.safety.allow_new_files = true;
        config.validation = ValidationConfig::relaxed_for_tests();

        let cancel = CancellationToken::new();
        let result = block_on(generate_patch(
            &provider,
            &workspace,
            "Generate a new JUnit test file.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect("relaxed validation should allow unresolved JUnit types");

        let generated = result
            .formatted_workspace
            .get("ExampleTest.java")
            .expect("new file should be present");
        assert!(
            generated.contains("org.junit.jupiter.api.Test"),
            "expected JUnit @Test import, got: {generated}"
        );
        assert!(
            generated.contains("org.junit.jupiter.api.Assertions"),
            "expected JUnit Assertions import, got: {generated}"
        );
        assert!(
            generated.contains("@Test"),
            "expected @Test, got: {generated}"
        );
    }

    struct CountingProvider {
        calls: AtomicUsize,
    }

    impl CountingProvider {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl PromptCompletionProvider for CountingProvider {
        async fn complete(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, PromptCompletionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(r#"{"edits":[]}"#.to_string())
        }
    }

    #[test]
    fn fails_fast_when_workspace_contains_privacy_excluded_paths() {
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
        };
        let workspace = VirtualWorkspace::new([
            (
                "src/secrets/Secret.java".to_string(),
                "public class Secret {}".to_string(),
            ),
            (
                "Example.java".to_string(),
                "public class Example {}".to_string(),
            ),
        ]);

        let config = CodeGenerationConfig::default();
        let cancel = CancellationToken::new();
        let privacy = AiPrivacyConfig {
            excluded_paths: vec!["src/secrets/**".to_string()],
            ..AiPrivacyConfig::default()
        };

        let err = block_on(generate_patch(
            &provider,
            &workspace,
            "Generate a patch.",
            &config,
            &privacy,
            &cancel,
            None,
        ))
        .expect_err("expected excluded path failure");

        let CodeGenerationError::WorkspaceContainsExcludedPaths { paths } = err else {
            panic!("expected WorkspaceContainsExcludedPaths, got {err:?}");
        };
        assert!(
            paths.iter().any(|p| p == "src/secrets/Secret.java"),
            "{paths:?}"
        );
        assert_eq!(provider.calls(), 0);
    }
}
