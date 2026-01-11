use crate::ai_codegen::{run_code_generation, CodeGenerationConfig, CodeGenerationError};
use lsp_types::{Position, Range};
use nova_ai::provider::AiProvider;
use nova_ai::provider::AiProviderError;
use nova_ai::workspace::VirtualWorkspace;
use nova_ai::CancellationToken;
use nova_config::AiPrivacyConfig;
use nova_ide::diagnostics::Diagnostic;
use thiserror::Error;

#[derive(Debug, Clone)]
pub enum AiCodeAction {
    ExplainError { diagnostic: Diagnostic },
    GenerateMethodBody { file: String, insert_range: Range },
    GenerateTest { file: String, insert_range: Range },
}

#[derive(Debug, Clone)]
pub enum CodeActionOutcome {
    Explanation(String),
    AppliedEdits(VirtualWorkspace),
}

#[derive(Debug, Error)]
pub enum CodeActionError {
    #[error(transparent)]
    Provider(#[from] AiProviderError),
    #[error(transparent)]
    Codegen(#[from] CodeGenerationError),
}

pub struct AiCodeActionExecutor<'a> {
    provider: &'a dyn AiProvider,
    config: CodeGenerationConfig,
    privacy: AiPrivacyConfig,
}

impl<'a> AiCodeActionExecutor<'a> {
    pub fn new(
        provider: &'a dyn AiProvider,
        config: CodeGenerationConfig,
        privacy: AiPrivacyConfig,
    ) -> Self {
        Self {
            provider,
            config,
            privacy,
        }
    }

