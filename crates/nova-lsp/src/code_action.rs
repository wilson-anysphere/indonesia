use std::collections::HashMap;
use std::path::Path;

use lsp_types::{Position, Range, TextEdit, Uri, WorkspaceEdit};
use nova_ai::context::{ContextBuilder, ContextRequest};
use nova_ai::workspace::VirtualWorkspace;
use nova_ai::PrivacyMode;
use nova_ai_codegen::{
    generate_patch, CodeGenerationConfig, CodeGenerationError, CodegenProgressEvent,
    CodegenProgressReporter, CodegenProgressStage, PromptCompletionError, PromptCompletionProvider,
};
use nova_config::AiPrivacyConfig;
use nova_core::{LineIndex, Position as CorePosition};
use nova_ide::diagnostics::Diagnostic;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum AiCodeAction {
    ExplainError { diagnostic: Diagnostic },
    GenerateMethodBody { file: String, insert_range: Range },
    GenerateTest { file: String, insert_range: Range },
}

#[derive(Debug, Clone)]
pub enum CodeActionOutcome {
    Explanation(String),
    WorkspaceEdit(WorkspaceEdit),
}

#[derive(Debug, Error)]
pub enum CodeActionError {
    #[error(transparent)]
    Provider(#[from] PromptCompletionError),
    #[error(transparent)]
    Codegen(#[from] CodeGenerationError),
}

pub struct AiCodeActionExecutor<'a> {
    provider: &'a dyn PromptCompletionProvider,
    config: CodeGenerationConfig,
    privacy: AiPrivacyConfig,
}

impl<'a> AiCodeActionExecutor<'a> {
    pub fn new(
        provider: &'a dyn PromptCompletionProvider,
        config: CodeGenerationConfig,
        privacy: AiPrivacyConfig,
    ) -> Self {
        Self {
            provider,
            config,
            privacy,
        }
    }

