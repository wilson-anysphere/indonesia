use crate::rpc_out::RpcOut;
use crate::ServerState;
use crate::stdio_paths::{load_document_text, path_from_uri};
use crate::stdio_progress::{
  chunk_utf8_by_bytes, send_log_message, send_progress_begin, send_progress_end,
  send_progress_report,
};

use lsp_server::RequestId;
use lsp_types::{Position as LspTypesPosition, Range as LspTypesRange, Uri as LspUri};
use nova_ai::context::{
  ContextDiagnostic, ContextDiagnosticKind, ContextDiagnosticSeverity, ContextRequest,
};
use nova_ai_codegen::{
  CodeGenerationConfig, CodegenProgressEvent, CodegenProgressReporter, CodegenProgressStage,
  PromptCompletionError, PromptCompletionProvider,
};
use nova_db::InMemoryFileStore;
use nova_ide::{
  CodeReviewArgs, ExplainErrorArgs, GenerateMethodBodyArgs, GenerateTestsArgs,
};
use nova_lsp::{AiCodeAction, AiCodeActionExecutor, CodeActionError, CodeActionOutcome};
use serde::Deserialize;
use serde_json::json;
use std::path::{Component, Path, PathBuf};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

pub(super) type AiRpcError = (i32, String, Option<serde_json::Value>);
pub(super) type AiRpcResult = Result<serde_json::Value, AiRpcError>;

const AI_ERROR_KIND_NOT_CONFIGURED: &str = "notConfigured";
const AI_ERROR_KIND_EXCLUDED_PATH: &str = "excludedPath";
const AI_ERROR_KIND_POLICY: &str = "policy";
const AI_ERROR_KIND_PROVIDER: &str = "provider";
const AI_ERROR_KIND_PATCH_PARSE: &str = "patchParse";
const AI_ERROR_KIND_PATCH_SAFETY: &str = "patchSafety";
const AI_ERROR_KIND_PATCH_APPLY: &str = "patchApply";
const AI_ERROR_KIND_VALIDATION: &str = "validation";
const AI_ERROR_KIND_CANCELLED: &str = "cancelled";

// Keep JSON-RPC error payloads bounded so editor clients can render failures reliably.
const AI_ERROR_DATA_MAX_DIAGNOSTICS: usize = 20;
const AI_ERROR_DATA_MAX_PATHS: usize = 50;
const AI_ERROR_DATA_MAX_STRING_BYTES: usize = 1024;
const AI_ERROR_DATA_MAX_CONTEXT_BYTES: usize = 2048;

fn truncate_json_string(value: &str, max_bytes: usize) -> String {
  if value.len() <= max_bytes {
    return value.to_owned();
  }
  if max_bytes == 0 {
    return String::new();
  }

  const SUFFIX: &str = "…";
  if max_bytes <= SUFFIX.len() {
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
      end = end.saturating_sub(1);
    }
    return value[..end].to_owned();
  }

  let max_content = max_bytes.saturating_sub(SUFFIX.len());
  let mut end = max_content;
  while end > 0 && !value.is_char_boundary(end) {
    end = end.saturating_sub(1);
  }

  let mut out = value[..end].to_owned();
  out.push_str(SUFFIX);
  out
}

fn patch_parse_subkind(err: &nova_ai::patch::PatchParseError) -> &'static str {
  use nova_ai::patch::PatchParseError as E;
  match err {
    E::UnsupportedFormat => "unsupportedFormat",
    E::InvalidJson(_) => "invalidJson",
    E::EmptyJsonPatch => "emptyJsonPatch",
    E::InvalidDiff(_) => "invalidDiff",
  }
}

fn patch_apply_subkind(err: &nova_ai::workspace::PatchApplyError) -> &'static str {
  use nova_ai::workspace::PatchApplyError as E;
  match err {
    E::InvalidRange { .. } => "invalidRange",
    E::OverlappingEdits { .. } => "overlappingEdits",
    E::MissingFile { .. } => "missingFile",
    E::NewFileNotAllowed { .. } => "newFileNotAllowed",
    E::FileAlreadyExists { .. } => "fileAlreadyExists",
    E::InvalidUnifiedDiff(_) => "invalidUnifiedDiff",
    E::UnifiedDiffApplyFailed(_) => "unifiedDiffApplyFailed",
  }
}

fn patch_safety_subkind(err: &nova_ai::safety::SafetyError) -> &'static str {
  use nova_ai::safety::SafetyError as E;
  match err {
    E::TooManyFiles { .. } => "tooManyFiles",
    E::TooManyInsertedChars { .. } => "tooManyInsertedChars",
    E::TooManyDeletedChars { .. } => "tooManyDeletedChars",
    E::TooManyHunks { .. } => "tooManyHunks",
    E::EditSpanTooLarge { .. } => "editSpanTooLarge",
    E::ExcludedPath { .. } => "excludedPath",
    E::NotAllowedPath { .. } => "notAllowedPath",
    E::NonRelativePath { .. } => "nonRelativePath",
    E::DisallowedFileExtension { .. } => "disallowedFileExtension",
    E::InvalidExcludedGlob { .. } => "invalidExcludedGlob",
    E::NewFileNotAllowed { .. } => "newFileNotAllowed",
    E::DeleteNotAllowed { .. } => "deleteNotAllowed",
    E::RenameNotAllowed { .. } => "renameNotAllowed",
    E::NewImports { .. } => "newImports",
    E::EditOutsideAllowedRange { .. } => "editOutsideAllowedRange",
  }
}

fn validation_severity_string(severity: &impl std::fmt::Debug) -> String {
  // `Severity` is an internal enum without a `Serialize`/`Display` impl; use its debug string
  // and normalize to a predictable lower-case payload.
  format!("{severity:?}").to_lowercase()
}

fn excluded_paths_error_data(mut paths: Vec<String>) -> serde_json::Value {
  let total = paths.len();
  paths.sort();
  paths.truncate(AI_ERROR_DATA_MAX_PATHS);
  let truncated = total > paths.len();
  let paths: Vec<String> = paths
    .into_iter()
    .map(|p| truncate_json_string(&p, AI_ERROR_DATA_MAX_STRING_BYTES))
    .collect();

  json!({
    "kind": AI_ERROR_KIND_EXCLUDED_PATH,
    "paths": paths,
    "pathsTotal": total,
    "pathsTruncated": truncated,
  })
}

const AI_POLICY_CLOUD_EDITS_DISABLED: &str = "cloudEditsDisabled";
const AI_POLICY_CLOUD_EDITS_WITH_ANONYMIZATION_ENABLED: &str =
  "cloudEditsWithAnonymizationEnabled";
const AI_POLICY_CLOUD_EDITS_WITHOUT_ANONYMIZATION_DISABLED: &str =
  "cloudEditsWithoutAnonymizationDisabled";

fn rpc_error((code, message): (i32, String)) -> AiRpcError {
  (code, message, None)
}

fn rpc_error_with_kind(code: i32, message: impl Into<String>, kind: &'static str) -> AiRpcError {
  (
    code,
    message.into(),
    Some(json!({
      "kind": kind,
    })),
  )
}

fn rpc_error_with_data(
  code: i32,
  message: impl Into<String>,
  data: serde_json::Value,
) -> AiRpcError {
  (code, message.into(), Some(data))
}

fn code_edit_policy_error(code: i32, err: nova_ai::CodeEditPolicyError) -> AiRpcError {
  let policy = match err {
    nova_ai::CodeEditPolicyError::CloudEditsDisabled => AI_POLICY_CLOUD_EDITS_DISABLED,
    nova_ai::CodeEditPolicyError::CloudEditsWithAnonymizationEnabled => {
      AI_POLICY_CLOUD_EDITS_WITH_ANONYMIZATION_ENABLED
    }
    nova_ai::CodeEditPolicyError::CloudEditsWithoutAnonymizationDisabled => {
      AI_POLICY_CLOUD_EDITS_WITHOUT_ANONYMIZATION_DISABLED
    }
  };
  rpc_error_with_data(
    code,
    err.to_string(),
    json!({
      "kind": AI_ERROR_KIND_POLICY,
      "policy": policy,
    }),
  )
}

fn prompt_completion_error(code: i32, err: PromptCompletionError) -> AiRpcError {
  match err {
    PromptCompletionError::Cancelled => {
      rpc_error_with_kind(-32800, "Request cancelled", AI_ERROR_KIND_CANCELLED)
    }
    PromptCompletionError::Provider(_) => {
      rpc_error_with_kind(code, err.to_string(), AI_ERROR_KIND_PROVIDER)
    }
  }
}

fn code_generation_error(code: i32, err: nova_ai_codegen::CodeGenerationError) -> AiRpcError {
  use nova_ai_codegen::CodeGenerationError as E;
  match err {
    E::Cancelled => rpc_error_with_kind(-32800, "Request cancelled", AI_ERROR_KIND_CANCELLED),
    E::Policy(policy) => code_edit_policy_error(code, policy),
    E::InvalidPrivacyConfig(_) => (code, err.to_string(), None),
    E::WorkspaceContainsExcludedPaths { paths } => {
      let total = paths.len();
      let message = format!(
        "AI code edits are blocked because the workspace contains files matching ai.privacy.excluded_paths ({total} file(s)). \
Those files must never be sent to an LLM. Remove them from the workspace snapshot or update ai.privacy.excluded_paths.",
      );

      rpc_error_with_data(
        -32600,
        message,
        excluded_paths_error_data(paths),
      )
    }
    E::Provider(provider) => prompt_completion_error(code, provider),
    E::PatchParse(parse_err) => rpc_error_with_data(
      code,
      parse_err.to_string(),
      json!({
        "kind": AI_ERROR_KIND_PATCH_PARSE,
        "subkind": patch_parse_subkind(&parse_err),
        "message": truncate_json_string(&parse_err.to_string(), AI_ERROR_DATA_MAX_STRING_BYTES),
      }),
    ),
    E::Safety(safety_err) => rpc_error_with_data(
      code,
      safety_err.to_string(),
      json!({
        "kind": AI_ERROR_KIND_PATCH_SAFETY,
        "subkind": patch_safety_subkind(&safety_err),
        "message": truncate_json_string(&safety_err.to_string(), AI_ERROR_DATA_MAX_STRING_BYTES),
      }),
    ),
    E::Apply(apply_err) => rpc_error_with_data(
      code,
      apply_err.to_string(),
      json!({
        "kind": AI_ERROR_KIND_PATCH_APPLY,
        "subkind": patch_apply_subkind(&apply_err),
        "message": truncate_json_string(&apply_err.to_string(), AI_ERROR_DATA_MAX_STRING_BYTES),
      }),
    ),
    E::EditRangeSafety(message) => rpc_error_with_data(
      code,
      message.clone(),
      json!({
        "kind": AI_ERROR_KIND_PATCH_SAFETY,
        "subkind": "editRangeSafety",
        "message": truncate_json_string(&message, AI_ERROR_DATA_MAX_STRING_BYTES),
      }),
    ),
    E::InvalidInsertRange { .. } => {
      rpc_error_with_kind(-32602, err.to_string(), AI_ERROR_KIND_VALIDATION)
    },
    E::ValidationFailed { report } => {
      let diagnostics_total = report.new_diagnostics.len();
      let diagnostics: Vec<serde_json::Value> = report
        .new_diagnostics
        .iter()
        .take(AI_ERROR_DATA_MAX_DIAGNOSTICS)
        .map(|diag| {
          json!({
            "file": truncate_json_string(&diag.file, AI_ERROR_DATA_MAX_STRING_BYTES),
            "code": truncate_json_string(diag.diagnostic.code.as_ref(), AI_ERROR_DATA_MAX_STRING_BYTES),
            "severity": validation_severity_string(&diag.diagnostic.severity),
            "message": truncate_json_string(&diag.diagnostic.message, AI_ERROR_DATA_MAX_STRING_BYTES),
            "position": {
              "line": diag.position.line,
              "character": diag.position.character,
            },
            "context": truncate_json_string(&diag.context, AI_ERROR_DATA_MAX_CONTEXT_BYTES),
          })
        })
        .collect();

      let summary = truncate_json_string(&report.summary, AI_ERROR_DATA_MAX_STRING_BYTES);
      rpc_error_with_data(
        code,
        summary.clone(),
        json!({
          "kind": AI_ERROR_KIND_VALIDATION,
          "subkind": "validationFailed",
          "summary": summary,
          "diagnostics": diagnostics,
          "diagnosticsTotal": diagnostics_total,
          "diagnosticsTruncated": diagnostics_total > AI_ERROR_DATA_MAX_DIAGNOSTICS,
        }),
      )
    }
  }
}

fn code_action_error(code: i32, err: CodeActionError) -> AiRpcError {
  match err {
    CodeActionError::Provider(provider) => prompt_completion_error(code, provider),
    CodeActionError::Codegen(codegen) => code_generation_error(code, codegen),
  }
}

const VALIDATION_OPEN_DOCUMENT_MAX_FILES: usize = 20;
const VALIDATION_OPEN_DOCUMENT_MAX_BYTES: usize = 256 * 1024;

