use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nova_ai::patch::{parse_structured_patch, Patch, PatchParseError};
use nova_ai::safety::{
    enforce_no_new_imports, enforce_patch_safety, PatchSafetyConfig, SafetyError,
};
use nova_ai::workspace::{AppliedPatch, PatchApplyConfig, PatchApplyError, VirtualWorkspace};
use nova_ai::CancellationToken;
use nova_ai::{enforce_code_edit_policy, CodeEditPolicyError, ExcludedPathMatcher};
use nova_config::AiPrivacyConfig;
use nova_core::{LineIndex, Position as CorePosition, TextRange, TextSize};
use nova_db::Database;
use nova_db::{FileId, ProjectId, SalsaDatabase, SalsaDbView};
use nova_format::FormatConfig;
use nova_jdk::JdkIndex;
use nova_metrics::MetricsRegistry;
use nova_types::{Diagnostic as NovaDiagnostic, Severity as NovaSeverity};
use thiserror::Error;

const AI_CODEGEN_BUILD_PROMPT_METRIC: &str = "ai/codegen/build_prompt";
const AI_CODEGEN_MODEL_CALL_METRIC: &str = "ai/codegen/model_call";
const AI_CODEGEN_PARSE_PATCH_METRIC: &str = "ai/codegen/parse_patch";
const AI_CODEGEN_APPLY_PATCH_METRIC: &str = "ai/codegen/apply_patch";
const AI_CODEGEN_FORMAT_METRIC: &str = "ai/codegen/format";
const AI_CODEGEN_VALIDATE_METRIC: &str = "ai/codegen/validate";
const AI_CODEGEN_REPAIR_ATTEMPT_METRIC: &str = "ai/codegen/repair_attempt";

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, OnceLock};

    pub(crate) fn metrics_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}

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
pub struct EditRangeSafetyConfig {
    /// Workspace-relative file path (forward slashes).
    pub file: String,
    /// LSP-compatible UTF-16 range (half-open) in the *original* document to which edits must be
    /// confined.
    pub allowed_range: nova_ai::patch::Range,
}

#[derive(Debug, Clone)]
pub struct CodeGenerationConfig {
    pub safety: PatchSafetyConfig,
    pub validation: ValidationConfig,
    pub max_repair_attempts: usize,
    pub allow_repair: bool,
    /// Optional enforcement that patches only edit within a specific range (e.g. method body).
    ///
    /// This is enforced against the model-authored patch *before formatting* to avoid rejecting
    /// safe patches due to formatter churn.
    pub edit_range_safety: Option<EditRangeSafetyConfig>,
}

impl Default for CodeGenerationConfig {
    fn default() -> Self {
        Self {
            safety: PatchSafetyConfig::default(),
            validation: ValidationConfig::default(),
            max_repair_attempts: 2,
            allow_repair: true,
            edit_range_safety: None,
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
    SafetyViolation(String),
    EditRangeSafety(String),
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
    #[error(
        "invalid insert range for '{file}': {range}. \
This usually means the editor selection is out of date. Re-run the code action."
    )]
    InvalidInsertRange { file: String, range: String },
    #[error("patch edited outside the allowed range: {0}")]
    EditRangeSafety(String),
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
    if let Some(edit_range_safety) = &config.edit_range_safety {
        validate_edit_range_safety_config(workspace, edit_range_safety)?;
    }

    let metrics = MetricsRegistry::global();
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

        if attempt > 0 {
            // Record the fact that we had to loop (i.e. the initial model output was not usable).
            // This is recorded as a counter-only metric (request_count).
            metrics.record_request(AI_CODEGEN_REPAIR_ATTEMPT_METRIC, Duration::from_micros(1));
        }
        if cancel.is_cancelled() {
            return Err(CodeGenerationError::Cancelled);
        }

        let build_prompt_start = Instant::now();
        let prompt = build_prompt(base_prompt, config, feedback.as_ref());
        metrics.record_request(AI_CODEGEN_BUILD_PROMPT_METRIC, build_prompt_start.elapsed());
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