    pub fn execute(
        &self,
        action: AiCodeAction,
        workspace: &VirtualWorkspace,
        cancel: &CancellationToken,
    ) -> Result<CodeActionOutcome, CodeActionError> {
        match action {
            AiCodeAction::ExplainError { diagnostic } => {
                let prompt = format!(
                    "Explain this compiler diagnostic:\n\n{:?}\n\nRespond in plain English.",
                    diagnostic
                );
                let explanation = self.provider.complete(&prompt, cancel)?;
                Ok(CodeActionOutcome::Explanation(explanation))
            }
            AiCodeAction::GenerateMethodBody { file, insert_range } => {
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
                let result = run_code_generation(
                    self.provider,
                    workspace,
                    &prompt,
                    &config,
                    &self.privacy,
                    cancel,
                )?;
                Ok(CodeActionOutcome::AppliedEdits(result.formatted_workspace))
            }
            AiCodeAction::GenerateTest { file, insert_range } => {
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
                let result = run_code_generation(
                    self.provider,
                    workspace,
                    &prompt,
                    &config,
                    &self.privacy,
                    cancel,
                )?;
                Ok(CodeActionOutcome::AppliedEdits(result.formatted_workspace))
            }
        }
    }
}

fn build_insert_prompt(
    header: &str,
    file: &str,
    insert_range: Range,
    workspace: &VirtualWorkspace,
) -> String {
    let contents = workspace.get(file).unwrap_or("");
    let marker = format!(
        "File: {file}\nRange: {}:{} - {}:{}\n\n```java\n{}\n```\n",
        insert_range.start.line + 1,
        insert_range.start.character + 1,
        insert_range.end.line + 1,
        insert_range.end.character + 1,
        contents
    );

    format!("{header}\n\n{marker}")
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
    use nova_ai::provider::AiProviderError;
    use nova_ai::safety::SafetyError;
    use nova_ai::PatchSafetyConfig;
    use nova_ai::CodeEditPolicyError;
    use pretty_assertions::assert_eq;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    #[derive(Default)]
    struct MockAiProvider {
        responses: Mutex<Vec<Result<String, AiProviderError>>>,
        calls: AtomicUsize,
    }

    impl MockAiProvider {
        fn new(responses: Vec<Result<String, AiProviderError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().collect()),
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl AiProvider for MockAiProvider {
        fn complete(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, AiProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .expect("lock responses")
                .pop()
                .unwrap_or_else(|| Err(AiProviderError::Provider("no more responses".into())))
        }
    }

    fn example_workspace() -> VirtualWorkspace {
        VirtualWorkspace::new(vec![(
            "Example.java".to_string(),
            "public class Example {\n    public int add(int a, int b) {\n        return 0;\n    }\n}\n"
                .to_string(),
        )])
    }

    #[test]
    fn generate_method_body_repairs_invalid_patch() {
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
        let cancel = CancellationToken::default();

        let action = AiCodeAction::GenerateMethodBody {
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
        };

        let outcome = executor
            .execute(action, &workspace, &cancel)
            .expect("success");
        assert_eq!(provider.call_count(), 2);

        match outcome {
            CodeActionOutcome::AppliedEdits(updated) => {
                let text = updated.get("Example.java").unwrap();
                assert!(text.contains("int x = 0;"));
                assert!(!text.contains("\"oops\""));
            }
            _ => panic!("expected edits"),
        }
    }

    #[test]
    fn cancellation_stops_repair_loop() {
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
            Err(AiProviderError::Cancelled),
        ]);

        let mut config = CodeGenerationConfig::default();
        config.max_repair_attempts = 2;
        config.allow_repair = true;

        let executor = AiCodeActionExecutor::new(&provider, config, AiPrivacyConfig::default());
        let workspace = example_workspace();
        let cancel = CancellationToken::default();

        let action = AiCodeAction::GenerateMethodBody {
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
        };

        let err = executor.execute(action, &workspace, &cancel).unwrap_err();
        assert_eq!(provider.call_count(), 2);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Cancelled) => {}
            other => panic!("unexpected error: {other:?}"),
        }

        assert_eq!(
            workspace.get("Example.java").unwrap(),
            example_workspace().get("Example.java").unwrap()
        );
    }

    #[test]
    fn excluded_paths_are_enforced() {
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
        let cancel = CancellationToken::default();

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

        let err = executor.execute(action, &workspace, &cancel).unwrap_err();
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

    #[test]
    fn cloud_mode_refuses_when_anonymization_is_enabled_by_default() {
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
        let cancel = CancellationToken::default();

        let action = AiCodeAction::GenerateMethodBody {
            file: "Example.java".into(),
            insert_range: Range {
                start: Position { line: 2, character: 0 },
                end: Position { line: 3, character: 0 },
            },
        };

        let err = executor.execute(action, &workspace, &cancel).unwrap_err();
        assert_eq!(provider.call_count(), 0);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Policy(
                CodeEditPolicyError::CloudEditsWithAnonymizationEnabled,
            )) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn cloud_mode_refuses_when_anonymization_is_enabled_even_with_cloud_opt_in() {
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
        let cancel = CancellationToken::default();

        let action = AiCodeAction::GenerateTest {
            file: "Example.java".into(),
            insert_range: Range {
                start: Position { line: 2, character: 0 },
                end: Position { line: 3, character: 0 },
            },
        };

        let err = executor.execute(action, &workspace, &cancel).unwrap_err();
        assert_eq!(provider.call_count(), 0);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Policy(
                CodeEditPolicyError::CloudEditsWithAnonymizationEnabled,
            )) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn cloud_mode_refuses_without_cloud_opt_in_when_anonymization_disabled() {
        let provider = MockAiProvider::new(vec![Ok("{}".into())]);
        let config = CodeGenerationConfig::default();
        let privacy = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(false),
            ..AiPrivacyConfig::default()
        };

        let executor = AiCodeActionExecutor::new(&provider, config, privacy);
        let workspace = example_workspace();
        let cancel = CancellationToken::default();

        let action = AiCodeAction::GenerateMethodBody {
            file: "Example.java".into(),
            insert_range: Range {
                start: Position { line: 2, character: 0 },
                end: Position { line: 3, character: 0 },
            },
        };

        let err = executor.execute(action, &workspace, &cancel).unwrap_err();
        assert_eq!(provider.call_count(), 0);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Policy(CodeEditPolicyError::CloudEditsDisabled)) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn cloud_mode_requires_separate_opt_in_when_anonymization_disabled() {
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
        let cancel = CancellationToken::default();

        let action = AiCodeAction::GenerateMethodBody {
            file: "Example.java".into(),
            insert_range: Range {
                start: Position { line: 2, character: 0 },
                end: Position { line: 3, character: 0 },
            },
        };

        let err = executor.execute(action, &workspace, &cancel).unwrap_err();
        assert_eq!(provider.call_count(), 0);
        match err {
            CodeActionError::Codegen(CodeGenerationError::Policy(
                CodeEditPolicyError::CloudEditsWithoutAnonymizationDisabled,
            )) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