fn path_to_forward_slash_rel(path: &Path) -> Option<String> {
  let mut parts: Vec<String> = Vec::new();
  for component in path.components() {
    match component {
      Component::Normal(seg) => parts.push(seg.to_string_lossy().to_string()),
      // Skip `.` segments.
      Component::CurDir => {}
      // Reject any other component kinds (`..`, prefixes, root dirs).
      _ => return None,
    }
  }

  if parts.is_empty() {
    return None;
  }

  Some(parts.join("/"))
}

fn insert_open_documents_for_validation(
  state: &ServerState,
  ai: &nova_ai::NovaAi,
  root_uri: &LspUri,
  prompt_file_rel: &str,
  workspace: &mut nova_ai::workspace::VirtualWorkspace,
) {
  let Some(root_path) = path_from_uri(root_uri.as_str()) else {
    return;
  };

  let mut candidates: Vec<(String, nova_core::FileId)> = Vec::new();
  for file_id in state.analysis.vfs.open_documents().snapshot() {
    let Some(path) = state.analysis.vfs.path_for_id(file_id) else {
      continue;
    };
    let Some(abs_path) = path.as_local_path() else {
      continue;
    };
    if ai.is_excluded_path(abs_path) {
      continue;
    }
    let Some(ext) = abs_path.extension().and_then(|ext| ext.to_str()) else {
      continue;
    };
    if !ext.eq_ignore_ascii_case("java") {
      continue;
    }
    let Ok(rel) = abs_path.strip_prefix(&root_path) else {
      continue;
    };
    let Some(file_rel) = path_to_forward_slash_rel(rel) else {
      continue;
    };

    // Prompt injection is handled elsewhere; this workspace augmentation is for validation only.
    // Avoid introducing new prompt context by skipping the prompt file unless it was already
    // explicitly inserted into the workspace snapshot.
    if file_rel == prompt_file_rel {
      continue;
    }
    if workspace.get(&file_rel).is_some() {
      continue;
    }

    let Some(text) = state.analysis.file_contents.get(&file_id) else {
      continue;
    };
    if text.len() > VALIDATION_OPEN_DOCUMENT_MAX_BYTES {
      continue;
    }

    candidates.push((file_rel, file_id));
  }

  candidates.sort_by(|a, b| a.0.cmp(&b.0));
  candidates.truncate(VALIDATION_OPEN_DOCUMENT_MAX_FILES);

  for (file_rel, file_id) in candidates {
    let Some(text) = state.analysis.file_contents.get(&file_id) else {
      continue;
    };
    workspace.insert(file_rel, text.as_str().to_owned());
  }
}

pub(super) fn is_ai_excluded_path(state: &ServerState, path: &Path) -> bool {
  if !state.ai_config.enabled {
    return false;
  }

  is_excluded_by_ai_privacy(state, path)
}

fn is_excluded_by_ai_privacy(state: &ServerState, path: &Path) -> bool {
  match state.ai_privacy_excluded_matcher.as_ref() {
    Ok(matcher) => matcher.is_match(path),
    // Best-effort fail-closed: if privacy configuration is invalid, avoid starting any AI work
    // based on potentially sensitive files.
    Err(_) => true,
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AiActionFeature {
  ExplainErrors,
  CodeActions,
  CodeReview,
}

impl AiActionFeature {
  pub(super) fn config_key(self) -> &'static str {
    match self {
      AiActionFeature::ExplainErrors => "ai.features.explain_errors",
      AiActionFeature::CodeActions => "ai.features.code_actions",
      AiActionFeature::CodeReview => "ai.features.code_review",
    }
  }
}

pub(super) fn ai_action_feature_for_method(method: &str) -> Option<AiActionFeature> {
  match method {
    nova_lsp::AI_EXPLAIN_ERROR_METHOD => Some(AiActionFeature::ExplainErrors),
    nova_lsp::AI_GENERATE_METHOD_BODY_METHOD | nova_lsp::AI_GENERATE_TESTS_METHOD => {
      Some(AiActionFeature::CodeActions)
    }
    nova_lsp::AI_CODE_REVIEW_METHOD => Some(AiActionFeature::CodeReview),
    _ => None,
  }
}

pub(super) fn ai_action_feature_for_command(command: &str) -> Option<AiActionFeature> {
  match command {
    nova_ide::COMMAND_EXPLAIN_ERROR => Some(AiActionFeature::ExplainErrors),
    nova_ide::COMMAND_GENERATE_METHOD_BODY | nova_ide::COMMAND_GENERATE_TESTS => {
      Some(AiActionFeature::CodeActions)
    }
    nova_ide::COMMAND_CODE_REVIEW => Some(AiActionFeature::CodeReview),
    _ => None,
  }
}

pub(super) fn ai_action_feature_enabled(state: &ServerState, feature: AiActionFeature) -> bool {
  match feature {
    AiActionFeature::ExplainErrors => state.ai_config.features.explain_errors,
    AiActionFeature::CodeActions => state.ai_config.features.code_actions,
    AiActionFeature::CodeReview => state.ai_config.features.code_review,
  }
}

pub(super) fn ai_action_feature_disabled_message(feature: AiActionFeature) -> String {
  match feature {
    AiActionFeature::ExplainErrors => format!(
      "AI explain-error actions are disabled ({key}=false). To enable, set {key}=true in nova.toml. \
Note: the server can hard-disable all AI code actions via NOVA_DISABLE_AI_CODE_ACTIONS=1.",
      key = feature.config_key(),
    ),
    AiActionFeature::CodeActions => format!(
      "AI code actions are disabled ({key}=false). To enable, set {key}=true in nova.toml. \
Note: the server can hard-disable AI code actions via NOVA_DISABLE_AI_CODE_ACTIONS=1.",
      key = feature.config_key(),
    ),
    AiActionFeature::CodeReview => format!(
      "AI code review is disabled ({key}=false). To enable, set {key}=true in nova.toml. \
Note: the server can hard-disable AI code review via NOVA_DISABLE_AI_CODE_REVIEW=1.",
      key = feature.config_key(),
    ),
  }
}

pub(super) fn ai_action_feature_disabled_error(feature: AiActionFeature) -> (i32, String) {
  (-32600, ai_action_feature_disabled_message(feature))
}

pub(super) fn ai_action_feature_disabled_error_data(feature: AiActionFeature) -> serde_json::Value {
  json!({
    "kind": "disabled",
    "feature": feature.config_key(),
  })
}

pub(super) fn ai_action_feature_disabled_rpc_error(feature: AiActionFeature) -> AiRpcError {
  let (code, message) = ai_action_feature_disabled_error(feature);
  let data = ai_action_feature_disabled_error_data(feature);
  (code, message, Some(data))
}

pub(super) fn handle_ai_custom_request<O: RpcOut + Sync>(
  method: &str,
  params: serde_json::Value,
  state: &mut ServerState,
  rpc_out: &O,
  cancel: &CancellationToken,
) -> AiRpcResult {
  #[derive(Debug, Deserialize)]
  #[serde(rename_all = "camelCase")]
  struct AiRequestParams<T> {
    #[serde(default)]
    work_done_token: Option<serde_json::Value>,
    #[serde(flatten)]
    args: T,
  }

  match method {
    nova_lsp::AI_EXPLAIN_ERROR_METHOD => {
      let params: AiRequestParams<ExplainErrorArgs> =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string(), None))?;
      run_ai_explain_error(
        params.args,
        params.work_done_token,
        state,
        rpc_out,
        cancel.clone(),
      )
    }
    nova_lsp::AI_CODE_REVIEW_METHOD => {
      let params: AiRequestParams<CodeReviewArgs> =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string(), None))?;
      run_ai_code_review(
        params.args.diff,
        params.args.uri,
        params.work_done_token,
        state,
        rpc_out,
        cancel.clone(),
      )
    }
    nova_lsp::AI_MODELS_METHOD => {
      // Allow `params` to be `{}` or `null`.
      let _: Option<serde_json::Value> =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string(), None))?;
      run_ai_models(state, cancel.clone())
    }
    nova_lsp::AI_STATUS_METHOD => {
      // Allow `params` to be `{}` or `null`.
      let _: Option<serde_json::Value> =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string(), None))?;
      Ok(ai_status_payload(state))
    }
    nova_lsp::AI_GENERATE_METHOD_BODY_METHOD => {
      let params: AiRequestParams<GenerateMethodBodyArgs> =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string(), None))?;
      run_ai_generate_method_body_apply(
        params.args,
        params.work_done_token,
        state,
        rpc_out,
        cancel.clone(),
      )
    }
    nova_lsp::AI_GENERATE_TESTS_METHOD => {
      let params: AiRequestParams<GenerateTestsArgs> =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string(), None))?;
      run_ai_generate_tests_apply(
        params.args,
        params.work_done_token,
        state,
        rpc_out,
        cancel.clone(),
      )
    }
    _ => Err((-32601, format!("Method not found: {method}"), None)),
  }
}

pub(super) fn run_ai_code_review(
  diff: String,
  uri: Option<String>,
  work_done_token: Option<serde_json::Value>,
  state: &mut ServerState,
  rpc_out: &impl RpcOut,
  cancel: CancellationToken,
) -> AiRpcResult {
  let ai = state
    .ai
    .as_ref()
    .ok_or_else(|| {
      rpc_error_with_kind(-32600, "AI is not configured", AI_ERROR_KIND_NOT_CONFIGURED)
    })?;
  if !state.ai_config.features.code_review {
    return Err(ai_action_feature_disabled_rpc_error(AiActionFeature::CodeReview));
  }
  let runtime = state
    .runtime
    .as_ref()
    .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string(), None))?;

  let uri_path = uri.as_deref().and_then(path_from_uri);
  let excluded = uri_path
    .as_deref()
    .is_some_and(|path| is_ai_excluded_path(state, path));

  // Mirror `explainError` excluded-path behavior: if the client provides a URI that is blocked by
  // `ai.privacy.excluded_paths`, omit the diff content before sending anything to the model.
  let diff = if excluded {
    "[diff omitted due to excluded_paths]".to_string()
  } else {
    diff
  };

  send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Code review").map_err(rpc_error)?;
  send_progress_report(rpc_out, work_done_token.as_ref(), "Calling model…", None)
    .map_err(rpc_error)?;
  send_log_message(rpc_out, "AI: reviewing diff…").map_err(rpc_error)?;

  let output = runtime
    .block_on(ai.code_review(&diff, cancel.clone()))
    .map_err(|e| {
      let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
      match e {
        nova_ai::AiError::Cancelled => {
          rpc_error_with_kind(-32800, "Request cancelled", AI_ERROR_KIND_CANCELLED)
        }
        other => rpc_error_with_kind(-32603, other.to_string(), AI_ERROR_KIND_PROVIDER),
      }
    })?;

  send_log_message(rpc_out, "AI: review ready").map_err(rpc_error)?;
  send_ai_output(rpc_out, "AI codeReview", &output)?;
  send_progress_end(rpc_out, work_done_token.as_ref(), "Done").map_err(rpc_error)?;
  Ok(serde_json::Value::String(output))
}

pub(super) fn run_ai_models(
  state: &mut ServerState,
  cancel: CancellationToken,
) -> AiRpcResult {
  let ai = state
    .ai
    .as_ref()
    .ok_or_else(|| {
      rpc_error_with_kind(-32600, "AI is not configured", AI_ERROR_KIND_NOT_CONFIGURED)
    })?;
  let runtime = state
    .runtime
    .as_ref()
    .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string(), None))?;

  let llm = ai.llm();
  let models = runtime
    .block_on(llm.list_models(cancel))
    // Best-effort: provider model listing is optional and may not be implemented (or may return
    // a 404/unsupported response). Return an empty list rather than failing the request.
    .unwrap_or_default();

  Ok(json!({ "models": models }))
}

fn env_truthy(key: &str) -> bool {
  matches!(
    std::env::var(key).as_deref(),
    Ok("1") | Ok("true") | Ok("TRUE")
  )
}

fn ai_status_payload(state: &ServerState) -> serde_json::Value {
  let disable_ai = env_truthy("NOVA_DISABLE_AI");
  let disable_ai_completions = env_truthy("NOVA_DISABLE_AI_COMPLETIONS");
  let disable_ai_code_actions = env_truthy("NOVA_DISABLE_AI_CODE_ACTIONS");
  let disable_ai_code_review = env_truthy("NOVA_DISABLE_AI_CODE_REVIEW");

  json!({
    "enabled": state.ai_config.enabled,
    "configured": state.ai.is_some(),
    "providerKind": &state.ai_config.provider.kind,
    "model": &state.ai_config.provider.model,
    "privacy": {
      "localOnly": state.ai_config.privacy.local_only,
      "anonymizeIdentifiers": state.privacy.anonymize_identifiers,
      "includeFilePaths": state.privacy.include_file_paths,
      "excludedPathsCount": state.ai_config.privacy.excluded_paths.len(),
    },
    "features": {
      "completion_ranking": state.ai_config.features.completion_ranking,
      "semantic_search": state.ai_config.features.semantic_search,
      "multi_token_completion": state.ai_config.features.multi_token_completion,
      "explain_errors": state.ai_config.features.explain_errors,
      "code_actions": state.ai_config.features.code_actions,
      "code_review": state.ai_config.features.code_review,
      "code_review_max_diff_chars": state.ai_config.features.code_review_max_diff_chars,
    },
    "cacheEnabled": state.ai_config.cache_enabled,
    "auditLogEnabled": state.ai_config.audit_log.enabled,
    "envOverrides": {
      "disableAi": disable_ai,
      "disableAiCompletions": disable_ai_completions,
      "disableAiCodeActions": disable_ai_code_actions,
      "disableAiCodeReview": disable_ai_code_review,
    }
  })
}

