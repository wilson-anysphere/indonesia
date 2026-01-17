use crate::rpc_out::RpcOut;
use crate::stdio_ai_context::byte_range_for_ide_range;
use crate::stdio_apply_edit::send_workspace_apply_edit;
use crate::stdio_paths::{load_document_text, path_from_uri};
use crate::stdio_progress::{send_progress_begin, send_progress_end, send_progress_report};
use crate::ServerState;

use lsp_types::ProgressToken;
use lsp_types::{Position as LspTypesPosition, Range as LspTypesRange, Uri as LspUri};
use nova_ai_codegen::{
    CodeGenerationConfig, CodegenProgressEvent, CodegenProgressReporter, CodegenProgressStage,
    PromptCompletionError, PromptCompletionProvider,
};
use nova_ide::{GenerateMethodBodyArgs, GenerateTestsArgs};
use nova_lsp::{AiCodeAction, AiCodeActionExecutor, CodeActionOutcome};
use nova_scheduler::CancellationToken;
use std::path::{Path, PathBuf};

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

/// Patch-based AI code-editing helpers (powered by `nova-ai-codegen`).
///
/// The `nova/ai/generateMethodBody` and `nova/ai/generateTests` custom request endpoints apply edits
/// via `workspace/applyEdit` and return JSON `null` on success. When a work-done token is provided,
/// these helpers also emit `$/progress` stage updates.
struct LspCodegenProgress<'a, O: RpcOut + Sync> {
    out: &'a O,
    token: Option<&'a ProgressToken>,
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
    work_done_token: Option<ProgressToken>,
    state: &mut ServerState,
    rpc_out: &O,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    // AI code generation is a code-editing operation. Enforce privacy policy early so clients
    // always see the policy error even if they invoke the command with incomplete arguments.
    nova_ai::enforce_code_edit_policy(&state.ai_config.privacy)
        .map_err(|e| (-32603, e.to_string()))?;

    let uri_string = args
        .uri
        .as_deref()
        .ok_or_else(|| (-32602, "missing uri".to_string()))?;
    let uri = uri_string
        .parse::<LspUri>()
        .map_err(|e| (-32602, format!("invalid uri: {e}")))?;

    let (root_uri, file_rel, abs_path) = resolve_ai_patch_target(&uri, state)?;

    // Enforce excluded_paths *before* building prompts or calling the model.
    if ai.is_excluded_path(&abs_path) {
        return Err((
            -32600,
            "AI disabled for this file due to ai.privacy.excluded_paths".to_string(),
        ));
    }

    let Some(source) =
        load_document_text(state, uri_string).or_else(|| load_document_text(state, uri.as_str()))
    else {
        return Err((
            -32602,
            format!("missing document text for `{}`", uri.as_str()),
        ));
    };

    let selection = args
        .range
        .ok_or_else(|| (-32602, "missing range".to_string()))?;
    let insert_range =
        insert_range_for_method_body(&source, selection).map_err(|message| (-32602, message))?;

    let workspace = nova_ai::workspace::VirtualWorkspace::new([(file_rel.clone(), source)]);

    let llm = ai.llm();
    let provider = LlmPromptCompletionProvider { llm: llm.as_ref() };
    let mut config = CodeGenerationConfig::default();
    config.safety.excluded_path_globs = state.ai_config.privacy.excluded_paths.clone();

    let executor = AiCodeActionExecutor::new(&provider, config, state.ai_config.privacy.clone());

    send_progress_begin(
        rpc_out,
        work_done_token.as_ref(),
        "AI: Generate method body",
    )?;
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
            },
            &workspace,
            &root_uri,
            &cancel,
            progress,
        ))
        .map_err(|err| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, err.to_string())
        })?;

    let _ = apply_code_action_outcome(outcome, "AI: Generate method body", state, rpc_out)
        .map_err(|err| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            err
        })?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    // The `nova/ai/*` patch-based endpoints return `null` on success and apply edits via
    // `workspace/applyEdit`.
    Ok(serde_json::Value::Null)
}