    pub async fn execute(
        &self,
        action: AiCodeAction,
        workspace: &VirtualWorkspace,
        root_uri: &Uri,
        cancel: &CancellationToken,
        progress: Option<&dyn CodegenProgressReporter>,
    ) -> Result<CodeActionOutcome, CodeActionError> {
        match action {
            AiCodeAction::ExplainError { diagnostic } => {
                let prompt = format!(
                    "Explain this compiler diagnostic:\n\n{:?}\n\nRespond in plain English.",
                    diagnostic
                );
                let explanation = self.provider.complete(&prompt, cancel).await?;
                Ok(CodeActionOutcome::Explanation(explanation))
            }
            AiCodeAction::GenerateMethodBody { file, insert_range } => {
                if let Some(progress) = progress {
                    progress.report(CodegenProgressEvent {
                        stage: CodegenProgressStage::BuildingPrompt,
                        attempt: 0,
                        message: "Building context…".to_string(),
                    });
                }
                let prompt = build_insert_prompt(
                    "Generate a Java method body for the marked range.",
                    &file,
                    insert_range,
                    workspace,
                );

                let mut config = self.config.clone();
                if config.safety.allowed_path_prefixes.is_empty() {
                    config.safety.allowed_path_prefixes = vec![file.clone()];
                }

                let result = generate_patch(
                    self.provider,
                    workspace,
                    &prompt,
                    &config,
                    &self.privacy,
                    cancel,
                    progress,
                )
                .await?;
                let edit = workspace_edit_from_virtual_workspace(
                    root_uri,
                    workspace,
                    &result.formatted_workspace,
                    result.applied.touched_ranges.keys(),
                );
                Ok(CodeActionOutcome::WorkspaceEdit(edit))
            }
            AiCodeAction::GenerateTest { file, insert_range } => {
                if let Some(progress) = progress {
                    progress.report(CodegenProgressEvent {
                        stage: CodegenProgressStage::BuildingPrompt,
                        attempt: 0,
                        message: "Building context…".to_string(),
                    });
                }
                let prompt = build_insert_prompt(
                    "Generate Java unit tests for the marked range.",
                    &file,
                    insert_range,
                    workspace,
                );

                let mut config = self.config.clone();
                if config.safety.allowed_path_prefixes.is_empty() {
                    config.safety.allowed_path_prefixes = vec![file.clone()];
                }

                let result = generate_patch(
                    self.provider,
                    workspace,
                    &prompt,
                    &config,
                    &self.privacy,
                    cancel,
                    progress,
                )
                .await?;
                let edit = workspace_edit_from_virtual_workspace(
                    root_uri,
                    workspace,
                    &result.formatted_workspace,
                    result.applied.touched_ranges.keys(),
                );
                Ok(CodeActionOutcome::WorkspaceEdit(edit))
            }
        }
    }
}

fn workspace_edit_from_virtual_workspace<'a>(
    root_uri: &Uri,
    before: &VirtualWorkspace,
    after: &VirtualWorkspace,
    touched_files: impl IntoIterator<Item = &'a String>,
) -> WorkspaceEdit {
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for file in touched_files {
        let before_text = before.get(file).unwrap_or("");
        let after_text = after.get(file).unwrap_or("");
        if before_text == after_text {
            continue;
        }
        let uri = crate::workspace_edit::join_uri(root_uri, Path::new(file));
        changes.insert(
            uri,
            vec![TextEdit {
                range: crate::workspace_edit::full_document_range(before_text),
                new_text: after_text.to_string(),
            }],
        );
    }
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

fn build_insert_prompt(
    header: &str,
    file: &str,
    insert_range: Range,
    workspace: &VirtualWorkspace,
) -> String {
    let contents = workspace.get(file).unwrap_or("");
    let annotated = annotate_file_with_range_markers(contents, insert_range);
    let context = build_prompt_context(contents, insert_range)
        .map(|ctx| format!("\nExtracted context:\n{ctx}\n"))
        .unwrap_or_default();

    format!(
        "{header}\n\n\
File: {file}\n\
Range: {}:{} - {}:{}\n\
The region to edit is delimited by the markers `/*__NOVA_AI_RANGE_START__*/` and `/*__NOVA_AI_RANGE_END__*/` below.\n\
Do NOT include these marker comments in the output patch.\n\
{context}\n\
```java\n{annotated}\n```\n",
        insert_range.start.line + 1,
        insert_range.start.character + 1,
        insert_range.end.line + 1,
        insert_range.end.character + 1,
    )
}

fn annotate_file_with_range_markers(contents: &str, range: Range) -> String {
    const START: &str = "/*__NOVA_AI_RANGE_START__*/";
    const END: &str = "/*__NOVA_AI_RANGE_END__*/";

    let Some((start, end)) = lsp_range_to_offsets(contents, range) else {
        return contents.to_string();
    };
    if start > end || end > contents.len() {
        return contents.to_string();
    }

    let mut out = contents.to_string();
    out.insert_str(end, END);
    out.insert_str(start, START);
    out
}

fn build_prompt_context(contents: &str, range: Range) -> Option<String> {
    let (start, end) = lsp_range_to_offsets(contents, range)?;
    let selection = start..end;

    let builder = ContextBuilder::new();
    let req = ContextRequest::for_java_source_range(
        contents,
        selection,
        /*token_budget=*/ 800,
        PrivacyMode::default(),
        /*include_doc_comments=*/ true,
    );
    Some(builder.build(req).text)
}

fn lsp_range_to_offsets(contents: &str, range: Range) -> Option<(usize, usize)> {
    let index = LineIndex::new(contents);
    let start = index.offset_of_position(
        contents,
        CorePosition::new(range.start.line, range.start.character),
    )?;
    let end = index.offset_of_position(
        contents,
        CorePosition::new(range.end.line, range.end.character),
    )?;
    Some((u32::from(start) as usize, u32::from(end) as usize))
}

#[allow(dead_code)]
fn _placeholder_range() -> Range {
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 0,
            character: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_ai::safety::SafetyError;
    use nova_ai::CodeEditPolicyError;
    use nova_ai::PatchSafetyConfig;
    use pretty_assertions::assert_eq;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    #[derive(Default)]
    struct MockAiProvider {
        responses: Mutex<Vec<Result<String, PromptCompletionError>>>,
        calls: AtomicUsize,
    }

    impl MockAiProvider {
        fn new(responses: Vec<Result<String, PromptCompletionError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().collect()),
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl PromptCompletionProvider for MockAiProvider {
        async fn complete(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, PromptCompletionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .expect("lock responses")
                .pop()
                .unwrap_or_else(|| Err(PromptCompletionError::Provider("no more responses".into())))
        }
    }

    struct CancelOnRepairAttempt {
        cancel: CancellationToken,
    }

    impl CodegenProgressReporter for CancelOnRepairAttempt {
        fn report(&self, event: CodegenProgressEvent) {
            if matches!(event.stage, CodegenProgressStage::RepairAttempt) && event.attempt == 1 {
                self.cancel.cancel();
            }
        }
    }

    fn example_workspace() -> VirtualWorkspace {
        VirtualWorkspace::new(vec![(
            "Example.java".to_string(),
            "public class Example {\n    public int add(int a, int b) {\n        return 0;\n    }\n}\n"
                .to_string(),
        )])
    }

    fn root_uri() -> Uri {
        "file:///workspace/".parse().expect("uri")
    }

    fn example_action() -> AiCodeAction {
        AiCodeAction::GenerateMethodBody {
            file: "Example.java".into(),
            insert_range: Range {
                start: Position {
                    line: 2,
                    character: 0,
                },
                end: Position {
                    line: 3,
                    character: 0,
                },
            },
        }
    }

    #[tokio::test]
    async fn generate_method_body_repairs_invalid_patch() {
        let invalid_patch = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 3, "character": 0 } },
      "text": "        int x = \"oops\";\n        return x;\n"
    }
  ]
}"#;

        let valid_patch = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 3, "character": 0 } },
      "text": "        int x = 0;\n        return x;\n"
    }
  ]
}"#;

        let provider = MockAiProvider::new(vec![Ok(invalid_patch.into()), Ok(valid_patch.into())]);
        let mut config = CodeGenerationConfig::default();
        config.max_repair_attempts = 2;
        config.allow_repair = true;

        let executor = AiCodeActionExecutor::new(&provider, config, AiPrivacyConfig::default());
        let workspace = example_workspace();
        let cancel = CancellationToken::new();

        let outcome = executor
            .execute(example_action(), &workspace, &root_uri(), &cancel, None)
            .await
            .expect("success");
        assert_eq!(provider.call_count(), 2);

        match outcome {
            CodeActionOutcome::WorkspaceEdit(edit) => {
                let changes = edit.changes.expect("changes");
                let uri = crate::workspace_edit::join_uri(&root_uri(), Path::new("Example.java"));
                let edits = changes.get(&uri).expect("edit for file");
                assert_eq!(edits.len(), 1);
                assert!(edits[0].new_text.contains("int x = 0;"));
                assert!(!edits[0].new_text.contains("\"oops\""));
            }
            _ => panic!("expected edits"),
        }
    }

    #[tokio::test]
    async fn cancellation_stops_repair_loop() {
        let invalid_patch = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 3, "character": 0 } },
      "text": "        int x = \"oops\";\n        return x;\n"
    }
  ]
}"#;

        let provider = MockAiProvider::new(vec![
            Ok(invalid_patch.into()),
            // Would be returned on a repair attempt, but cancellation stops the loop first.
            Ok(invalid_patch.into()),
        ]);

        let mut config = CodeGenerationConfig::default();
        config.max_repair_attempts = 2;
        config.allow_repair = true;

        let executor = AiCodeActionExecutor::new(&provider, config, AiPrivacyConfig::default());
        let workspace = example_workspace();
        let cancel = CancellationToken::new();
        let progress = CancelOnRepairAttempt {
            cancel: cancel.clone(),
        };

        let err = executor
            .execute(
                example_action(),
                &workspace,
                &root_uri(),
                &cancel,
                Some(&progress),
            )
            .await
            .unwrap_err();
        assert_eq!(provider.call_count(), 1);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Cancelled) => {}
            other => panic!("unexpected error: {other:?}"),
        }

        assert_eq!(
            workspace.get("Example.java").unwrap(),
            example_workspace().get("Example.java").unwrap()
        );
    }

    #[tokio::test]
    async fn excluded_paths_are_enforced() {
        let patch = r#"{
  "edits": [
    {
      "file": "secret/Config.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "public class Config {}\n"
    }
  ]
}"#;

        let provider = MockAiProvider::new(vec![Ok(patch.into())]);
        let mut config = CodeGenerationConfig::default();
        config.safety = PatchSafetyConfig {
            excluded_path_prefixes: vec!["secret/".into()],
            ..PatchSafetyConfig::default()
        };

        let executor = AiCodeActionExecutor::new(&provider, config, AiPrivacyConfig::default());
        let workspace = VirtualWorkspace::default();
        let cancel = CancellationToken::new();

        let action = AiCodeAction::GenerateTest {
            file: "secret/Config.java".into(),
            insert_range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
        };

        let err = executor
            .execute(action, &workspace, &root_uri(), &cancel, None)
            .await
            .unwrap_err();
        assert_eq!(provider.call_count(), 1);

        match err {
            CodeActionError::Codegen(CodeGenerationError::Safety(SafetyError::ExcludedPath {
                path,
            })) => {
                assert_eq!(path, "secret/Config.java");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cloud_mode_refuses_when_anonymization_is_enabled_by_default() {
        let provider = MockAiProvider::new(vec![Ok("{}".into())]);
        let config = CodeGenerationConfig::default();
        let privacy = AiPrivacyConfig {
            local_only: false,
            // Cloud mode defaults to anonymization, but the first refusal reason should
            // be that anonymization makes patches impossible to apply reliably.
            anonymize: None,
            ..AiPrivacyConfig::default()
        };

        let executor = AiCodeActionExecutor::new(&provider, config, privacy);
        let workspace = example_workspace();
        let cancel = CancellationToken::new();

        let err = executor
            .execute(example_action(), &workspace, &root_uri(), &cancel, None)
            .await
            .unwrap_err();
        assert_eq!(provider.call_count(), 0);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Policy(
                CodeEditPolicyError::CloudEditsWithAnonymizationEnabled,
            )) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cloud_mode_refuses_when_anonymization_is_enabled_even_with_cloud_opt_in() {
        let provider = MockAiProvider::new(vec![Ok("{}".into())]);
        let config = CodeGenerationConfig::default();
        let privacy = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(true),
            allow_cloud_code_edits: true,
            ..AiPrivacyConfig::default()
        };

        let executor = AiCodeActionExecutor::new(&provider, config, privacy);
        let workspace = example_workspace();
        let cancel = CancellationToken::new();

        let err = executor
            .execute(example_action(), &workspace, &root_uri(), &cancel, None)
            .await
            .unwrap_err();
        assert_eq!(provider.call_count(), 0);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Policy(
                CodeEditPolicyError::CloudEditsWithAnonymizationEnabled,
            )) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cloud_mode_refuses_without_cloud_opt_in_when_anonymization_disabled() {
        let provider = MockAiProvider::new(vec![Ok("{}".into())]);
        let config = CodeGenerationConfig::default();
        let privacy = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(false),
            ..AiPrivacyConfig::default()
        };

        let executor = AiCodeActionExecutor::new(&provider, config, privacy);
        let workspace = example_workspace();
        let cancel = CancellationToken::new();

        let err = executor
            .execute(example_action(), &workspace, &root_uri(), &cancel, None)
            .await
            .unwrap_err();
        assert_eq!(provider.call_count(), 0);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Policy(
                CodeEditPolicyError::CloudEditsDisabled,
            )) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cloud_mode_requires_separate_opt_in_when_anonymization_disabled() {
        let provider = MockAiProvider::new(vec![Ok("{}".into())]);
        let config = CodeGenerationConfig::default();
        let privacy = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(false),
            allow_cloud_code_edits: true,
            ..AiPrivacyConfig::default()
        };

        let executor = AiCodeActionExecutor::new(&provider, config, privacy);
        let workspace = example_workspace();
        let cancel = CancellationToken::new();

        let err = executor
            .execute(example_action(), &workspace, &root_uri(), &cancel, None)
            .await
            .unwrap_err();
        assert_eq!(provider.call_count(), 0);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Policy(
                CodeEditPolicyError::CloudEditsWithoutAnonymizationDisabled,
            )) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_new_imports_triggers_failure() {
        let patch = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "import foo.Bar;\n"
    }
  ]
}"#;

        let provider = MockAiProvider::new(vec![Ok(patch.into())]);
        let mut config = CodeGenerationConfig::default();
        config.safety = PatchSafetyConfig {
            no_new_imports: true,
            ..PatchSafetyConfig::default()
        };

        let executor = AiCodeActionExecutor::new(&provider, config, AiPrivacyConfig::default());
        let workspace = example_workspace();
        let cancel = CancellationToken::new();

        let err = executor
            .execute(example_action(), &workspace, &root_uri(), &cancel, None)
            .await
            .unwrap_err();

        match err {
            CodeActionError::Codegen(CodeGenerationError::Safety(SafetyError::NewImports {
                file,
                imports,
            })) => {
                assert_eq!(file, "Example.java");
                assert_eq!(imports, vec!["import foo.Bar;".to_string()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn validation_report_includes_context_and_is_deterministic() {
        let invalid_patch = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 3, "character": 0 } },
      "text": "        int x = \"oops\";\n        return x;\n"
    }
  ]
}"#;

        async fn run_once(invalid_patch: &str) -> String {
            let provider = MockAiProvider::new(vec![Ok(invalid_patch.into())]);
            let mut config = CodeGenerationConfig::default();
            config.allow_repair = false;
            config.max_repair_attempts = 0;

            let executor = AiCodeActionExecutor::new(&provider, config, AiPrivacyConfig::default());
            let workspace = example_workspace();
            let cancel = CancellationToken::new();

            let err = executor
                .execute(example_action(), &workspace, &root_uri(), &cancel, None)
                .await
                .unwrap_err();

            match err {
                CodeActionError::Codegen(CodeGenerationError::ValidationFailed { report }) => {
                    assert!(
                        !report.new_diagnostics.is_empty(),
                        "expected at least one diagnostic"
                    );
                    let block = report.to_prompt_block();
                    assert!(
                        block.contains('^'),
                        "expected caret marker in context snippet: {block}"
                    );
                    block
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        let out1 = run_once(invalid_patch).await;
        let out2 = run_once(invalid_patch).await;
        assert_eq!(out1, out2);
    }
}