struct LlmPromptCompletionProvider<'a> {
  llm: &'a dyn nova_ai::LlmClient,
}

#[async_trait::async_trait]
impl<'a> PromptCompletionProvider for LlmPromptCompletionProvider<'a> {
  async fn complete(
    &self,
    prompt: &str,
    cancel: &nova_ai::CancellationToken,
  ) -> Result<String, PromptCompletionError> {
    let request = nova_ai::ChatRequest {
      messages: vec![nova_ai::ChatMessage::user(prompt.to_string())],
      max_tokens: None,
      temperature: None,
    };
    self.llm
      .chat(request, cancel.clone())
      .await
      .map_err(|err| match err {
        nova_ai::AiError::Cancelled => PromptCompletionError::Cancelled,
        other => PromptCompletionError::Provider(other.to_string()),
      })
  }
}

pub(super) fn run_ai_explain_error(
  args: ExplainErrorArgs,
  work_done_token: Option<serde_json::Value>,
  state: &mut ServerState,
  rpc_out: &impl RpcOut,
  cancel: CancellationToken,
) -> AiRpcResult {
  let ai = state
    .ai
    .as_ref()
    .ok_or_else(|| {
      rpc_error_with_kind(-32600, "AI is not configured", AI_ERROR_KIND_NOT_CONFIGURED)
    })?;
  if !state.ai_config.features.explain_errors {
    return Err(ai_action_feature_disabled_rpc_error(AiActionFeature::ExplainErrors));
  }
  let runtime = state
    .runtime
    .as_ref()
    .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string(), None))?;

  let uri_path = args.uri.as_deref().and_then(path_from_uri);
  let excluded = uri_path
    .as_deref()
    .is_some_and(|path| is_ai_excluded_path(state, path));

  send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Explain this error")
    .map_err(rpc_error)?;
  send_progress_report(rpc_out, work_done_token.as_ref(), "Building context…", None)
    .map_err(rpc_error)?;
  send_log_message(rpc_out, "AI: explaining error…").map_err(rpc_error)?;
  let mut ctx = if excluded {
    // `ai.privacy.excluded_paths` is a server-side hard stop for sending file-backed code to the
    // model. Even if a client supplies `code`, omit it and build a diagnostic-only prompt.
    //
    // Keep this conservative: don't run semantic search or attach URI/range metadata that could
    // leak excluded file paths into prompts.
    build_context_request(
      state,
      r#""[code context omitted due to excluded_paths]""#.to_string(),
      None,
    )
  } else {
    build_context_request_from_args(
      state,
      args.uri.as_deref(),
      args.range,
      args.code.unwrap_or_default(),
      /*fallback_enclosing=*/ None,
      /*include_doc_comments=*/ true,
    )
  };
  ctx.diagnostics.push(ContextDiagnostic {
    file: if excluded { None } else { args.uri.clone() },
    range: if excluded {
      None
    } else {
      args.range.map(|range| nova_ai::patch::Range {
        start: nova_ai::patch::Position {
          line: range.start.line,
          character: range.start.character,
        },
        end: nova_ai::patch::Position {
          line: range.end.line,
          character: range.end.character,
        },
      })
    },
    severity: ContextDiagnosticSeverity::Error,
    message: args.diagnostic_message.clone(),
    kind: Some(ContextDiagnosticKind::Other),
  });
  send_progress_report(rpc_out, work_done_token.as_ref(), "Calling model…", None)
    .map_err(rpc_error)?;
  let output = runtime
    .block_on(ai.explain_error(&args.diagnostic_message, ctx, cancel.clone()))
    .map_err(|e| {
      let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
      match e {
        nova_ai::AiError::Cancelled => {
          rpc_error_with_kind(-32800, "Request cancelled", AI_ERROR_KIND_CANCELLED)
        }
        other => rpc_error_with_kind(-32603, other.to_string(), AI_ERROR_KIND_PROVIDER),
      }
    })?;
  send_log_message(rpc_out, "AI: explanation ready").map_err(rpc_error)?;
  send_ai_output(rpc_out, "AI explainError", &output)?;
  send_progress_end(rpc_out, work_done_token.as_ref(), "Done").map_err(rpc_error)?;
  Ok(serde_json::Value::String(output))
}

/// Patch-based AI code-editing helpers (powered by `nova-ai-codegen`).
///
/// The `nova/ai/generateMethodBody` and `nova/ai/generateTests` custom request endpoints apply edits
/// via `workspace/applyEdit` and return JSON `null` on success. When a work-done token is provided,
/// these helpers also emit `$/progress` stage updates.
struct LspCodegenProgress<'a, O: RpcOut + Sync> {
  out: &'a O,
  token: Option<&'a serde_json::Value>,
}

impl<'a, O: RpcOut + Sync> CodegenProgressReporter for LspCodegenProgress<'a, O> {
  fn report(&self, event: CodegenProgressEvent) {
    let message = match event.stage {
      CodegenProgressStage::RepairAttempt => format!("Attempt {}", event.attempt + 1),
      CodegenProgressStage::BuildingPrompt => "Building prompt…".to_string(),
      CodegenProgressStage::ModelCall => "Calling model…".to_string(),
      CodegenProgressStage::ParsingPatch => "Parsing AI patch…".to_string(),
      CodegenProgressStage::ApplyingPatch => "Applying patch…".to_string(),
      CodegenProgressStage::Formatting => "Formatting…".to_string(),
      CodegenProgressStage::Validating => "Validating…".to_string(),
    };
    // Best-effort: ignore transport failures during progress updates.
    let _ = send_progress_report(self.out, self.token, &message, None);
  }
}

pub(super) fn run_ai_generate_method_body_apply<O: RpcOut + Sync>(
  args: GenerateMethodBodyArgs,
  work_done_token: Option<serde_json::Value>,
  state: &mut ServerState,
  rpc_out: &O,
  cancel: CancellationToken,
) -> AiRpcResult {
  let ai = state
    .ai
    .as_ref()
    .ok_or_else(|| {
      rpc_error_with_kind(-32600, "AI is not configured", AI_ERROR_KIND_NOT_CONFIGURED)
    })?;
  if !state.ai_config.features.code_actions {
    return Err(ai_action_feature_disabled_rpc_error(AiActionFeature::CodeActions));
  }
  let runtime = state
    .runtime
    .as_ref()
    .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string(), None))?;

  // AI code generation is a code-editing operation. Enforce privacy policy early so clients
  // always see the policy error even if they invoke the command with incomplete arguments.
  nova_ai::enforce_code_edit_policy(&state.ai_config.privacy)
    .map_err(|e| code_edit_policy_error(-32603, e))?;

  let GenerateMethodBodyArgs {
    method_signature,
    context,
    uri,
    range,
  } = args;

  let uri_string = uri
    .as_deref()
    .ok_or_else(|| (-32602, "missing uri".to_string(), None))?;
  let uri = uri_string
    .parse::<LspUri>()
    .map_err(|e| (-32602, format!("invalid uri: {e}"), None))?;

  let (root_uri, file_rel, abs_path) = resolve_ai_patch_target(&uri, state).map_err(rpc_error)?;

  // Enforce excluded_paths *before* building prompts or calling the model.
  if ai.is_excluded_path(&abs_path) {
    return Err(rpc_error_with_data(
      -32600,
      "AI disabled for this file due to ai.privacy.excluded_paths".to_string(),
      excluded_paths_error_data(vec![file_rel.clone()]),
    ));
  }

  let Some(source) =
    load_document_text(state, uri_string).or_else(|| load_document_text(state, uri.as_str()))
  else {
    return Err((
      -32602,
      format!("missing document text for `{}`", uri.as_str()),
      None,
    ));
  };

  let selection = range.ok_or_else(|| (-32602, "missing range".to_string(), None))?;
  let insert_range =
    insert_range_for_method_body(&source, selection).map_err(|message| (-32602, message, None))?;

  let prompt_context = if state.ai_config.enabled && state.ai_config.features.semantic_search {
    Some(build_context_request_from_args(
      state,
      Some(uri_string),
      Some(selection),
      method_signature.clone(),
      context.clone(),
      /*include_doc_comments=*/ true,
    ))
  } else {
    None
  };

  let mut workspace = nova_ai::workspace::VirtualWorkspace::new([(file_rel.clone(), source)]);
  insert_open_documents_for_validation(state, ai, &root_uri, &file_rel, &mut workspace);

  let llm = ai.llm();
  let provider = LlmPromptCompletionProvider { llm: llm.as_ref() };
  let mut config = CodeGenerationConfig::default();
  config.safety.excluded_path_globs = state.ai_config.privacy.excluded_paths.clone();

  let executor = AiCodeActionExecutor::new(&provider, config, state.ai_config.privacy.clone());

  send_progress_begin(
    rpc_out,
    work_done_token.as_ref(),
    "AI: Generate method body",
  )
  .map_err(rpc_error)?;
  let progress = LspCodegenProgress {
    out: rpc_out,
    token: work_done_token.as_ref(),
  };
  let progress = work_done_token
    .as_ref()
    .map(|_| &progress as &dyn CodegenProgressReporter);

  let outcome = runtime
    .block_on(executor.execute(
      AiCodeAction::GenerateMethodBody {
        file: file_rel,
        insert_range,
        method_signature,
        context,
        prompt_context,
      },
      &workspace,
      &root_uri,
      &cancel,
      progress,
    ))
    .map_err(|err| {
      let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
      code_action_error(-32603, err)
    })?;

  apply_code_action_outcome(outcome, "AI: Generate method body", state, rpc_out).map_err(|err| {
    let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
    err
  })?;
  send_progress_end(rpc_out, work_done_token.as_ref(), "Done").map_err(rpc_error)?;
  // The `nova/ai/*` patch-based endpoints return `null` on success and apply edits via
  // `workspace/applyEdit`.
  Ok(serde_json::Value::Null)
}