        let model_call_start = Instant::now();
        let response_result = provider.complete(&prompt, cancel).await;
        metrics.record_request(AI_CODEGEN_MODEL_CALL_METRIC, model_call_start.elapsed());
        let response = match response_result {
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

        let parse_patch_start = Instant::now();
        let patch = match parse_structured_patch(&response) {
            Ok(patch) => patch,
            Err(err) => {
                metrics.record_request(AI_CODEGEN_PARSE_PATCH_METRIC, parse_patch_start.elapsed());
                if config.allow_repair && attempt < config.max_repair_attempts {
                    feedback = Some(ErrorFeedback::PatchFormat(err.to_string()));
                    attempt += 1;
                    continue;
                }
                return Err(err.into());
            }
        };

        if let Err(err) = enforce_patch_safety(&patch, workspace, &config.safety) {
            metrics.record_request(AI_CODEGEN_PARSE_PATCH_METRIC, parse_patch_start.elapsed());
            if config.allow_repair && attempt < config.max_repair_attempts {
                feedback = Some(ErrorFeedback::SafetyViolation(format!(
                    "{err}\nPlease adjust the patch to comply with the safety limits listed above."
                )));
                attempt += 1;
                continue;
            }
            return Err(err.into());
        }

        if let Some(safety) = &config.edit_range_safety {
            if let Err(message) = enforce_edit_range_safety_patch_intent(&patch, safety) {
                metrics.record_request(AI_CODEGEN_PARSE_PATCH_METRIC, parse_patch_start.elapsed());
                if config.allow_repair && attempt < config.max_repair_attempts {
                    feedback = Some(ErrorFeedback::EditRangeSafety(message));
                    attempt += 1;
                    continue;
                }
                return Err(CodeGenerationError::EditRangeSafety(message));
            }
        }

        metrics.record_request(AI_CODEGEN_PARSE_PATCH_METRIC, parse_patch_start.elapsed());

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

        let apply_patch_start = Instant::now();
        let applied = match workspace.apply_patch_with_config(
            &patch,
            &PatchApplyConfig {
                allow_new_files: config.safety.allow_new_files,
            },
        ) {
            Ok(applied) => applied,
            Err(err) => {
                metrics.record_request(AI_CODEGEN_APPLY_PATCH_METRIC, apply_patch_start.elapsed());
                if config.allow_repair && attempt < config.max_repair_attempts {
                    feedback = Some(ErrorFeedback::PatchApply(err.to_string()));
                    attempt += 1;
                    continue;
                }
                return Err(err.into());
            }
        };

        if let Some(safety) = &config.edit_range_safety {
            if let Err(message) = enforce_edit_range_safety_pre_format(workspace, &applied, safety)
            {
                metrics.record_request(AI_CODEGEN_APPLY_PATCH_METRIC, apply_patch_start.elapsed());
                if config.allow_repair && attempt < config.max_repair_attempts {
                    feedback = Some(ErrorFeedback::EditRangeSafety(message));
                    attempt += 1;
                    continue;
                }
                return Err(CodeGenerationError::EditRangeSafety(message));
            }
        }

        metrics.record_request(AI_CODEGEN_APPLY_PATCH_METRIC, apply_patch_start.elapsed());

        if let Some(progress) = progress {
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::Formatting,
                attempt,
                message: "Formatting touched files".to_string(),
            });
        }

        let format_start = Instant::now();
        let formatted_workspace = format_workspace(&applied, config);

        if config.safety.no_new_imports {
            if let Err(err) = enforce_no_new_imports(workspace, &formatted_workspace, &applied) {
                metrics.record_request(AI_CODEGEN_FORMAT_METRIC, format_start.elapsed());
                if config.allow_repair && attempt < config.max_repair_attempts {
                    feedback = Some(ErrorFeedback::SafetyViolation(format!(
                        "{err}\nRemove the new imports (or use fully-qualified names / existing imports) and try again."
                    )));
                    attempt += 1;
                    continue;
                }
                return Err(err.into());
            }
        }
        metrics.record_request(AI_CODEGEN_FORMAT_METRIC, format_start.elapsed());

        if let Some(progress) = progress {
            progress.report(CodegenProgressEvent {
                stage: CodegenProgressStage::Validating,
                attempt,
                message: "Validating diagnostics".to_string(),
            });
        }

        let validate_start = Instant::now();
        let validation = validate_patch(
            workspace,
            &formatted_workspace,
            &applied,
            &config.validation,
        );
        metrics.record_request(AI_CODEGEN_VALIDATE_METRIC, validate_start.elapsed());

        match validation {
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
    if let Some(safety) = &config.edit_range_safety {
        out.push_str(&format!(
            "Range safety: only edit '{}' within {}.\n",
            safety.file,
            fmt_patch_range(safety.allowed_range)
        ));
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
            ErrorFeedback::SafetyViolation(message) => {
                out.push_str("Patch safety violation:\n");
                out.push_str(message);
                out.push('\n');
            }
            ErrorFeedback::EditRangeSafety(message) => {
                out.push_str("Patch range-safety error:\n");
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

fn enforce_edit_range_safety_patch_intent(
    patch: &Patch,
    safety: &EditRangeSafetyConfig,
) -> Result<(), String> {
    match patch {
        Patch::Json(patch) => {
            if !patch.ops.is_empty() {
                return Err(format!(
                    "patch contains file operations, but only text edits within the allowed range are permitted"
                ));
            }
            for edit in &patch.edits {
                if edit.file != safety.file {
                    return Err(format!(
                        "patch attempted to edit '{}' but only '{}' is allowed",
                        edit.file, safety.file
                    ));
                }
                if !patch_range_contains_range(safety.allowed_range, edit.range) {
                    return Err(format!(
                        "patch edit range {} is outside the allowed range {} for '{}'",
                        fmt_patch_range(edit.range),
                        fmt_patch_range(safety.allowed_range),
                        safety.file
                    ));
                }
            }
        }
        Patch::UnifiedDiff(diff) => {
            for file in &diff.files {
                let file_id = if file.new_path != "/dev/null" {
                    &file.new_path
                } else {
                    &file.old_path
                };
                if file_id != &safety.file {
                    return Err(format!(
                        "patch attempted to edit '{}' but only '{}' is allowed",
                        file_id, safety.file
                    ));
                }
                if file.old_path != file.new_path {
                    return Err(format!(
                        "patch attempted to rename '{}' to '{}', but only in-place edits within the allowed range are permitted",
                        file.old_path, file.new_path
                    ));
                }
            }
        }
    }

    Ok(())
}

fn enforce_edit_range_safety_pre_format(
    before: &VirtualWorkspace,
    applied: &AppliedPatch,
    safety: &EditRangeSafetyConfig,
) -> Result<(), String> {
    // We intentionally validate against the pre-format patched workspace. Formatting can
    // legitimately change whitespace outside the edited span, and we don't want to reject safe
    // model patches due to formatter churn.
    let before_text = before
        .get(&safety.file)
        .ok_or_else(|| format!("missing target file '{}' in workspace", safety.file))?;
    let after_text = applied
        .workspace
        .get(&safety.file)
        .ok_or_else(|| format!("missing target file '{}' after applying patch", safety.file))?;

    enforce_text_unchanged_outside_range(before_text, after_text, safety.allowed_range)
        .map_err(|message| format!("{message} (file '{}')", safety.file))
}

fn validate_edit_range_safety_config(
    workspace: &VirtualWorkspace,
    safety: &EditRangeSafetyConfig,
) -> Result<(), CodeGenerationError> {
    let before_text = workspace.get(&safety.file).ok_or_else(|| {
        CodeGenerationError::Apply(PatchApplyError::MissingFile {
            file: safety.file.clone(),
        })
    })?;

    let index = LineIndex::new(before_text);
    let Some(start) = index.offset_of_position(
        before_text,
        CorePosition::new(safety.allowed_range.start.line, safety.allowed_range.start.character),
    ) else {
        return Err(CodeGenerationError::InvalidInsertRange {
            file: safety.file.clone(),
            range: fmt_patch_range(safety.allowed_range),
        });
    };
    let Some(end) = index.offset_of_position(
        before_text,
        CorePosition::new(safety.allowed_range.end.line, safety.allowed_range.end.character),
    ) else {
        return Err(CodeGenerationError::InvalidInsertRange {
            file: safety.file.clone(),
            range: fmt_patch_range(safety.allowed_range),
        });
    };

    if start > end {
        return Err(CodeGenerationError::InvalidInsertRange {
            file: safety.file.clone(),
            range: fmt_patch_range(safety.allowed_range),
        });
    }

    Ok(())
}

fn enforce_text_unchanged_outside_range(
    before: &str,
    after: &str,
    allowed_range: nova_ai::patch::Range,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }

    let index = LineIndex::new(before);
    let allowed_start = index
        .offset_of_position(
            before,
            CorePosition::new(allowed_range.start.line, allowed_range.start.character),
        )
        .ok_or_else(|| format!("invalid allowed range start {}", fmt_patch_range(allowed_range)))?;
    let allowed_end = index
        .offset_of_position(
            before,
            CorePosition::new(allowed_range.end.line, allowed_range.end.character),
        )
        .ok_or_else(|| format!("invalid allowed range end {}", fmt_patch_range(allowed_range)))?;

    let allowed_start = u32::from(allowed_start) as usize;
    let allowed_end = u32::from(allowed_end) as usize;
    if allowed_start > allowed_end || allowed_end > before.len() {
        return Err(format!(
            "invalid allowed range {} (computed offsets {allowed_start}..{allowed_end})",
            fmt_patch_range(allowed_range)
        ));
    }

    let Some((changed_start, changed_end)) = diff_span_before(before, after) else {
        return Ok(());
    };

    if changed_start < allowed_start || changed_end > allowed_end {
        let changed_start_pos = index.position(
            before,
            TextSize::from((changed_start.min(u32::MAX as usize)) as u32),
        );
        let changed_end_pos =
            index.position(before, TextSize::from((changed_end.min(u32::MAX as usize)) as u32));
        return Err(format!(
            "patch modified text outside the allowed range {} (changed range {}:{}-{}:{})",
            fmt_patch_range(allowed_range),
            changed_start_pos.line + 1,
            changed_start_pos.character + 1,
            changed_end_pos.line + 1,
            changed_end_pos.character + 1,
        ));
    }

    Ok(())
}

fn diff_span_before(before: &str, after: &str) -> Option<(usize, usize)> {
    if before == after {
        return None;
    }

    let before_bytes = before.as_bytes();
    let after_bytes = after.as_bytes();

    let mut prefix = 0usize;
    let min_len = before_bytes.len().min(after_bytes.len());
    while prefix < min_len && before_bytes[prefix] == after_bytes[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    let max_suffix = (before_bytes.len().saturating_sub(prefix))
        .min(after_bytes.len().saturating_sub(prefix));
    while suffix < max_suffix
        && before_bytes[before_bytes.len() - 1 - suffix]
            == after_bytes[after_bytes.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mut start = prefix;
    let mut end = before_bytes.len().saturating_sub(suffix);

    // Ensure boundaries are on UTF-8 character boundaries so error reporting via `LineIndex`
    // conversions is safe and deterministic.
    while start > 0 && !before.is_char_boundary(start) {
        start = start.saturating_sub(1);
    }
    while end < before.len() && !before.is_char_boundary(end) {
        end = end.saturating_add(1);
    }

    Some((start, end))
}

fn patch_range_contains_range(outer: nova_ai::patch::Range, inner: nova_ai::patch::Range) -> bool {
    patch_pos_le(outer.start, inner.start) && patch_pos_le(inner.end, outer.end)
}

fn patch_pos_le(a: nova_ai::patch::Position, b: nova_ai::patch::Position) -> bool {
    (a.line, a.character) <= (b.line, b.character)
}

fn fmt_patch_range(range: nova_ai::patch::Range) -> String {
    format!(
        "{}:{}-{}:{}",
        range.start.line + 1,
        range.start.character + 1,
        range.end.line + 1,
        range.end.character + 1
    )
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
        let after_text = after.get(file).unwrap_or_default();

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

fn diagnostics_db_from_workspace(workspace: &VirtualWorkspace) -> WorkspaceDiagnosticsDb {
    WorkspaceDiagnosticsDb::from_workspace(workspace)
}

#[derive(Clone)]
struct WorkspaceDiagnosticsDb {
    salsa: SalsaDatabase,
    view: SalsaDbView,
}

impl WorkspaceDiagnosticsDb {
    fn from_workspace(workspace: &VirtualWorkspace) -> Self {
        let project = ProjectId::from_raw(0);
        let salsa = SalsaDatabase::new();

        // Seed a minimal JDK + classpath configuration so semantic queries don't panic.
        // This intentionally uses an empty index to keep validation deterministic and fast.
        salsa.set_jdk_index(project, Arc::new(JdkIndex::new()));
        salsa.set_classpath_index(project, None);

        let mut project_files: Vec<FileId> = Vec::new();
        for (idx, (path, text)) in workspace.files().enumerate() {
            let file_id = FileId::from_raw(idx as u32);
            // Set `file_rel_path` before `set_file_text` so Salsa doesn't synthesize a default path.
            salsa.set_file_rel_path(file_id, Arc::new(path.to_string()));
            salsa.set_file_text(file_id, text.to_string());
            project_files.push(file_id);
        }
        salsa.set_project_files(project, Arc::new(project_files));

        let snapshot = salsa.snapshot();
        let view = SalsaDbView::new(snapshot);
        Self { salsa, view }
    }
}

impl Database for WorkspaceDiagnosticsDb {
    fn file_content(&self, file_id: FileId) -> &str {
        self.view.file_content(file_id)
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        self.view.file_path(file_id)
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        self.view.all_file_ids()
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.view.file_id(path)
    }

    fn salsa_db(&self) -> Option<SalsaDatabase> {
        Some(self.salsa.clone())
    }
}

fn diagnostics_for_path(db: &WorkspaceDiagnosticsDb, path: &str) -> Vec<NovaDiagnostic> {
    let Some(file_id) = db.file_id(Path::new(path)) else {
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
    for (idx, line) in lines.iter().enumerate().take(to).skip(from) {
        out.push_str(&format!("{:>4} | {}\n", idx + 1, line));
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

    struct MockPromptCompletionProvider {
        responses: Vec<String>,
        calls: AtomicUsize,
        prompts: Mutex<Vec<String>>,
    }

    impl MockPromptCompletionProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses,
                calls: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn prompts(&self) -> Vec<String> {
            self.prompts
                .lock()
                .expect("poisoned mutex")
                .clone()
        }
    }

    #[async_trait]
    impl PromptCompletionProvider for MockPromptCompletionProvider {
        async fn complete(
            &self,
            prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, PromptCompletionError> {
            self.prompts
                .lock()
                .expect("poisoned mutex")
                .push(prompt.to_string());
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .responses
                .get(idx)
                .cloned()
                .or_else(|| self.responses.last().cloned())
                .unwrap_or_default())
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
    fn records_metrics_for_codegen_stages_and_repairs() {
        let _guard = crate::test_support::metrics_lock()
            .lock()
            .expect("metrics lock poisoned");

        let metrics = MetricsRegistry::global();
        metrics.reset();

        let provider = MockPromptCompletionProvider::new(vec![
            // First attempt: invalid patch payload forces a repair loop.
            "not json".to_string(),
            // Second attempt: valid patch.
            r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 48 }, "end": { "line": 0, "character": 50 } },
      "text": "42"
    }
  ]
}"#
            .to_string(),
        ]);

        let before = "public class Example{public int answer(){return 41;}}";
        let workspace = VirtualWorkspace::new([("Example.java".to_string(), before.to_string())]);

        let config = CodeGenerationConfig {
            max_repair_attempts: 1,
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
        .expect("codegen should succeed after repair");

        let applied = result
            .applied
            .workspace
            .get("Example.java")
            .expect("patched file");
        assert!(applied.contains("return 42;"), "{applied}");

        let snap = metrics.snapshot();
        for (metric, min_count) in [
            (AI_CODEGEN_BUILD_PROMPT_METRIC, 2),
            (AI_CODEGEN_MODEL_CALL_METRIC, 2),
            (AI_CODEGEN_PARSE_PATCH_METRIC, 2),
            (AI_CODEGEN_APPLY_PATCH_METRIC, 1),
            (AI_CODEGEN_FORMAT_METRIC, 1),
            (AI_CODEGEN_VALIDATE_METRIC, 1),
            (AI_CODEGEN_REPAIR_ATTEMPT_METRIC, 1),
        ] {
            let entry = snap
                .methods
                .get(metric)
                .unwrap_or_else(|| panic!("missing metrics entry for {metric}"));
            assert!(
                entry.request_count >= min_count,
                "expected {metric} request_count >= {min_count}, got {}",
                entry.request_count
            );
            assert!(
                entry.latency_us.max_us > 0,
                "expected {metric} latency histogram to be non-empty"
            );
        }
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

    #[test]
    fn repair_loop_retries_when_patch_safety_violation_occurs() {
        let unsafe_patch = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 48 }, "end": { "line": 0, "character": 50 } },
      "text": "4242424242"
    }
  ]
}"#
        .to_string();
        let safe_patch = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 48 }, "end": { "line": 0, "character": 50 } },
      "text": "42"
    }
  ]
}"#
        .to_string();

        let provider = MockPromptCompletionProvider::new(vec![unsafe_patch, safe_patch]);
        let before = "public class Example{public int answer(){return 41;}}";
        let workspace = VirtualWorkspace::new([("Example.java".to_string(), before.to_string())]);

        let mut config = CodeGenerationConfig::default();
        config.max_repair_attempts = 1;
        config.allow_repair = true;
        config.safety.max_total_inserted_chars = 5;

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
        .expect("repair loop should recover from safety violation");

        assert_eq!(provider.calls(), 2);
        let prompts = provider.prompts();
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        assert!(
            prompts[1].contains("Patch safety violation:"),
            "expected safety violation feedback in retry prompt:\n{}",
            prompts[1]
        );
        assert!(
            prompts[1].contains("patch inserts too many characters"),
            "expected safety violation details in retry prompt:\n{}",
            prompts[1]
        );
        let applied = result
            .applied
            .workspace
            .get("Example.java")
            .expect("patched file");
        assert!(applied.contains("return 42;"), "{applied}");
    }

    #[test]
    fn patch_safety_violation_fails_without_repair() {
        let unsafe_patch = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 48 }, "end": { "line": 0, "character": 50 } },
      "text": "4242424242"
    }
  ]
}"#
        .to_string();
        let safe_patch = r#"{"edits":[]}"#.to_string();

        let provider = MockPromptCompletionProvider::new(vec![unsafe_patch, safe_patch]);
        let before = "public class Example{public int answer(){return 41;}}";
        let workspace = VirtualWorkspace::new([("Example.java".to_string(), before.to_string())]);

        let mut config = CodeGenerationConfig::default();
        config.max_repair_attempts = 1;
        config.allow_repair = false;
        config.safety.max_total_inserted_chars = 5;

        let cancel = CancellationToken::new();
        let err = block_on(generate_patch(
            &provider,
            &workspace,
            "Change the answer to 42.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect_err("safety violation should fail fast when repair is disabled");

        let CodeGenerationError::Safety(SafetyError::TooManyInsertedChars { .. }) = err else {
            panic!("expected Safety(TooManyInsertedChars), got {err:?}");
        };
        assert_eq!(provider.calls(), 1);
        assert_eq!(provider.prompts().len(), 1);
    }

    #[test]
    fn repair_loop_retries_when_new_imports_introduced() {
        let patch_with_import = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "import java.util.List;\n\n"
    }
  ]
}"#
        .to_string();
        let patch_without_import = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 48 }, "end": { "line": 0, "character": 50 } },
      "text": "42"
    }
  ]
}"#
        .to_string();

        let provider = MockPromptCompletionProvider::new(vec![patch_with_import, patch_without_import]);
        let before = "public class Example{public int answer(){return 41;}}";
        let workspace = VirtualWorkspace::new([("Example.java".to_string(), before.to_string())]);

        let mut config = CodeGenerationConfig::default();
        config.max_repair_attempts = 1;
        config.allow_repair = true;
        config.safety.no_new_imports = true;

        let cancel = CancellationToken::new();
        let result = block_on(generate_patch(
            &provider,
            &workspace,
            "Change the answer to 42 without adding imports.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect("repair loop should recover from new-imports violation");

        assert_eq!(provider.calls(), 2);
        let prompts = provider.prompts();
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        assert!(
            prompts[1].contains("Patch safety violation:"),
            "expected safety violation feedback in retry prompt:\n{}",
            prompts[1]
        );
        assert!(
            prompts[1].contains("import java.util.List;"),
            "expected import line in retry prompt feedback:\n{}",
            prompts[1]
        );
        let applied = result
            .applied
            .workspace
            .get("Example.java")
            .expect("patched file");
        assert!(applied.contains("return 42;"), "{applied}");
    }

    #[test]
    fn new_imports_violation_fails_without_repair() {
        let patch_with_import = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "import java.util.List;\n\n"
    }
  ]
}"#
        .to_string();
        let patch_without_import = r#"{"edits":[]}"#.to_string();

        let provider = MockPromptCompletionProvider::new(vec![patch_with_import, patch_without_import]);
        let before = "public class Example{public int answer(){return 41;}}";
        let workspace = VirtualWorkspace::new([("Example.java".to_string(), before.to_string())]);

        let mut config = CodeGenerationConfig::default();
        config.max_repair_attempts = 1;
        config.allow_repair = false;
        config.safety.no_new_imports = true;

        let cancel = CancellationToken::new();
        let err = block_on(generate_patch(
            &provider,
            &workspace,
            "Change the answer to 42 without adding imports.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect_err("new imports should fail fast when repair is disabled");

        let CodeGenerationError::Safety(SafetyError::NewImports { .. }) = err else {
            panic!("expected Safety(NewImports), got {err:?}");
        };
        assert_eq!(provider.calls(), 1);
        assert_eq!(provider.prompts().len(), 1);
    }

    #[test]
    fn build_prompt_includes_safety_violation_feedback() {
        let config = CodeGenerationConfig::default();
        let prompt = build_prompt(
            "Base prompt.",
            &config,
            Some(&ErrorFeedback::SafetyViolation(
                "patch inserts too many characters (123 > 10)".to_string(),
            )),
        );

        let idx_previous = prompt
            .find("Previous output could not be applied:")
            .expect("expected previous-output header in prompt");
        let idx_safety = prompt
            .find("Patch safety violation:")
            .expect("expected safety-violation header in prompt");

        assert!(
            idx_previous < idx_safety,
            "expected safety section after previous-output header:\n{prompt}"
        );
        assert!(
            prompt.contains("patch inserts too many characters"),
            "expected safety violation message in prompt:\n{prompt}"
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

    #[test]
    fn edit_range_safety_invalid_range_fails_fast_without_model_call() {
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
        };
        let workspace = VirtualWorkspace::new([(
            "Example.java".to_string(),
            "public class Example {}".to_string(),
        )]);

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.edit_range_safety = Some(EditRangeSafetyConfig {
            file: "Example.java".to_string(),
            allowed_range: nova_ai::patch::Range {
                start: nova_ai::patch::Position {
                    line: 10,
                    character: 0,
                },
                end: nova_ai::patch::Position {
                    line: 10,
                    character: 0,
                },
            },
        });

        let cancel = CancellationToken::new();
        let err = block_on(generate_patch(
            &provider,
            &workspace,
            "Generate a patch.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect_err("expected invalid insert range failure");

        let CodeGenerationError::InvalidInsertRange { file, .. } = err else {
            panic!("expected InvalidInsertRange, got {err:?}");
        };
        assert_eq!(file, "Example.java");
        assert_eq!(provider.calls(), 0);
    }

    fn patch_pos_for_offset(text: &str, offset: usize) -> nova_ai::patch::Position {
        let index = LineIndex::new(text);
        let pos = index.position(text, TextSize::from(offset as u32));
        nova_ai::patch::Position {
            line: pos.line,
            character: pos.character,
        }
    }

    fn patch_range_for_offsets(text: &str, start: usize, end: usize) -> nova_ai::patch::Range {
        nova_ai::patch::Range {
            start: patch_pos_for_offset(text, start),
            end: patch_pos_for_offset(text, end),
        }
    }

    #[test]
    fn edit_range_safety_accepts_insertion_at_allowed_range_boundary() {
        let before = "class Test {\n    int add(int a, int b) {\n    }\n}\n";
        let file = "Test.java";
        let workspace = VirtualWorkspace::new([(file.to_string(), before.to_string())]);

        let method_line = "    int add(int a, int b) {";
        let open_brace_offset = before
            .find(method_line)
            .expect("method line")
            .saturating_add(method_line.len().saturating_sub(1));
        let close_brace_offset = before
            .find("\n    }\n")
            .expect("method close")
            .saturating_add("\n    ".len());

        let allowed_range = patch_range_for_offsets(before, open_brace_offset + 1, close_brace_offset);
        let insert_pos = patch_pos_for_offset(before, close_brace_offset);

        // Insert a return statement at the *end* boundary of the allowed range (right before `}`).
        let provider = StaticProvider {
            response: format!(
                r#"{{
  "edits": [{{
    "file": "{file}",
    "range": {{ "start": {{ "line": {line}, "character": {ch} }}, "end": {{ "line": {line}, "character": {ch} }} }},
    "text": "return a + b;\n    "
  }}]
}}"#,
                line = insert_pos.line,
                ch = insert_pos.character
            ),
        };

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.edit_range_safety = Some(EditRangeSafetyConfig {
            file: file.to_string(),
            allowed_range,
        });

        let cancel = CancellationToken::new();
        let result = block_on(generate_patch(
            &provider,
            &workspace,
            "Generate method body.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect("patch should be accepted");

        let applied = result
            .applied
            .workspace
            .get(file)
            .expect("patched file");
        assert!(applied.contains("return a + b;"), "{applied}");
    }

    #[test]
    fn edit_range_safety_accepts_insertion_at_allowed_range_start_boundary() {
        let before = "class Test {\n    int add(int a, int b) {\n    }\n}\n";
        let file = "Test.java";
        let workspace = VirtualWorkspace::new([(file.to_string(), before.to_string())]);

        let method_line = "    int add(int a, int b) {";
        let open_brace_offset = before
            .find(method_line)
            .expect("method line")
            .saturating_add(method_line.len().saturating_sub(1));
        let close_brace_offset = before
            .find("\n    }\n")
            .expect("method close")
            .saturating_add("\n    ".len());

        let allowed_range =
            patch_range_for_offsets(before, open_brace_offset + 1, close_brace_offset);
        let insert_pos = patch_pos_for_offset(before, open_brace_offset + 1);

        // Insert at the *start* boundary of the allowed range (right after `{`).
        let provider = StaticProvider {
            response: format!(
                r#"{{
  "edits": [{{
    "file": "{file}",
    "range": {{ "start": {{ "line": {line}, "character": {ch} }}, "end": {{ "line": {line}, "character": {ch} }} }},
    "text": "\n        return a + b;"
  }}]
}}"#,
                line = insert_pos.line,
                ch = insert_pos.character
            ),
        };

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.edit_range_safety = Some(EditRangeSafetyConfig {
            file: file.to_string(),
            allowed_range,
        });

        let cancel = CancellationToken::new();
        let result = block_on(generate_patch(
            &provider,
            &workspace,
            "Generate method body.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect("patch should be accepted");

        let applied = result
            .applied
            .workspace
            .get(file)
            .expect("patched file");
        assert!(applied.contains("return a + b;"), "{applied}");
    }

    #[test]
    fn edit_range_safety_accepts_deletion_at_allowed_range_end_boundary() {
        let before = "class Test {\n    int add(int a, int b) {\n    }\n}\n";
        let file = "Test.java";
        let workspace = VirtualWorkspace::new([(file.to_string(), before.to_string())]);

        let method_line = "    int add(int a, int b) {";
        let open_brace_offset = before
            .find(method_line)
            .expect("method line")
            .saturating_add(method_line.len().saturating_sub(1));
        let close_brace_offset = before
            .find("\n    }\n")
            .expect("method close")
            .saturating_add("\n    ".len());

        let allowed_range =
            patch_range_for_offsets(before, open_brace_offset + 1, close_brace_offset);

        // Delete the indentation spaces immediately before the closing `}` at the end boundary of
        // the allowed range.
        //
        // This is safe, but would be rejected by implementations that validate
        // `AppliedPatch.touched_ranges` in output coordinates, because deletions have an empty
        // inserted span and `touched_ranges` is expanded by ±1 byte (which can extend into the
        // closing `}`, outside the allowed range).
        let delete_start_offset = close_brace_offset.saturating_sub(4);
        assert_eq!(
            &before[delete_start_offset..close_brace_offset],
            "    ",
            "expected four-space indent before method close"
        );

        let delete_start = patch_pos_for_offset(before, delete_start_offset);
        let delete_end = patch_pos_for_offset(before, close_brace_offset);

        let provider = StaticProvider {
            response: format!(
                r#"{{
  "edits": [{{
    "file": "{file}",
    "range": {{ "start": {{ "line": {start_line}, "character": {start_ch} }}, "end": {{ "line": {end_line}, "character": {end_ch} }} }},
    "text": ""
  }}]
}}"#,
                start_line = delete_start.line,
                start_ch = delete_start.character,
                end_line = delete_end.line,
                end_ch = delete_end.character,
            ),
        };

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.edit_range_safety = Some(EditRangeSafetyConfig {
            file: file.to_string(),
            allowed_range,
        });

        let cancel = CancellationToken::new();
        let result = block_on(generate_patch(
            &provider,
            &workspace,
            "Edit method body.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect("patch should be accepted");

        let applied = result
            .applied
            .workspace
            .get(file)
            .expect("patched file");
        assert!(
            applied.contains("\n}\n}\n"),
            "expected method close brace to be de-indented: {applied}"
        );
    }

    #[test]
    fn edit_range_safety_rejects_json_patch_edit_outside_allowed_range() {
        let before = "class Test {\n    int add(int a, int b) {\n    }\n}\n";
        let file = "Test.java";
        let workspace = VirtualWorkspace::new([(file.to_string(), before.to_string())]);

        let method_line = "    int add(int a, int b) {";
        let open_brace_offset = before
            .find(method_line)
            .expect("method line")
            .saturating_add(method_line.len().saturating_sub(1));
        let close_brace_offset = before
            .find("\n    }\n")
            .expect("method close")
            .saturating_add("\n    ".len());

        let allowed_range = patch_range_for_offsets(before, open_brace_offset + 1, close_brace_offset);

        // Attempt to rename the method outside the allowed method-body range.
        let name_start = before.find("add").expect("method name");
        let name_end = name_start + "add".len();
        let name_range = patch_range_for_offsets(before, name_start, name_end);

        let provider = StaticProvider {
            response: format!(
                r#"{{
  "edits": [{{
    "file": "{file}",
    "range": {{ "start": {{ "line": {start_line}, "character": {start_ch} }}, "end": {{ "line": {end_line}, "character": {end_ch} }} }},
    "text": "sum"
  }}]
}}"#,
                start_line = name_range.start.line,
                start_ch = name_range.start.character,
                end_line = name_range.end.line,
                end_ch = name_range.end.character,
            ),
        };

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.edit_range_safety = Some(EditRangeSafetyConfig {
            file: file.to_string(),
            allowed_range,
        });

        let cancel = CancellationToken::new();
        let err = block_on(generate_patch(
            &provider,
            &workspace,
            "Rename method.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect_err("patch should be rejected");

        let CodeGenerationError::EditRangeSafety(message) = err else {
            panic!("expected EditRangeSafety, got {err:?}");
        };
        assert!(
            message.contains("outside the allowed range") || message.contains("outside the allowed"),
            "expected clear range-safety message, got: {message}"
        );
    }

    #[test]
    fn edit_range_safety_rejects_unified_diff_edit_outside_allowed_range() {
        let before = "class Test {\n    int add(int a, int b) {\n    }\n}\n";
        let file = "Test.java";
        let workspace = VirtualWorkspace::new([(file.to_string(), before.to_string())]);

        let method_line = "    int add(int a, int b) {";
        let open_brace_offset = before
            .find(method_line)
            .expect("method line")
            .saturating_add(method_line.len().saturating_sub(1));
        let close_brace_offset = before
            .find("\n    }\n")
            .expect("method close")
            .saturating_add("\n    ".len());

        let allowed_range = patch_range_for_offsets(before, open_brace_offset + 1, close_brace_offset);

        let provider = StaticProvider {
            response: format!(
                r#"--- a/{file}
+++ b/{file}
@@ -1,4 +1,4 @@
 class Test {{
-    int add(int a, int b) {{
+    int sum(int a, int b) {{
     }}
 }}
"#
            ),
        };

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.edit_range_safety = Some(EditRangeSafetyConfig {
            file: file.to_string(),
            allowed_range,
        });

        let cancel = CancellationToken::new();
        let err = block_on(generate_patch(
            &provider,
            &workspace,
            "Rename method.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect_err("patch should be rejected");

        let CodeGenerationError::EditRangeSafety(message) = err else {
            panic!("expected EditRangeSafety, got {err:?}");
        };
        assert!(
            message.contains("outside the allowed range") || message.contains("outside the allowed"),
            "expected clear range-safety message, got: {message}"
        );
    }

    #[test]
    fn edit_range_safety_is_checked_pre_format_for_unified_diffs() {
        // Intentionally unformatted Java file. Formatting will rewrite whitespace outside the
        // method body, so range-safety must be enforced pre-format.
        let before = "public class Example{public int add(int a,int b){return 0;}}";
        let file = "Example.java";
        let workspace = VirtualWorkspace::new([(file.to_string(), before.to_string())]);

        let method_sig = "add(int a,int b){";
        let open_brace_offset = before
            .find(method_sig)
            .expect("method sig")
            .saturating_add(method_sig.len().saturating_sub(1));
        let close_brace_offset = before
            .find("return 0;}")
            .expect("method close")
            .saturating_add("return 0;".len());
        let allowed_range =
            patch_range_for_offsets(before, open_brace_offset + 1, close_brace_offset);

        let provider = StaticProvider {
            response: format!(
                r#"--- a/{file}
+++ b/{file}
@@ -1 +1 @@
-public class Example{{public int add(int a,int b){{return 0;}}}}
+public class Example{{public int add(int a,int b){{return a + b;}}}}
"#
            ),
        };

        let mut config = CodeGenerationConfig {
            allow_repair: false,
            ..CodeGenerationConfig::default()
        };
        config.edit_range_safety = Some(EditRangeSafetyConfig {
            file: file.to_string(),
            allowed_range,
        });

        let cancel = CancellationToken::new();
        let result = block_on(generate_patch(
            &provider,
            &workspace,
            "Update method body.",
            &config,
            &AiPrivacyConfig::default(),
            &cancel,
            None,
        ))
        .expect("patch should be accepted");

        let applied = result
            .applied
            .workspace
            .get(file)
            .expect("patched file");
        assert!(applied.contains("return a + b;"), "{applied}");

        let formatted = result
            .formatted_workspace
            .get(file)
            .expect("formatted file");
        assert!(
            formatted.contains("public class Example {"),
            "expected formatter to change whitespace outside method body: {formatted}"
        );
    }
}