pub(super) fn run_ai_generate_tests_apply<O: RpcOut + Sync>(
    args: GenerateTestsArgs,
    work_done_token: Option<ProgressToken>,
    state: &mut ServerState,
    rpc_out: &O,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    // AI test generation is a code-editing operation. Enforce privacy policy early so clients
    // always see the policy error even if they invoke the command with incomplete arguments.
    nova_ai::enforce_code_edit_policy(&state.ai_config.privacy)
        .map_err(|e| (-32603, e.to_string()))?;

    let GenerateTestsArgs {
        target,
        context,
        uri,
        range,
    } = args;
    let uri_string = uri
        .as_deref()
        .ok_or_else(|| (-32602, "missing uri".to_string()))?;
    let uri = uri_string
        .parse::<LspUri>()
        .map_err(|e| (-32602, format!("invalid uri: {e}")))?;

    let (root_uri, file_rel, abs_path) = resolve_ai_patch_target(&uri, state)?;

    // Enforce excluded_paths *before* building prompts or calling the model.
    if ai.is_excluded_path(&abs_path) {
        return Err((
            -32600,
            "AI disabled for this file due to ai.privacy.excluded_paths".to_string(),
        ));
    }

    let Some(source) =
        load_document_text(state, uri_string).or_else(|| load_document_text(state, uri.as_str()))
    else {
        return Err((
            -32602,
            format!("missing document text for `{}`", uri.as_str()),
        ));
    };

    let selection = range.ok_or_else(|| (-32602, "missing range".to_string()))?;
    // Always validate the incoming selection range (UTF-16 correctness, in-bounds) so we can
    // produce deterministic errors when clients send malformed ranges.
    let selection_range =
        insert_range_from_ide_range(&source, selection).map_err(|message| (-32602, message))?;

    let target = Some(target);
    let source_file = Some(file_rel.clone());
    let source_snippet = byte_range_for_ide_range(&source, selection)
        .and_then(|r| source.get(r).map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty());

    let llm = ai.llm();
    let provider = LlmPromptCompletionProvider { llm: llm.as_ref() };
    let mut config = CodeGenerationConfig::default();
    config.safety.excluded_path_globs = state.ai_config.privacy.excluded_paths.clone();

    let (action_file, insert_range, workspace) = if file_rel.starts_with("src/main/java/") {
        if let Some(test_file) = derive_test_file_path(&source, &abs_path) {
            // `derive_test_file_path` returns a workspace-relative path (e.g. `src/test/java/...`).
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
                            format!(
                                "failed to determine workspace root for `{}`",
                                abs_path.display()
                            ),
                        )
                    })?;
                let test_file_path = root_path.join(&test_file);
                match std::fs::read_to_string(&test_file_path) {
                    Ok(existing) => {
                        workspace.insert(test_file.clone(), existing);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            path = %test_file_path.display(),
                            error = %err,
                            "failed to read existing derived test file; proceeding without it"
                        );
                    }
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
        }
    } else {
        (
            file_rel.clone(),
            selection_range,
            nova_ai::workspace::VirtualWorkspace::new([(file_rel.clone(), source)]),
        )
    };

    let executor = AiCodeActionExecutor::new(&provider, config, state.ai_config.privacy.clone());

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate tests")?;
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
            },
            &workspace,
            &root_uri,
            &cancel,
            progress,
        ))
        .map_err(|err| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, err.to_string())
        })?;

    let _ = apply_code_action_outcome(outcome, "AI: Generate tests", state, rpc_out).map_err(
        |err| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            err
        },
    )?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    // The `nova/ai/*` patch-based endpoints return `null` on success and apply edits via
    // `workspace/applyEdit`.
    Ok(serde_json::Value::Null)
}

fn apply_code_action_outcome<O: RpcOut>(
    outcome: CodeActionOutcome,
    label: &str,
    state: &mut ServerState,
    rpc_out: &O,
) -> Result<serde_json::Value, (i32, String)> {
    match outcome {
        CodeActionOutcome::WorkspaceEdit(edit) => {
            send_workspace_apply_edit(state, rpc_out, label, &edit)?;
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

    let open_rel = selection_text.find('{').ok_or_else(|| {
        "selection does not contain an opening `{` for the method body".to_string()
    })?;
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

    let (root_uri, file_rel) = nova_lsp::patch_paths::patch_root_uri_and_file_rel(
        state.project_root.as_deref(),
        &abs_path,
    )
    .map_err(|err| (-32603, err))?;

    Ok((root_uri, file_rel, abs_path))
}

fn derive_test_file_path(source_text: &str, source_path: &Path) -> Option<String> {
    // Only derive a `src/test/java/...` target when the source file lives under a conventional
    // `src/main/java` tree. For ad-hoc single-file projects (e.g. `Test.java` in the workspace
    // root), prefer inserting tests into the current file selection.
    let in_src_main_java = source_path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .windows(3)
        .any(|window| window == ["src", "main", "java"]);
    if !in_src_main_java {
        return None;
    }

    let class_name = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)?;
    if !is_java_identifier(&class_name) {
        return None;
    }
    let test_class = format!("{class_name}Test");

    let pkg = crate::stdio_code_lens::parse_java_package(source_text);
    let pkg_path = match pkg.as_deref() {
        None => String::new(),
        Some(pkg) => {
            let segments: Vec<_> = pkg.split('.').collect();
            if segments.is_empty() || segments.iter().any(|s| !is_java_identifier(s)) {
                return None;
            }
            segments.join("/")
        }
    };

    let mut out = String::from("src/test/java/");
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