pub(super) fn run_ai_generate_tests_apply<O: RpcOut + Sync>(
  args: GenerateTestsArgs,
  work_done_token: Option<serde_json::Value>,
  state: &mut ServerState,
  rpc_out: &O,
  cancel: CancellationToken,
) -> AiRpcResult {
  let ai = state
    .ai
    .as_ref()
    .ok_or_else(|| {
      rpc_error_with_kind(-32600, "AI is not configured", AI_ERROR_KIND_NOT_CONFIGURED)
    })?;
  if !state.ai_config.features.code_actions {
    return Err(ai_action_feature_disabled_rpc_error(AiActionFeature::CodeActions));
  }
  let runtime = state
    .runtime
    .as_ref()
    .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string(), None))?;

  // AI test generation is a code-editing operation. Enforce privacy policy early so clients
  // always see the policy error even if they invoke the command with incomplete arguments.
  nova_ai::enforce_code_edit_policy(&state.ai_config.privacy)
    .map_err(|e| code_edit_policy_error(-32603, e))?;

  let GenerateTestsArgs {
    target,
    context,
    uri,
    range,
  } = args;
  let uri_string = uri
    .as_deref()
    .ok_or_else(|| (-32602, "missing uri".to_string(), None))?;
  let uri = uri_string
    .parse::<LspUri>()
    .map_err(|e| (-32602, format!("invalid uri: {e}"), None))?;

  let (root_uri, file_rel, abs_path) = resolve_ai_patch_target(&uri, state).map_err(rpc_error)?;

  // Enforce excluded_paths *before* building prompts or calling the model.
  if ai.is_excluded_path(&abs_path) {
    return Err(rpc_error_with_data(
      -32600,
      "AI disabled for this file due to ai.privacy.excluded_paths".to_string(),
      excluded_paths_error_data(vec![file_rel.clone()]),
    ));
  }

  let Some(source) =
    load_document_text(state, uri_string).or_else(|| load_document_text(state, uri.as_str()))
  else {
    return Err((
      -32602,
      format!("missing document text for `{}`", uri.as_str()),
      None,
    ));
  };

  let selection = range.ok_or_else(|| (-32602, "missing range".to_string(), None))?;
  // Always validate the incoming selection range (UTF-16 correctness, in-bounds) so we can
  // produce deterministic errors when clients send malformed ranges.
  let selection_range =
    insert_range_from_ide_range(&source, selection).map_err(|message| (-32602, message, None))?;

  let prompt_context = if state.ai_config.enabled && state.ai_config.features.semantic_search {
    Some(build_context_request_from_args(
      state,
      Some(uri_string),
      Some(selection),
      target.clone(),
      context.clone(),
      /*include_doc_comments=*/ true,
    ))
  } else {
    None
  };

  let target = Some(target);
  let source_file = Some(file_rel.clone());
  let source_snippet = byte_range_for_ide_range(&source, selection)
    .and_then(|r| source.get(r).map(|s| s.to_string()))
    .filter(|s| !s.trim().is_empty());

  let llm = ai.llm();
  let provider = LlmPromptCompletionProvider { llm: llm.as_ref() };
  let mut config = CodeGenerationConfig::default();
  config.safety.excluded_path_globs = state.ai_config.privacy.excluded_paths.clone();

  let (action_file, insert_range, mut workspace) =
    if let Some(test_file) = derive_test_file_path(&source, Path::new(&file_rel)) {
      // `derive_test_file_path` returns a workspace-relative path (e.g.
      // `src/test/java/...` or `moduleA/src/test/java/...` for multi-module builds).
      //
      // Enforce `ai.privacy.excluded_paths` on the derived destination to ensure we never
      // generate/modify tests in excluded directories.
      //
      // Match conservatively: treat patterns as matching either paths relative to the
      // workspace root or absolute paths resolved against the root.
      let test_file_is_excluded = ai.is_excluded_path(Path::new(&test_file))
        || state
          .project_root
          .as_deref()
          .is_some_and(|root_path| ai.is_excluded_path(&root_path.join(&test_file)));

      if test_file_is_excluded {
        // Fallback: insert tests into the current file at the selection range.
        config.safety.allowed_path_prefixes = vec![file_rel.clone()];
        (
          file_rel.clone(),
          selection_range,
          nova_ai::workspace::VirtualWorkspace::new([(file_rel.clone(), source)]),
        )
      } else {
        // Patch safety: only allow edits to the derived test file.
        config.safety.allowed_path_prefixes = vec![test_file.clone()];
        config.safety.allow_new_files = true;

        let mut workspace =
          nova_ai::workspace::VirtualWorkspace::new([(file_rel.clone(), source)]);
        let root_path = state
          .project_root
          .as_deref()
          .filter(|root| abs_path.starts_with(root))
          .or_else(|| abs_path.parent())
          .ok_or_else(|| {
            (
              -32603,
              format!("failed to determine workspace root for `{}`", abs_path.display()),
              None,
            )
          })?;
        if let Ok(existing) = std::fs::read_to_string(root_path.join(&test_file)) {
          workspace.insert(test_file.clone(), existing);
        }

        (
          test_file,
          LspTypesRange::new(LspTypesPosition::new(0, 0), LspTypesPosition::new(0, 0)),
          workspace,
        )
      }
    } else {
      (
        file_rel.clone(),
        selection_range,
        nova_ai::workspace::VirtualWorkspace::new([(file_rel.clone(), source)]),
      )
    };

  insert_open_documents_for_validation(state, ai, &root_uri, &action_file, &mut workspace);

  let executor = AiCodeActionExecutor::new(&provider, config, state.ai_config.privacy.clone());

  send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate tests").map_err(rpc_error)?;
  let progress = LspCodegenProgress {
    out: rpc_out,
    token: work_done_token.as_ref(),
  };
  let progress = work_done_token
    .as_ref()
    .map(|_| &progress as &dyn CodegenProgressReporter);

  let outcome = runtime
    .block_on(executor.execute(
      AiCodeAction::GenerateTest {
        file: action_file,
        insert_range,
        target,
        source_file,
        source_snippet,
        context,
        prompt_context,
      },
      &workspace,
      &root_uri,
      &cancel,
      progress,
    ))
    .map_err(|err| {
      let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
      code_action_error(-32603, err)
    })?;

  apply_code_action_outcome(outcome, "AI: Generate tests", state, rpc_out).map_err(|err| {
    let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
    err
  })?;
  send_progress_end(rpc_out, work_done_token.as_ref(), "Done").map_err(rpc_error)?;
  // The `nova/ai/*` patch-based endpoints return `null` on success and apply edits via
  // `workspace/applyEdit`.
  Ok(serde_json::Value::Null)
}

fn apply_code_action_outcome<O: RpcOut>(
  outcome: CodeActionOutcome,
  label: &str,
  state: &mut ServerState,
  rpc_out: &O,
) -> AiRpcResult {
  match outcome {
    CodeActionOutcome::WorkspaceEdit(edit) => {
      let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
        .map_err(|e| (-32603, e.to_string(), None))?;
      rpc_out
        .send_request(
          id,
          "workspace/applyEdit",
          json!({
            "label": label,
            "edit": edit.clone(),
          }),
        )
        .map_err(|e| (-32603, e.to_string(), None))?;
      Ok(serde_json::Value::Null)
    }
    CodeActionOutcome::Explanation(text) => Ok(serde_json::Value::String(text)),
  }
}

fn insert_range_for_method_body(
  source: &str,
  selection: nova_ide::LspRange,
) -> Result<LspTypesRange, String> {
  let selection_range = insert_range_from_ide_range(source, selection)?;
  let pos = nova_lsp::text_pos::TextPos::new(source);
  let selection_bytes = pos.byte_range(selection_range).ok_or_else(|| {
    "invalid selection range (UTF-16 positions may be out of bounds)".to_string()
  })?;

  let selection_text = source
    .get(selection_bytes.clone())
    .ok_or_else(|| "invalid selection range (not on UTF-8 boundaries)".to_string())?;

  let open_rel = selection_text
    .find('{')
    .ok_or_else(|| "selection does not contain an opening `{` for the method body".to_string())?;
  let open_abs = selection_bytes.start.saturating_add(open_rel);

  let tail = source
    .get(open_abs..selection_bytes.end)
    .ok_or_else(|| "invalid method selection bounds".to_string())?;
  let mut depth: i32 = 0;
  let mut close_abs: Option<usize> = None;
  for (idx, ch) in tail.char_indices() {
    match ch {
      '{' => depth += 1,
      '}' => {
        depth -= 1;
        if depth == 0 {
          close_abs = Some(open_abs + idx);
          break;
        }
        if depth < 0 {
          break;
        }
      }
      _ => {}
    }
  }

  let close_abs = close_abs.ok_or_else(|| {
    "selection does not contain a matching `}` for the method body".to_string()
  })?;

  let body = source
    .get(open_abs + 1..close_abs)
    .ok_or_else(|| "invalid method selection bounds".to_string())?;
  if !body.trim().is_empty() {
    return Err("selected method body is not empty; select an empty method".to_string());
  }

  let start = pos
    .lsp_position(open_abs + 1)
    .ok_or_else(|| "failed to convert method body start position".to_string())?;
  let end = pos
    .lsp_position(close_abs)
    .ok_or_else(|| "failed to convert method body end position".to_string())?;

  Ok(LspTypesRange { start, end })
}

fn insert_range_from_ide_range(
  source: &str,
  range: nova_ide::LspRange,
) -> Result<LspTypesRange, String> {
  let lsp_range = LspTypesRange {
    start: LspTypesPosition {
      line: range.start.line,
      character: range.start.character,
    },
    end: LspTypesPosition {
      line: range.end.line,
      character: range.end.character,
    },
  };

  // Validate that the incoming range is usable (UTF-16 correctness, in-bounds).
  if nova_lsp::text_pos::byte_range(source, lsp_range).is_none() {
    return Err("invalid selection range (UTF-16 positions may be out of bounds)".to_string());
  }

  Ok(lsp_range)
}

fn resolve_ai_patch_target(
  uri: &LspUri,
  state: &ServerState,
) -> Result<(LspUri, String, PathBuf), (i32, String)> {
  let abs_path = path_from_uri(uri.as_str()).ok_or_else(|| {
    (
      -32602,
      format!("unsupported uri (expected file://): {}", uri.as_str()),
    )
  })?;

  let (root_uri, file_rel) =
    nova_lsp::patch_paths::patch_root_uri_and_file_rel(state.project_root.as_deref(), &abs_path);

  Ok((root_uri, file_rel, abs_path))
}

fn derive_test_file_path(source_text: &str, source_path: &Path) -> Option<String> {
  // Only derive a `src/test/java/...` target when the source file lives under a conventional
  // `src/main/java` tree. For ad-hoc single-file projects (e.g. `Test.java` in the workspace
  // root), prefer inserting tests into the current file selection.
  //
  // In multi-module Maven/Gradle workspaces, the source file path may be prefixed with the
  // module directory (e.g. `moduleA/src/main/java/...`). Preserve that prefix in the derived
  // destination so generated tests land in `moduleA/src/test/java/...`.
  let components = source_path
    .components()
    .filter_map(|c| c.as_os_str().to_str())
    .collect::<Vec<_>>();
  let src_main_java_idx = components
    .windows(3)
    .position(|window| window == ["src", "main", "java"])?;
  let prefix = components[..src_main_java_idx].join("/");

  let class_name = source_path
    .file_stem()
    .and_then(|s| s.to_str())
    .map(str::to_string)?;
  if !is_java_identifier(&class_name) {
    return None;
  }
  let test_class = format!("{class_name}Test");

  let pkg = crate::stdio_code_lens::parse_java_package(source_text);
  let pkg_path = pkg
    .as_deref()
    .map(|pkg| {
      let segments: Vec<_> = pkg.split('.').collect();
      if segments.is_empty() || segments.iter().any(|s| !is_java_identifier(s)) {
        return None;
      }
      Some(segments.join("/"))
    })
    .unwrap_or(Some(String::new()))?;

  let mut out = String::new();
  if !prefix.is_empty() {
    out.push_str(&prefix);
    out.push('/');
  }
  out.push_str("src/test/java/");
  if !pkg_path.is_empty() {
    out.push_str(&pkg_path);
    out.push('/');
  }
  out.push_str(&test_class);
  out.push_str(".java");
  Some(out)
}

fn is_java_identifier(s: &str) -> bool {
  let mut chars = s.chars();
  let Some(first) = chars.next() else {
    return false;
  };
  if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
    return false;
  }
  chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

pub(super) const AI_LOG_MESSAGE_CHUNK_BYTES: usize = 6 * 1024;

fn send_ai_output(out: &impl RpcOut, label: &str, output: &str) -> Result<(), AiRpcError> {
  let chunks = chunk_utf8_by_bytes(output, AI_LOG_MESSAGE_CHUNK_BYTES);
  let total = chunks.len();
  for (idx, chunk) in chunks.into_iter().enumerate() {
    let message = if total == 1 {
      format!("{label}: {chunk}")
    } else {
      format!("{label} ({}/{total}): {chunk}", idx + 1)
    };
    send_log_message(out, &message).map_err(rpc_error)?;
  }
  Ok(())
}

pub(super) fn maybe_add_related_code(
  state: &ServerState,
  req: ContextRequest,
) -> ContextRequest {
  if !(state.ai_config.enabled && state.ai_config.features.semantic_search) {
    return req;
  }

  // Keep this conservative: extra context is useful, but should not drown the prompt.
  let search = state
    .semantic_search
    .read()
    .unwrap_or_else(|err| err.into_inner());
  let mut req = req.with_related_code_from_focal(search.as_ref(), 3);
  req.related_code
    .retain(|item| !is_ai_excluded_path(state, &item.path));
  req
}

pub(super) fn byte_range_for_ide_range(
  text: &str,
  range: nova_ide::LspRange,
) -> Option<std::ops::Range<usize>> {
  let range = LspTypesRange {
    start: LspTypesPosition {
      line: range.start.line,
      character: range.start.character,
    },
    end: LspTypesPosition {
      line: range.end.line,
      character: range.end.character,
    },
  };
  nova_lsp::text_pos::byte_range(text, range)
}

fn looks_like_project_root(root: &Path) -> bool {
  if !root.is_dir() {
    return false;
  }

  // Avoid expensive filesystem scans when we only have an ad-hoc URI (e.g. `file:///Test.java`)
  // that doesn't correspond to an actual workspace folder.
  const MARKERS: &[&str] = &[
    // VCS roots.
    ".git",
    ".hg",
    // Maven.
    "pom.xml",
    "mvnw",
    "mvnw.cmd",
    // Gradle.
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradlew",
    "gradlew.bat",
    // Bazel.
    "WORKSPACE",
    "WORKSPACE.bazel",
    "MODULE.bazel",
    // Nova workspace config.
    ".nova",
    "nova.toml",
    ".nova.toml",
    "nova.config.toml",
  ];

  if MARKERS.iter().any(|marker| root.join(marker).exists())
    || root.join("src").join("main").join("java").is_dir()
    || root.join("src").join("test").join("java").is_dir()
  {
    return true;
  }

  let src = root.join("src");
  if !src.is_dir() {
    return false;
  }

  // Simple projects: accept a `src/` tree that actually contains Java source files near the
  // top-level. Cap the walk so this stays cheap even for large workspaces.
  let mut inspected = 0usize;
  for entry in WalkDir::new(&src).max_depth(4) {
    let entry = match entry {
      Ok(entry) => entry,
      Err(_) => continue,
    };
    inspected += 1;
    if inspected > 2_000 {
      break;
    }
    if !entry.file_type().is_file() {
      continue;
    }
    if entry
      .path()
      .extension()
      .and_then(|ext| ext.to_str())
      .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
    {
      return true;
    }
  }

  false
}

fn project_context_for_root(root: &Path) -> Option<nova_ai::context::ProjectContext> {
  if !looks_like_project_root(root) {
    return None;
  }

  let config = nova_ide::framework_cache::project_config(root)?;

  let build_system = Some(format!("{:?}", config.build_system));
  let java_version = Some(format!(
    "source {} / target {}",
    config.java.source.0, config.java.target.0
  ));

  let mut frameworks = Vec::new();
  let deps = &config.dependencies;
  if deps
    .iter()
    .any(|d| d.group_id.starts_with("org.springframework"))
  {
    frameworks.push("Spring".to_string());
  }
  if deps.iter().any(|d| {
    d.group_id.contains("micronaut")
      || d.artifact_id.contains("micronaut")
      || d.group_id.starts_with("io.micronaut")
  }) {
    frameworks.push("Micronaut".to_string());
  }
  if deps.iter().any(|d| d.group_id.starts_with("io.quarkus")) {
    frameworks.push("Quarkus".to_string());
  }
  if deps.iter().any(|d| {
    d.group_id.contains("jakarta.persistence")
      || d.group_id.contains("javax.persistence")
      || d.artifact_id.contains("persistence")
  }) {
    frameworks.push("JPA".to_string());
  }
  if deps
    .iter()
    .any(|d| d.group_id == "org.projectlombok" || d.artifact_id == "lombok")
  {
    frameworks.push("Lombok".to_string());
  }
  if deps
    .iter()
    .any(|d| d.group_id.starts_with("org.mapstruct") || d.artifact_id.contains("mapstruct"))
  {
    frameworks.push("MapStruct".to_string());
  }
  if deps
    .iter()
    .any(|d| d.group_id == "com.google.dagger" || d.artifact_id.contains("dagger"))
  {
    frameworks.push("Dagger".to_string());
  }

  frameworks.sort();
  frameworks.dedup();

  let classpath = config
    .classpath
    .iter()
    .chain(config.module_path.iter())
    .map(|entry| entry.path.to_string_lossy().to_string())
    .collect();

  Some(nova_ai::context::ProjectContext {
    build_system,
    java_version,
    frameworks,
    classpath,
  })
}

fn semantic_context_for_hover(
  path: &Path,
  text: &str,
  position: LspTypesPosition,
) -> Option<String> {
  let mut db = InMemoryFileStore::new();
  let file = db.file_id_for_path(path);
  db.set_file_text(file, text.to_string());

  let hover = nova_ide::hover(&db, file, position)?;
  match hover.contents {
    lsp_types::HoverContents::Markup(markup) => Some(markup.value),
    lsp_types::HoverContents::Scalar(marked) => Some(match marked {
      lsp_types::MarkedString::String(s) => s,
      lsp_types::MarkedString::LanguageString(ls) => ls.value,
    }),
    lsp_types::HoverContents::Array(items) => {
      let mut out = String::new();
      for item in items {
        match item {
          lsp_types::MarkedString::String(s) => {
            out.push_str(&s);
            out.push('\n');
          }
          lsp_types::MarkedString::LanguageString(ls) => {
            out.push_str(&ls.value);
            out.push('\n');
          }
        }
      }
      let out = out.trim().to_string();
      if out.is_empty() {
        None
      } else {
        Some(out)
      }
    }
  }
}

pub(super) fn build_context_request(
  state: &ServerState,
  focal_code: String,
  enclosing: Option<String>,
) -> ContextRequest {
  ContextRequest {
    file_path: None,
    focal_code,
    enclosing_context: enclosing,
    project_context: state
      .project_root
      .as_deref()
      .and_then(project_context_for_root),
    semantic_context: None,
    related_symbols: Vec::new(),
    related_code: Vec::new(),
    cursor: None,
    diagnostics: Vec::new(),
    extra_files: Vec::new(),
    doc_comments: None,
    include_doc_comments: false,
    token_budget: 800,
    privacy: state.privacy.clone(),
  }
}

pub(super) fn build_context_request_from_args(
  state: &ServerState,
  uri: Option<&str>,
  range: Option<nova_ide::LspRange>,
  fallback_focal: String,
  fallback_enclosing: Option<String>,
  include_doc_comments: bool,
) -> ContextRequest {
  if let (Some(uri), Some(range)) = (uri, range) {
    if let Some(text) = load_document_text(state, uri) {
      if let Some(selection) = byte_range_for_ide_range(&text, range) {
        let mut req = ContextRequest::for_java_source_range(
          &text,
          selection,
          800,
          state.privacy.clone(),
          include_doc_comments,
        );
        // Store the filesystem path for privacy filtering (excluded_paths) and optional
        // prompt inclusion. The builder will only emit it when `include_file_paths`
        // is enabled.
        if let Some(path) = path_from_uri(uri) {
          req.file_path = Some(path.display().to_string());
          let project_root = state
            .project_root
            .clone()
            .unwrap_or_else(|| nova_ide::framework_cache::project_root_for_path(&path));
          req.project_context = project_context_for_root(&project_root);
          req.semantic_context = semantic_context_for_hover(
            &path,
            &text,
            LspTypesPosition::new(range.start.line, range.start.character),
          );
        }
        req.cursor = Some(nova_ai::patch::Position {
          line: range.start.line,
          character: range.start.character,
        });
        return maybe_add_related_code(state, req);
      }
    }
  }

  maybe_add_related_code(state, build_context_request(state, fallback_focal, fallback_enclosing))
}

pub(super) fn extract_snippet(text: &str, range: &lsp_types::Range, context_lines: u32) -> String {
  let start_line = range.start.line.saturating_sub(context_lines);
  let end_line = range.end.line.saturating_add(context_lines);

  let mut out = String::new();
  for (idx, line) in text.lines().enumerate() {
    let idx_u32 = idx as u32;
    if idx_u32 < start_line || idx_u32 > end_line {
      continue;
    }
    out.push_str(line);
    out.push('\n');
  }
  out
}

pub(super) fn extract_range_text(text: &str, range: &lsp_types::Range) -> Option<String> {
  let range = LspTypesRange {
    start: LspTypesPosition {
      line: range.start.line,
      character: range.start.character,
    },
    end: LspTypesPosition {
      line: range.end.line,
      character: range.end.character,
    },
  };
  let bytes = nova_lsp::text_pos::byte_range(text, range)?;
  text.get(bytes).map(ToString::to_string)
}

pub(super) fn detect_empty_method_signature(selected: &str) -> Option<String> {
  let trimmed = selected.trim();
  let open = trimmed.find('{')?;
  let close = trimmed.rfind('}')?;
  if close <= open {
    return None;
  }
  let body = trimmed[open + 1..close].trim();
  if !body.is_empty() {
    return None;
  }
  Some(trimmed[..open].trim().to_string())
}

fn parse_excluded_paths_env_var(value: &str) -> Vec<String> {
  // `NOVA_AI_EXCLUDED_PATHS` parsing rules:
  // - Split on commas and newlines (both act as separators).
  // - Trim whitespace around each entry.
  // - Ignore empty entries.
  // - Preserve the remaining pattern verbatim (do not normalize path separators).
  value
    .split(|c| c == ',' || c == '\n')
    .map(|part| part.trim())
    .filter(|part| !part.is_empty())
    .map(|part| part.to_string())
    .collect()
}

pub(super) fn load_ai_config_from_env(
) -> Result<Option<(nova_config::AiConfig, nova_ai::PrivacyMode)>, String> {
  let provider = match std::env::var("NOVA_AI_PROVIDER") {
    Ok(p) => p,
    Err(_) => return Ok(None),
  };

  let model = std::env::var("NOVA_AI_MODEL").unwrap_or_else(|_| "default".to_string());
  let api_key = std::env::var("NOVA_AI_API_KEY").ok();
  let provider_max_tokens = match std::env::var("NOVA_AI_MAX_TOKENS") {
    Ok(raw) => {
      let trimmed = raw.trim();
      let value = trimmed.parse::<u32>().map_err(|_| {
        format!("invalid NOVA_AI_MAX_TOKENS: `{raw}` (expected an integer >= 1)")
      })?;
      Some(value.max(1))
    }
    Err(std::env::VarError::NotPresent) => None,
    Err(std::env::VarError::NotUnicode(_)) => {
      return Err("invalid NOVA_AI_MAX_TOKENS: value is not valid unicode".to_string());
    }
  };
  let provider_temperature = match std::env::var("NOVA_AI_TEMPERATURE") {
    Ok(raw) => {
      let trimmed = raw.trim();
      let value = trimmed.parse::<f32>().map_err(|_| {
        format!("invalid NOVA_AI_TEMPERATURE: `{raw}` (expected a float >= 0)")
      })?;
      if value.is_nan() || value < 0.0 {
        return Err(format!(
          "invalid NOVA_AI_TEMPERATURE: `{raw}` (expected a float >= 0)"
        ));
      }
      Some(value)
    }
    Err(std::env::VarError::NotPresent) => None,
    Err(std::env::VarError::NotUnicode(_)) => {
      return Err("invalid NOVA_AI_TEMPERATURE: value is not valid unicode".to_string());
    }
  };
  let provider_concurrency = match std::env::var("NOVA_AI_CONCURRENCY") {
    Ok(raw) => {
      let trimmed = raw.trim();
      let value = trimmed.parse::<usize>().map_err(|_| {
        format!("invalid NOVA_AI_CONCURRENCY: `{raw}` (expected an integer >= 1)")
      })?;
      Some(value.max(1))
    }
    Err(std::env::VarError::NotPresent) => None,
    Err(std::env::VarError::NotUnicode(_)) => {
      return Err("invalid NOVA_AI_CONCURRENCY: value is not valid unicode".to_string());
    }
  };
  let provider_retry_max_retries = match std::env::var("NOVA_AI_RETRY_MAX_RETRIES") {
    Ok(raw) => {
      let trimmed = raw.trim();
      let value = trimmed.parse::<usize>().map_err(|_| {
        format!("invalid NOVA_AI_RETRY_MAX_RETRIES: `{raw}` (expected an integer >= 0)")
      })?;
      Some(value)
    }
    Err(std::env::VarError::NotPresent) => None,
    Err(std::env::VarError::NotUnicode(_)) => {
      return Err("invalid NOVA_AI_RETRY_MAX_RETRIES: value is not valid unicode".to_string());
    }
  };
  let provider_retry_initial_backoff_ms = match std::env::var("NOVA_AI_RETRY_INITIAL_BACKOFF_MS") {
    Ok(raw) => {
      let trimmed = raw.trim();
      let value = trimmed.parse::<u64>().map_err(|_| {
        format!(
          "invalid NOVA_AI_RETRY_INITIAL_BACKOFF_MS: `{raw}` (expected an integer >= 1)"
        )
      })?;
      Some(value.max(1))
    }
    Err(std::env::VarError::NotPresent) => None,
    Err(std::env::VarError::NotUnicode(_)) => {
      return Err(
        "invalid NOVA_AI_RETRY_INITIAL_BACKOFF_MS: value is not valid unicode".to_string(),
      );
    }
  };
  let provider_retry_max_backoff_ms = match std::env::var("NOVA_AI_RETRY_MAX_BACKOFF_MS") {
    Ok(raw) => {
      let trimmed = raw.trim();
      let value = trimmed.parse::<u64>().map_err(|_| {
        format!("invalid NOVA_AI_RETRY_MAX_BACKOFF_MS: `{raw}` (expected an integer >= 1)")
      })?;
      Some(value.max(1))
    }
    Err(std::env::VarError::NotPresent) => None,
    Err(std::env::VarError::NotUnicode(_)) => {
      return Err("invalid NOVA_AI_RETRY_MAX_BACKOFF_MS: value is not valid unicode".to_string());
    }
  };

  let audit_logging = matches!(
    std::env::var("NOVA_AI_AUDIT_LOGGING").as_deref(),
    Ok("1") | Ok("true") | Ok("TRUE")
  );

  let cache_enabled = matches!(
    std::env::var("NOVA_AI_CACHE_ENABLED").as_deref(),
    Ok("1") | Ok("true") | Ok("TRUE")
  );
  let cache_max_entries = std::env::var("NOVA_AI_CACHE_MAX_ENTRIES")
    .ok()
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or(256);
  let cache_ttl = std::env::var("NOVA_AI_CACHE_TTL_SECS")
    .ok()
    .and_then(|s| s.parse::<u64>().ok())
    .map(std::time::Duration::from_secs)
    .unwrap_or_else(|| std::time::Duration::from_secs(300));

  let timeout = std::env::var("NOVA_AI_TIMEOUT_SECS")
    .ok()
    .and_then(|s| s.parse::<u64>().ok())
    .map(std::time::Duration::from_secs)
    .unwrap_or_else(|| std::time::Duration::from_secs(30));
  // Privacy defaults for legacy env-var based AI wiring:
  // - `include_file_paths` stays disabled unless explicitly opted in.
  // - `anonymize_identifiers` defaults to match `nova.toml` semantics:
  //   - local-only mode: `false` (patch-based edits rely on exact identifiers/positions)
  //   - cloud mode: `true` (privacy-first when sending code to third-party providers)
  //
  // Supported env vars (legacy env-var based AI wiring):
  // - `NOVA_AI_MAX_TOKENS=<n>` overrides `ai.provider.max_tokens` (values are clamped to >= 1).
  // - `NOVA_AI_TEMPERATURE=<f>` overrides `ai.provider.temperature` (must be >= 0).
  // - `NOVA_AI_CONCURRENCY=<n>` overrides `ai.provider.concurrency` (values are clamped to >= 1).
  // - `NOVA_AI_RETRY_MAX_RETRIES=<n>` overrides `ai.provider.retry_max_retries`.
  // - `NOVA_AI_RETRY_INITIAL_BACKOFF_MS=<n>` overrides `ai.provider.retry_initial_backoff_ms`
  //   (values are clamped to >= 1).
  // - `NOVA_AI_RETRY_MAX_BACKOFF_MS=<n>` overrides `ai.provider.retry_max_backoff_ms`
  //   (values are clamped to >= 1).
  // - `NOVA_AI_ANONYMIZE_IDENTIFIERS=0|false|FALSE` disables identifier anonymization.
  //   When unset, defaults depend on the resolved provider/local-only mode (see above).
  // - `NOVA_AI_INCLUDE_FILE_PATHS=1|true|TRUE` allows including paths in prompts
  //   (default: disabled).
  // - `NOVA_AI_EXCLUDED_PATHS` configures `ai.privacy.excluded_paths` as a list of glob patterns.
  //   Parsing: comma- or newline-separated; whitespace trimmed; empty entries ignored; patterns are
  //   preserved verbatim (no path separator normalization).
  //
  // Code-editing (patch/workspace-edit) opt-ins:
  // - `NOVA_AI_LOCAL_ONLY=1|true|TRUE` forces `ai.privacy.local_only=true` regardless of
  //   provider kind (default: unset).
  // - `NOVA_AI_ALLOW_CLOUD_CODE_EDITS=1|true|TRUE` maps to
  //   `ai.privacy.allow_cloud_code_edits` (default: false).
  // - `NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION=1|true|TRUE` maps to
  //   `ai.privacy.allow_code_edits_without_anonymization` (default: false).
  //
  // Optional redaction overrides (mirror `ai.privacy.*` config knobs):
  // - `NOVA_AI_REDACT_SENSITIVE_STRINGS=0|1|false|true|FALSE|TRUE`
  // - `NOVA_AI_REDACT_NUMERIC_LITERALS=0|1|false|true|FALSE|TRUE`
  // - `NOVA_AI_STRIP_OR_REDACT_COMMENTS=0|1|false|true|FALSE|TRUE`
  let force_local_only = matches!(
    std::env::var("NOVA_AI_LOCAL_ONLY").as_deref(),
    Ok("1") | Ok("true") | Ok("TRUE")
  );
  let anonymize_identifiers = std::env::var("NOVA_AI_ANONYMIZE_IDENTIFIERS")
    .ok()
    .map(|value| !matches!(value.as_str(), "0" | "false" | "FALSE"));
  let include_file_paths = matches!(
    std::env::var("NOVA_AI_INCLUDE_FILE_PATHS").as_deref(),
    Ok("1") | Ok("true") | Ok("TRUE")
  );
  let allow_cloud_code_edits = matches!(
    std::env::var("NOVA_AI_ALLOW_CLOUD_CODE_EDITS").as_deref(),
    Ok("1") | Ok("true") | Ok("TRUE")
  );
  let allow_code_edits_without_anonymization = matches!(
    std::env::var("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION").as_deref(),
    Ok("1") | Ok("true") | Ok("TRUE")
  );
  let optional_bool = |key: &str| match std::env::var(key).as_deref() {
    Ok("1") | Ok("true") | Ok("TRUE") => Some(true),
    Ok("0") | Ok("false") | Ok("FALSE") => Some(false),
    _ => None,
  };
  let redact_sensitive_strings = optional_bool("NOVA_AI_REDACT_SENSITIVE_STRINGS");
  let redact_numeric_literals = optional_bool("NOVA_AI_REDACT_NUMERIC_LITERALS");
  let strip_or_redact_comments = optional_bool("NOVA_AI_STRIP_OR_REDACT_COMMENTS");
  let excluded_paths = std::env::var("NOVA_AI_EXCLUDED_PATHS")
    .ok()
    .map(|value| parse_excluded_paths_env_var(&value))
    .unwrap_or_default();

  let mut cfg = nova_config::AiConfig::default();
  cfg.enabled = true;
  cfg.api_key = api_key;
  cfg.audit_log.enabled = audit_logging;
  cfg.cache_enabled = cache_enabled;
  cfg.cache_max_entries = cache_max_entries;
  cfg.cache_ttl_secs = cache_ttl.as_secs().max(1);
  cfg.provider.model = model;
  if let Some(value) = provider_max_tokens {
    cfg.provider.max_tokens = value;
  }
  cfg.provider.temperature = provider_temperature;
  if let Some(value) = provider_retry_max_retries {
    cfg.provider.retry_max_retries = value;
  }
  if let Some(value) = provider_retry_initial_backoff_ms {
    cfg.provider.retry_initial_backoff_ms = value;
  }
  if let Some(value) = provider_retry_max_backoff_ms {
    cfg.provider.retry_max_backoff_ms = value;
  }
  cfg.provider.timeout_ms = timeout.as_millis().min(u64::MAX as u128) as u64;
  cfg.provider.concurrency = provider_concurrency;
  cfg.privacy.include_file_paths = include_file_paths;
  cfg.privacy.redact_sensitive_strings = redact_sensitive_strings;
  cfg.privacy.redact_numeric_literals = redact_numeric_literals;
  cfg.privacy.strip_or_redact_comments = strip_or_redact_comments;
  cfg.privacy.excluded_paths = excluded_paths;
  cfg.privacy.allow_cloud_code_edits = allow_cloud_code_edits;
  cfg.privacy.allow_code_edits_without_anonymization = allow_code_edits_without_anonymization;

  cfg.provider.kind = match provider.as_str() {
    "ollama" => {
      cfg.privacy.local_only = true;
      nova_config::AiProviderKind::Ollama
    }
    "openai_compatible" => {
      cfg.privacy.local_only = true;
      nova_config::AiProviderKind::OpenAiCompatible
    }
    "http" => {
      // Treat the legacy env-var based HTTP provider as local-only by default so code-editing
      // actions (Generate tests/method bodies) are available without additional opt-ins.
      //
      // Cloud-mode privacy policy (anonymization + explicit code-edit opt-ins) is still
      // enforced when using `nova.toml` configuration.
      cfg.privacy.local_only = true;
      nova_config::AiProviderKind::Http
    }
    "openai" => {
      cfg.privacy.local_only = false;
      nova_config::AiProviderKind::OpenAi
    }
    "anthropic" => {
      cfg.privacy.local_only = false;
      nova_config::AiProviderKind::Anthropic
    }
    "gemini" => {
      cfg.privacy.local_only = false;
      nova_config::AiProviderKind::Gemini
    }
    "azure" => {
      cfg.privacy.local_only = false;
      nova_config::AiProviderKind::AzureOpenAi
    }
    other => return Err(format!("unknown NOVA_AI_PROVIDER: {other}")),
  };
  if force_local_only {
    cfg.privacy.local_only = true;
  }

  // If anonymization isn't explicitly configured, default based on the resolved local-only mode.
  // This matches `nova.toml` defaults and keeps patch-based code edits reliable in local-only
  // mode (anonymizing code fences can break patch application).
  cfg.privacy.anonymize_identifiers =
    Some(anonymize_identifiers.unwrap_or(!cfg.privacy.local_only));

  cfg.provider.url = match provider.as_str() {
    "http" => {
      let endpoint = std::env::var("NOVA_AI_ENDPOINT")
        .map_err(|_| "NOVA_AI_ENDPOINT is required for http provider".to_string())?;
      url::Url::parse(&endpoint).map_err(|e| e.to_string())?
    }
    "ollama" => url::Url::parse(
      &std::env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| "http://localhost:11434".to_string()),
    )
    .map_err(|e| e.to_string())?,
    "openai_compatible" => {
      let endpoint = std::env::var("NOVA_AI_ENDPOINT").map_err(|_| {
        "NOVA_AI_ENDPOINT is required for openai_compatible provider".to_string()
      })?;
      url::Url::parse(&endpoint).map_err(|e| e.to_string())?
    }
    "openai" => url::Url::parse(
      &std::env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| "https://api.openai.com/".to_string()),
    )
    .map_err(|e| e.to_string())?,
    "anthropic" => url::Url::parse(
      &std::env::var("NOVA_AI_ENDPOINT")
        .unwrap_or_else(|_| "https://api.anthropic.com/".to_string()),
    )
    .map_err(|e| e.to_string())?,
    "gemini" => url::Url::parse(
      &std::env::var("NOVA_AI_ENDPOINT")
        .unwrap_or_else(|_| "https://generativelanguage.googleapis.com/".to_string()),
    )
    .map_err(|e| e.to_string())?,
    "azure" => {
      let endpoint = std::env::var("NOVA_AI_ENDPOINT")
        .map_err(|_| "NOVA_AI_ENDPOINT is required for azure provider".to_string())?;
      url::Url::parse(&endpoint).map_err(|e| e.to_string())?
    }
    _ => cfg.provider.url.clone(),
  };

  if provider == "azure" {
    cfg.provider.azure_deployment =
      Some(std::env::var("NOVA_AI_AZURE_DEPLOYMENT").map_err(|_| {
        "NOVA_AI_AZURE_DEPLOYMENT is required for azure provider".to_string()
      })?);
    cfg.provider.azure_api_version = Some(
      std::env::var("NOVA_AI_AZURE_API_VERSION").unwrap_or_else(|_| "2024-02-01".to_string()),
    );
  }

  let mut privacy = nova_ai::PrivacyMode::from_ai_privacy_config(&cfg.privacy);
  privacy.include_file_paths = include_file_paths;

  Ok(Some((cfg, privacy)))
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::NovaAi;
  use crate::test_support::{EnvVarGuard, ENV_LOCK};
  use nova_memory::MemoryBudgetOverrides;
  use lsp_types::{
    CompletionList, CompletionParams, Position, TextDocumentPositionParams, Uri,
  };
  use nova_ide::{
    CODE_ACTION_KIND_AI_GENERATE, CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN,
  };
  use serde_json::json;
  use std::path::PathBuf;
  use std::sync::{Arc, RwLock};
  use std::time::Duration;
  use tempfile::TempDir;
  use tokio_util::sync::CancellationToken;

  use httpmock::prelude::*;

  #[test]
  fn load_ai_config_from_env_exposes_privacy_opt_ins() {
    let _lock = ENV_LOCK.lock().unwrap();

    let _provider = EnvVarGuard::set("NOVA_AI_PROVIDER", "http");
    let _endpoint = EnvVarGuard::set("NOVA_AI_ENDPOINT", "http://localhost:1234/complete");
    let _model = EnvVarGuard::set("NOVA_AI_MODEL", "default");
    let _max_tokens = EnvVarGuard::remove("NOVA_AI_MAX_TOKENS");
    let _concurrency = EnvVarGuard::remove("NOVA_AI_CONCURRENCY");

    // Baseline: no explicit code-edit opt-ins.
    let _local_only = EnvVarGuard::remove("NOVA_AI_LOCAL_ONLY");
    let _allow_cloud_code_edits = EnvVarGuard::remove("NOVA_AI_ALLOW_CLOUD_CODE_EDITS");
    let _allow_code_edits_without_anonymization =
      EnvVarGuard::remove("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION");
    let _anonymize = EnvVarGuard::remove("NOVA_AI_ANONYMIZE_IDENTIFIERS");
    let _include_file_paths = EnvVarGuard::remove("NOVA_AI_INCLUDE_FILE_PATHS");
    let _excluded_paths = EnvVarGuard::remove("NOVA_AI_EXCLUDED_PATHS");

    let _redact_sensitive_strings = EnvVarGuard::remove("NOVA_AI_REDACT_SENSITIVE_STRINGS");
    let _redact_numeric_literals = EnvVarGuard::remove("NOVA_AI_REDACT_NUMERIC_LITERALS");
    let _strip_or_redact_comments = EnvVarGuard::remove("NOVA_AI_STRIP_OR_REDACT_COMMENTS");

    let (cfg, privacy) = load_ai_config_from_env()
      .expect("load_ai_config_from_env")
      .expect("config should be present");
    assert_eq!(cfg.privacy.local_only, true);
    assert_eq!(cfg.privacy.anonymize_identifiers, Some(false));
    assert!(!cfg.privacy.allow_cloud_code_edits);
    assert!(!cfg.privacy.allow_code_edits_without_anonymization);
    assert_eq!(cfg.privacy.redact_sensitive_strings, None);
    assert_eq!(cfg.privacy.redact_numeric_literals, None);
    assert_eq!(cfg.privacy.strip_or_redact_comments, None);
    assert!(cfg.privacy.excluded_paths.is_empty());
    assert!(!privacy.include_file_paths);

    // `NOVA_AI_EXCLUDED_PATHS` supports comma-separated values.
    {
      let _excluded_paths = EnvVarGuard::set("NOVA_AI_EXCLUDED_PATHS", "src/secrets/**,gen/**");
      let (cfg, _privacy) = load_ai_config_from_env()
        .expect("load_ai_config_from_env")
        .expect("config should be present");
      assert_eq!(
        cfg.privacy.excluded_paths,
        vec!["src/secrets/**".to_string(), "gen/**".to_string()]
      );
    }

    // `NOVA_AI_EXCLUDED_PATHS` supports newline-separated values.
    {
      let _excluded_paths = EnvVarGuard::set("NOVA_AI_EXCLUDED_PATHS", "src/secrets/**\ngen/**");
      let (cfg, _privacy) = load_ai_config_from_env()
        .expect("load_ai_config_from_env")
        .expect("config should be present");
      assert_eq!(
        cfg.privacy.excluded_paths,
        vec!["src/secrets/**".to_string(), "gen/**".to_string()]
      );
    }

    // Empty/whitespace-only inputs yield an empty list.
    {
      let _excluded_paths = EnvVarGuard::set("NOVA_AI_EXCLUDED_PATHS", "  \n\n,  ,\n");
      let (cfg, _privacy) = load_ai_config_from_env()
        .expect("load_ai_config_from_env")
        .expect("config should be present");
      assert!(cfg.privacy.excluded_paths.is_empty());
    }

    // Explicit opt-in for patch-based code edits (cloud-mode gating).
    {
      let _anonymize = EnvVarGuard::set("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0");
      let _allow_cloud_code_edits = EnvVarGuard::set("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", "1");
      let _allow_code_edits_without_anonymization =
        EnvVarGuard::set("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION", "true");
      let _redact_sensitive_strings = EnvVarGuard::set("NOVA_AI_REDACT_SENSITIVE_STRINGS", "0");
      let _redact_numeric_literals = EnvVarGuard::set("NOVA_AI_REDACT_NUMERIC_LITERALS", "false");
      let _strip_or_redact_comments =
        EnvVarGuard::set("NOVA_AI_STRIP_OR_REDACT_COMMENTS", "1");

      let (cfg, privacy) = load_ai_config_from_env()
        .expect("load_ai_config_from_env")
        .expect("config should be present");
      assert_eq!(cfg.privacy.local_only, true);
      assert_eq!(cfg.privacy.anonymize_identifiers, Some(false));
      assert!(cfg.privacy.allow_cloud_code_edits);
      assert!(cfg.privacy.allow_code_edits_without_anonymization);
      assert_eq!(cfg.privacy.redact_sensitive_strings, Some(false));
      assert_eq!(cfg.privacy.redact_numeric_literals, Some(false));
      assert_eq!(cfg.privacy.strip_or_redact_comments, Some(true));
      assert!(!privacy.include_file_paths);
    }

    // `NOVA_AI_INCLUDE_FILE_PATHS` explicitly opts into including paths in prompts.
    {
      let _include_file_paths = EnvVarGuard::set("NOVA_AI_INCLUDE_FILE_PATHS", "1");
      let (_cfg, privacy) = load_ai_config_from_env()
        .expect("load_ai_config_from_env")
        .expect("config should be present");
      assert!(privacy.include_file_paths);
    }

    // `NOVA_AI_LOCAL_ONLY` forces local-only mode regardless of provider.
    {
      let _force_local_only = EnvVarGuard::set("NOVA_AI_LOCAL_ONLY", "1");
      let (cfg, _privacy) = load_ai_config_from_env()
        .expect("load_ai_config_from_env")
        .expect("config should be present");
      assert_eq!(cfg.privacy.local_only, true);
    }
  }

  #[test]
  fn load_ai_config_from_env_reads_provider_max_tokens() {
    let _lock = ENV_LOCK.lock().unwrap();

    let _provider = EnvVarGuard::set("NOVA_AI_PROVIDER", "http");
    let _endpoint = EnvVarGuard::set("NOVA_AI_ENDPOINT", "http://localhost:1234/complete");
    let _model = EnvVarGuard::set("NOVA_AI_MODEL", "default");
    let _concurrency = EnvVarGuard::remove("NOVA_AI_CONCURRENCY");
    let _max_tokens = EnvVarGuard::set("NOVA_AI_MAX_TOKENS", "2048");

    let (cfg, _privacy) = load_ai_config_from_env()
      .expect("load_ai_config_from_env")
      .expect("config should be present");
    assert_eq!(cfg.provider.max_tokens, 2048);
  }

  #[test]
  fn load_ai_config_from_env_reads_provider_concurrency() {
    let _lock = ENV_LOCK.lock().unwrap();

    let _provider = EnvVarGuard::set("NOVA_AI_PROVIDER", "http");
    let _endpoint = EnvVarGuard::set("NOVA_AI_ENDPOINT", "http://localhost:1234/complete");
    let _model = EnvVarGuard::set("NOVA_AI_MODEL", "default");
    let _max_tokens = EnvVarGuard::remove("NOVA_AI_MAX_TOKENS");
    let _concurrency = EnvVarGuard::set("NOVA_AI_CONCURRENCY", "3");

    let (cfg, _privacy) = load_ai_config_from_env()
      .expect("load_ai_config_from_env")
      .expect("config should be present");
    assert_eq!(cfg.provider.concurrency, Some(3));
  }

  #[test]
  fn load_ai_config_from_env_defaults_anonymization_based_on_provider_kind() {
    let _lock = ENV_LOCK.lock().unwrap();

    let _provider = EnvVarGuard::set("NOVA_AI_PROVIDER", "openai");
    let _model = EnvVarGuard::set("NOVA_AI_MODEL", "default");

    // Explicitly clear any overrides so we can validate the default behavior.
    let _local_only = EnvVarGuard::remove("NOVA_AI_LOCAL_ONLY");
    let _anonymize = EnvVarGuard::remove("NOVA_AI_ANONYMIZE_IDENTIFIERS");

    let (cfg, _privacy) = load_ai_config_from_env()
      .expect("load_ai_config_from_env")
      .expect("config should be present");

    assert_eq!(cfg.privacy.local_only, false);
    assert_eq!(cfg.privacy.anonymize_identifiers, Some(true));

    // `NOVA_AI_LOCAL_ONLY` forces local-only mode (and therefore flips the default anonymization
    // behavior back to `false` unless explicitly overridden).
    {
      let _force_local_only = EnvVarGuard::set("NOVA_AI_LOCAL_ONLY", "1");
      let (cfg, _privacy) = load_ai_config_from_env()
        .expect("load_ai_config_from_env")
        .expect("config should be present");
      assert_eq!(cfg.privacy.local_only, true);
      assert_eq!(cfg.privacy.anonymize_identifiers, Some(false));
    }
  }

  #[test]
  fn run_ai_explain_error_emits_chunked_log_messages_and_progress() {
    let server = MockServer::start();
    let long = "Nova AI output ".repeat((AI_LOG_MESSAGE_CHUNK_BYTES * 2) / 14 + 32);
    let mock = server.mock(|when, then| {
      when.method(POST).path("/complete");
      then.status(200).json_body(json!({ "completion": long }));
    });

    let mut cfg = nova_config::AiConfig::default();
    cfg.enabled = true;
    cfg.provider.kind = nova_config::AiProviderKind::Http;
    cfg.provider.url = url::Url::parse(&format!("{}/complete", server.base_url())).unwrap();
    cfg.provider.model = "default".to_string();
    cfg.provider.timeout_ms = Duration::from_secs(2).as_millis() as u64;
    cfg.provider.concurrency = Some(1);
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.cache_enabled = false;

    let ai = NovaAi::new(&cfg).unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap();

    let mut state =
      ServerState::new(nova_config::NovaConfig::default(), None, MemoryBudgetOverrides::default());
    state.ai = Some(ai);
    state.runtime = Some(runtime);

    let work_done_token = Some(json!("token"));
    let args = ExplainErrorArgs {
      diagnostic_message: "cannot find symbol".to_string(),
      code: Some("class Foo {}".to_string()),
      uri: None,
      range: None,
    };

    let client = crate::rpc_out::WriteRpcOut::new(Vec::<u8>::new());
    let result = run_ai_explain_error(
      args,
      work_done_token,
      &mut state,
      &client,
      CancellationToken::new(),
    )
    .unwrap();
    let expected = result.as_str().expect("string result");

    let bytes = client.into_inner();
    let mut reader = std::io::BufReader::new(bytes.as_slice());
    let mut messages = Vec::new();
    loop {
      match crate::codec::read_json_message(&mut reader) {
        Ok(value) => messages.push(value),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
        Err(err) => panic!("failed to read JSON-RPC message: {err}"),
      }
    }

    assert!(
      messages.iter().any(|msg| {
        msg.get("method").and_then(|m| m.as_str()) == Some("$/progress")
          && msg
            .get("params")
            .and_then(|p| p.get("value"))
            .and_then(|v| v.get("kind"))
            .and_then(|k| k.as_str())
            == Some("begin")
      }),
      "expected a work-done progress begin notification"
    );

    assert!(
      messages.iter().any(|msg| {
        msg.get("method").and_then(|m| m.as_str()) == Some("$/progress")
          && msg
            .get("params")
            .and_then(|p| p.get("value"))
            .and_then(|v| v.get("kind"))
            .and_then(|k| k.as_str())
            == Some("end")
      }),
      "expected a work-done progress end notification"
    );

    let mut output_chunks = Vec::new();
    for msg in &messages {
      if msg.get("method").and_then(|m| m.as_str()) != Some("window/logMessage") {
        continue;
      }
      let Some(text) = msg
        .get("params")
        .and_then(|p| p.get("message"))
        .and_then(|m| m.as_str())
      else {
        continue;
      };
      if !text.starts_with("AI explainError") {
        continue;
      }
      let (_, chunk) = text
        .split_once(": ")
        .expect("chunk messages should contain ': ' delimiter");
      output_chunks.push(chunk.to_string());
    }

    assert!(
      output_chunks.len() > 1,
      "expected output to be chunked into multiple logMessage notifications"
    );
    assert_eq!(output_chunks.join(""), expected);

    mock.assert();
  }

  #[test]
  fn excluded_paths_disable_ai_completions_and_code_edit_actions() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let src_dir = root.join("src");
    let secrets_dir = src_dir.join("secrets");
    std::fs::create_dir_all(&secrets_dir).expect("create src/secrets dir");

    let secret_path = secrets_dir.join("Secret.java");
    let secret_text = "class Secret { void foo() {} }";
    std::fs::write(&secret_path, secret_text).expect("write Secret.java");

    let main_path = src_dir.join("Main.java");
    let main_text = "class Main { void foo() {} }";
    std::fs::write(&main_path, main_text).expect("write Main.java");

    let secret_uri: Uri = url::Url::from_file_path(&secret_path)
      .expect("file url")
      .to_string()
      .parse()
      .expect("uri");
    let main_uri: Uri = url::Url::from_file_path(&main_path)
      .expect("file url")
      .to_string()
      .parse()
      .expect("uri");

    let mut cfg = nova_config::NovaConfig::default();
    cfg.ai.enabled = true;
    cfg.ai.features.multi_token_completion = true;
    cfg.ai.privacy.excluded_paths = vec!["src/secrets/**".to_string()];

    let mut state = ServerState::new(cfg, None, MemoryBudgetOverrides::default());
    state.project_root = Some(root.to_path_buf());
    state
      .analysis
      .open_document(secret_uri.clone(), secret_text.to_string(), 1);
    state
      .analysis
      .open_document(main_uri.clone(), main_text.to_string(), 1);

    // Multi-token completion must not run for excluded paths (no async follow-up completions).
    let completion_params = CompletionParams {
      text_document_position: TextDocumentPositionParams {
        text_document: lsp_types::TextDocumentIdentifier {
          uri: secret_uri.clone(),
        },
        position: Position::new(0, 0),
      },
      work_done_progress_params: Default::default(),
      partial_result_params: Default::default(),
      context: None,
    };
    let completion_list: CompletionList = serde_json::from_value(
      crate::stdio_completion::handle_completion(
        serde_json::to_value(completion_params).expect("completion params"),
        &state,
        CancellationToken::new(),
      )
      .expect("completion response"),
    )
    .expect("completion list");
    assert!(
      !completion_list.is_incomplete,
      "expected no AI completion session for excluded file"
    );

    let excluded_actions = crate::stdio_code_action::handle_code_action(
      json!({
        "textDocument": { "uri": secret_uri.to_string() },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
        "context": {
          "diagnostics": [
            { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "message": "boom" }
          ]
        }
      }),
      &mut state,
      CancellationToken::new(),
    )
    .expect("code action response");
    let excluded_actions = excluded_actions.as_array().expect("array");

    let explain = excluded_actions
      .iter()
      .find(|action| action.get("kind").and_then(|k| k.as_str()) == Some(CODE_ACTION_KIND_EXPLAIN))
      .expect("expected explain action for excluded file");
    let explain_code = explain
      .get("command")
      .and_then(|cmd| cmd.get("arguments"))
      .and_then(|args| args.get(0))
      .and_then(|arg0| arg0.get("code"))
      .expect("expected ExplainErrorArgs.code field");
    assert!(
      explain_code.is_null(),
      "expected explain action to omit code snippet for excluded file; got: {explain_code:?}"
    );
    assert!(
      excluded_actions.iter().all(|action| {
        !action
          .get("kind")
          .and_then(|kind| kind.as_str())
          .is_some_and(|kind| kind == CODE_ACTION_KIND_AI_GENERATE || kind == CODE_ACTION_KIND_AI_TESTS)
      }),
      "expected no AI code-edit actions for excluded file"
    );

    let allowed_actions = crate::stdio_code_action::handle_code_action(
      json!({
        "textDocument": { "uri": main_uri.to_string() },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
        "context": {
          "diagnostics": [
            { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "message": "boom" }
          ]
        }
      }),
      &mut state,
      CancellationToken::new(),
    )
    .expect("code action response");
    let allowed_actions = allowed_actions.as_array().expect("array");
    let explain = allowed_actions
      .iter()
      .find(|action| action.get("kind").and_then(|k| k.as_str()) == Some(CODE_ACTION_KIND_EXPLAIN))
      .expect("expected explain action for non-excluded file when AI is configured");
    let explain_code = explain
      .get("command")
      .and_then(|cmd| cmd.get("arguments"))
      .and_then(|args| args.get(0))
      .and_then(|arg0| arg0.get("code"))
      .expect("expected ExplainErrorArgs.code field");
    assert!(
      explain_code.is_string(),
      "expected explain action to include code snippet for non-excluded file; got: {explain_code:?}"
    );
  }

  fn explain_error_request_omits_excluded_code(req: &HttpMockRequest) -> bool {
    let Some(body) = req.body.as_deref() else {
      return false;
    };
    let body = String::from_utf8_lossy(body);
    body.contains("boom") && !body.contains("DO_NOT_LEAK_THIS_SECRET")
  }

  #[test]
  fn excluded_paths_strip_ai_explain_error_file_context() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let secrets_dir = root.join("src").join("secrets");
    std::fs::create_dir_all(&secrets_dir).expect("create src/secrets dir");

    let secret_marker = "DO_NOT_LEAK_THIS_SECRET";
    let secret_path = secrets_dir.join("Secret.java");
    let secret_text = format!(r#"class Secret {{ String v = "{secret_marker}"; }}"#);
    std::fs::write(&secret_path, &secret_text).expect("write Secret.java");
    let secret_uri: Uri = url::Url::from_file_path(&secret_path)
      .expect("file url")
      .to_string()
      .parse()
      .expect("uri");

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
      when
        .method(POST)
        .path("/complete")
        .matches(explain_error_request_omits_excluded_code);
      then
        .status(200)
        .json_body(json!({ "completion": "mock explanation" }));
    });

    let mut cfg = nova_config::NovaConfig::default();
    cfg.ai.enabled = true;
    cfg.ai.provider.kind = nova_config::AiProviderKind::Http;
    cfg.ai.provider.url = url::Url::parse(&format!("{}/complete", server.base_url())).unwrap();
    cfg.ai.provider.model = "default".to_string();
    cfg.ai.provider.timeout_ms = Duration::from_secs(2).as_millis() as u64;
    cfg.ai.provider.concurrency = Some(1);
    cfg.ai.privacy.local_only = false;
    cfg.ai.privacy.anonymize_identifiers = Some(false);
    cfg.ai.privacy.excluded_paths = vec!["src/secrets/**".to_string()];
    cfg.ai.cache_enabled = false;

    let mut state = ServerState::new(cfg, None, MemoryBudgetOverrides::default());
    state.project_root = Some(root.to_path_buf());
    state
      .analysis
      .open_document(secret_uri.clone(), secret_text.clone(), 1);

    let out = crate::rpc_out::WriteRpcOut::new(Vec::<u8>::new());
    run_ai_explain_error(
      ExplainErrorArgs {
        diagnostic_message: "boom".to_string(),
        // Even if a client supplies code, excluded_paths is enforced server-side.
        code: Some(secret_text.clone()),
        uri: Some(secret_uri.to_string()),
        range: Some(nova_ide::LspRange {
          start: nova_ide::LspPosition {
            line: 0,
            character: 0,
          },
          end: nova_ide::LspPosition {
            line: 0,
            character: 10,
          },
        }),
      },
      None,
      &mut state,
      &out,
      CancellationToken::new(),
    )
    .expect("explainError should be allowed for excluded paths (without file-backed context)");

    mock.assert_hits(1);
  }

  #[test]
  fn excluded_paths_explain_error_placeholder_survives_anonymization() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let secrets_dir = root.join("src").join("secrets");
    std::fs::create_dir_all(&secrets_dir).expect("create src/secrets dir");

    let secret_marker = "DO_NOT_LEAK_THIS_SECRET";
    let secret_path = secrets_dir.join("Secret.java");
    let secret_text = format!(r#"class Secret {{ String v = "{secret_marker}"; }}"#);
    std::fs::write(&secret_path, &secret_text).expect("write Secret.java");
    let secret_uri: Uri = url::Url::from_file_path(&secret_path)
      .expect("file url")
      .to_string()
      .parse()
      .expect("uri");

    let server = MockServer::start();
    let captured_prompt = Arc::new(RwLock::new(None::<String>));
    let mock = server.mock(|when, then| {
      let captured_prompt = captured_prompt.clone();
      when.method(POST).path("/complete").matches(move |req| {
        if let Some(body) = req.body.as_deref() {
          if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
            if let Some(prompt) = value.get("prompt").and_then(|p| p.as_str()) {
              *captured_prompt.write().unwrap() = Some(prompt.to_string());
            }
          }
        }
        true
      });
      then
        .status(200)
        .json_body(json!({ "completion": "mock explanation" }));
    });

    let mut cfg = nova_config::NovaConfig::default();
    cfg.ai.enabled = true;
    cfg.ai.provider.kind = nova_config::AiProviderKind::Http;
    cfg.ai.provider.url = url::Url::parse(&format!("{}/complete", server.base_url())).unwrap();
    cfg.ai.provider.model = "default".to_string();
    cfg.ai.provider.timeout_ms = Duration::from_secs(2).as_millis() as u64;
    cfg.ai.provider.concurrency = Some(1);
    cfg.ai.privacy.local_only = false;
    cfg.ai.privacy.anonymize_identifiers = Some(true);
    cfg.ai.privacy.excluded_paths = vec!["src/secrets/**".to_string()];
    cfg.ai.cache_enabled = false;

    let mut state = ServerState::new(cfg, None, MemoryBudgetOverrides::default());
    state.project_root = Some(root.to_path_buf());
    state
      .analysis
      .open_document(secret_uri.clone(), secret_text.clone(), 1);

    let out = crate::rpc_out::WriteRpcOut::new(Vec::<u8>::new());
    run_ai_explain_error(
      ExplainErrorArgs {
        diagnostic_message: "boom".to_string(),
        // Even if a client supplies code, excluded_paths is enforced server-side.
        code: Some(secret_text.clone()),
        uri: Some(secret_uri.to_string()),
        range: Some(nova_ide::LspRange {
          start: nova_ide::LspPosition {
            line: 0,
            character: 0,
          },
          end: nova_ide::LspPosition {
            line: 0,
            character: 10,
          },
        }),
      },
      None,
      &mut state,
      &out,
      CancellationToken::new(),
    )
    .expect("explainError should be allowed for excluded paths (without file-backed context)");

    mock.assert_hits(1);
    let prompt = captured_prompt
      .read()
      .unwrap()
      .clone()
      .expect("expected to capture prompt sent to HTTP AI provider");
    assert!(
      prompt.contains(r#""[code context omitted due to excluded_paths]""#),
      "expected excluded_paths omission marker to remain readable under anonymization; prompt:\n{prompt}"
    );
  }

  #[test]
  fn semantic_search_related_code_filters_excluded_paths() {
    #[derive(Clone)]
    struct StaticSemanticSearch {
      results: Vec<nova_ai::SearchResult>,
    }

    impl nova_ai::SemanticSearch for StaticSemanticSearch {
      fn search(&self, _query: &str) -> Vec<nova_ai::SearchResult> {
        self.results.clone()
      }
    }

    let mut cfg = nova_config::NovaConfig::default();
    cfg.ai.enabled = true;
    cfg.ai.features.semantic_search = true;
    cfg.ai.privacy.excluded_paths = vec!["src/secrets/**".to_string()];

    let mut state = ServerState::new(cfg, None, MemoryBudgetOverrides::default());
    state.semantic_search = Arc::new(RwLock::new(Box::new(StaticSemanticSearch {
      results: vec![
        nova_ai::SearchResult {
          path: PathBuf::from("src/secrets/Secret.java"),
          range: 0..0,
          kind: "file".to_string(),
          score: 1.0,
          snippet: "DO_NOT_LEAK".to_string(),
        },
        nova_ai::SearchResult {
          path: PathBuf::from("src/Main.java"),
          range: 0..0,
          kind: "file".to_string(),
          score: 0.5,
          snippet: "class Main {}".to_string(),
        },
      ],
    }) as Box<dyn nova_ai::SemanticSearch>));

    let req = build_context_request(&state, "class Main {}".to_string(), None);
    let enriched = maybe_add_related_code(&state, req);
    assert_eq!(enriched.related_code.len(), 1);
    assert_eq!(enriched.related_code[0].path, PathBuf::from("src/Main.java"));
  }

  #[test]
  fn build_context_request_attaches_project_and_semantic_context_when_available() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let file_path = src_dir.join("Main.java");
    let text = r#"class Main { void run() { String s = "hi"; } }"#;
    std::fs::write(&file_path, text).expect("write java file");

    let uri: Uri = url::Url::from_file_path(&file_path)
      .expect("file url")
      .to_string()
      .parse()
      .expect("uri");

    let mut state = ServerState::new(
      nova_config::NovaConfig::default(),
      Some(nova_ai::PrivacyMode::default()),
      MemoryBudgetOverrides::default(),
    );
    state.project_root = Some(root.to_path_buf());
    state.analysis.open_document(uri.clone(), text.to_string(), 1);

    let offset = text.find("s =").expect("variable occurrence");
    let start = nova_lsp::text_pos::lsp_position(text, offset).expect("start pos");
    let end = nova_lsp::text_pos::lsp_position(text, offset + 1).expect("end pos");
    let range = nova_ide::LspRange {
      start: nova_ide::LspPosition {
        line: start.line,
        character: start.character,
      },
      end: nova_ide::LspPosition {
        line: end.line,
        character: end.character,
      },
    };

    let req = build_context_request_from_args(
      &state,
      Some(uri.as_str()),
      Some(range),
      String::new(),
      None,
      /*include_doc_comments=*/ false,
    );

    assert!(
      req.project_context.is_some(),
      "expected project context for a real workspace root"
    );
    assert!(
      req.semantic_context.is_some(),
      "expected hover/type info for identifier at selection"
    );

    let built = nova_ai::ContextBuilder::new().build(req);
    assert!(built.text.contains("Project context"));
    assert!(built.text.contains("Symbol/type info"));
  }

  #[test]
  fn build_context_request_includes_file_path_when_opted_in() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let file_path = src_dir.join("Main.java");
    let text = r#"class Main { void run() { String s = "hi"; } }"#;
    std::fs::write(&file_path, text).expect("write java file");

    let uri: Uri = url::Url::from_file_path(&file_path)
      .expect("file url")
      .to_string()
      .parse()
      .expect("uri");

    let mut privacy = nova_ai::PrivacyMode::default();
    privacy.include_file_paths = true;
    let mut state = ServerState::new(
      nova_config::NovaConfig::default(),
      Some(privacy),
      MemoryBudgetOverrides::default(),
    );
    state.project_root = Some(root.to_path_buf());
    state.analysis.open_document(uri.clone(), text.to_string(), 1);

    let offset = text.find("s =").expect("variable occurrence");
    let start = nova_lsp::text_pos::lsp_position(text, offset).expect("start pos");
    let end = nova_lsp::text_pos::lsp_position(text, offset + 1).expect("end pos");
    let range = nova_ide::LspRange {
      start: nova_ide::LspPosition {
        line: start.line,
        character: start.character,
      },
      end: nova_ide::LspPosition {
        line: end.line,
        character: end.character,
      },
    };

    let req = build_context_request_from_args(
      &state,
      Some(uri.as_str()),
      Some(range),
      String::new(),
      None,
      /*include_doc_comments=*/ false,
    );

    let built = nova_ai::ContextBuilder::new().build(req);
    assert!(built.text.contains("## File"), "built context: {}", built.text);
    assert!(built.text.contains("Main.java"), "built context: {}", built.text);
  }
}
