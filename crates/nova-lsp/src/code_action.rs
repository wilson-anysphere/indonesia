use std::collections::HashMap;
use std::path::Path;

use lsp_types::{
    CreateFile, CreateFileOptions, DocumentChangeOperation, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp, TextDocumentEdit,
    TextEdit, Uri, WorkspaceEdit,
};
use nova_ai::context::{ContextBuilder, ContextRequest};
use nova_ai::workspace::VirtualWorkspace;
use nova_ai::PrivacyMode;
use nova_ai_codegen::{
    generate_patch, CodeGenerationConfig, CodeGenerationError, CodegenProgressEvent,
    CodegenProgressReporter, CodegenProgressStage, PromptCompletionError, PromptCompletionProvider,
    ValidationConfig,
};
use nova_config::AiPrivacyConfig;
use nova_core::{LineIndex, Position as CorePosition};
use nova_db::InMemoryFileStore;
use nova_ide::diagnostics::Diagnostic;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum AiCodeAction {
    ExplainError { diagnostic: Diagnostic },
    GenerateMethodBody { file: String, insert_range: Range },
    GenerateTest {
        file: String,
        insert_range: Range,
        /// A human readable description of the symbol under test (e.g. method signature).
        ///
        /// This is provided by `GenerateTestsArgs.target` and is particularly important when
        /// writing tests into a *different* destination file (e.g. a derived `src/test/java/...`
        /// file), where the destination file contents alone may not contain enough information.
        target: Option<String>,
        /// Relative path to the file containing the code under test.
        source_file: Option<String>,
        /// Snippet of the selected code under test (best-effort).
        source_snippet: Option<String>,
        /// Optional surrounding context (best-effort, e.g. enclosing class).
        context: Option<String>,
    },
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
                    root_uri,
                    &self.privacy,
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
            AiCodeAction::GenerateTest {
                file,
                insert_range,
                target,
                source_file,
                source_snippet,
                context,
            } => {
                if let Some(progress) = progress {
                    progress.report(CodegenProgressEvent {
                        stage: CodegenProgressStage::BuildingPrompt,
                        attempt: 0,
                        message: "Building context…".to_string(),
                    });
                }
                let prompt = build_generate_tests_prompt(
                    &file,
                    insert_range,
                    workspace,
                    root_uri,
                    &self.privacy,
                    /*target=*/ target.as_deref(),
                    /*source_file=*/ source_file.as_deref(),
                    /*source_snippet=*/ source_snippet.as_deref(),
                    /*context=*/ context.as_deref(),
                );

                let mut config = self.config.clone();
                config.validation = ValidationConfig::relaxed_for_tests();
                // Test generation commonly creates new files. Enable that explicitly (it's disabled
                // by default for safety).
                config.safety.allow_new_files = true;
                if config.safety.allowed_path_prefixes.is_empty() {
                    // Allow edits in the selected file (for context updates) and in typical test
                    // roots (for creating the generated test file).
                    config.safety.allowed_path_prefixes = vec![file.clone(), "src/test/".into()];
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

fn build_generate_tests_prompt(
    file: &str,
    insert_range: Range,
    workspace: &VirtualWorkspace,
    root_uri: &Uri,
    privacy: &AiPrivacyConfig,
    target: Option<&str>,
    source_file: Option<&str>,
    source_snippet: Option<&str>,
    context: Option<&str>,
) -> String {
    let mut preamble = String::new();
    if let Some(target) = target.filter(|t| !t.trim().is_empty()) {
        preamble.push_str("Test target: ");
        preamble.push_str(target.trim());
        preamble.push('\n');
    }
    if let Some(source_file) = source_file.filter(|f| !f.trim().is_empty()) {
        preamble.push_str("Source file under test: ");
        preamble.push_str(source_file.trim());
        preamble.push('\n');
    }
    if let Some(source_snippet) = source_snippet.filter(|s| !s.trim().is_empty()) {
        preamble.push_str("\nSelected source snippet:\n```java\n");
        preamble.push_str(source_snippet.trim_end());
        preamble.push_str("\n```\n");
    }
    if let Some(context) = context.filter(|s| !s.trim().is_empty()) {
        preamble.push_str("\nSurrounding context:\n```java\n");
        preamble.push_str(context.trim_end());
        preamble.push_str("\n```\n");
    }

    let insert = build_insert_prompt(
        "Generate Java unit tests for the target described above and write them into the marked range.",
        file,
        insert_range,
        workspace,
        root_uri,
        privacy,
    );

    if preamble.trim().is_empty() {
        insert
    } else {
        format!("{preamble}\n{insert}")
    }
}

fn workspace_edit_from_virtual_workspace<'a>(
    root_uri: &Uri,
    before: &VirtualWorkspace,
    after: &VirtualWorkspace,
    touched_files: impl IntoIterator<Item = &'a String>,
) -> WorkspaceEdit {
    let mut changed: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    let mut has_new_file = false;

    for file in touched_files {
        let before_text = before.get(file).map(str::to_string);
        let after_text = after.get(file).map(str::to_string);
        if before_text == after_text {
            continue;
        }
        if before_text.is_none() && after_text.is_some() {
            has_new_file = true;
        }
        changed.push((file.clone(), before_text, after_text));
    }

    if !has_new_file {
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        for (file, before_text, after_text) in changed {
            let (Some(before_text), Some(after_text)) = (before_text, after_text) else {
                continue;
            };
            let uri = crate::workspace_edit::join_uri(root_uri, Path::new(&file));
            changes.insert(
                uri,
                vec![TextEdit {
                    range: crate::workspace_edit::full_document_range(&before_text),
                    new_text: after_text,
                }],
            );
        }
        return WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
    }

    let mut ops: Vec<DocumentChangeOperation> = Vec::new();
    for (file, before_text, after_text) in &changed {
        if before_text.is_none() && after_text.is_some() {
            let uri = crate::workspace_edit::join_uri(root_uri, Path::new(file));
            ops.push(DocumentChangeOperation::Op(ResourceOp::Create(
                CreateFile {
                    uri,
                    options: Some(CreateFileOptions {
                        overwrite: Some(false),
                        ignore_if_exists: Some(true),
                    }),
                    annotation_id: None,
                },
            )));
        }
    }

    for (file, before_text, after_text) in changed {
        let Some(after_text) = after_text else {
            // Deletions are not surfaced in `WorkspaceEdit` via this helper today.
            continue;
        };
        let before_text = before_text.unwrap_or_default();
        let uri = crate::workspace_edit::join_uri(root_uri, Path::new(&file));
        ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
            edits: vec![OneOf::Left(TextEdit {
                range: crate::workspace_edit::full_document_range(&before_text),
                new_text: after_text,
            })],
        }));
    }

    WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(ops)),
        change_annotations: None,
    }
}

fn build_insert_prompt(
    header: &str,
    file: &str,
    insert_range: Range,
    workspace: &VirtualWorkspace,
    root_uri: &Uri,
    privacy: &AiPrivacyConfig,
) -> String {
    let contents = workspace.get(file).unwrap_or("");
    let annotated = annotate_file_with_range_markers(contents, insert_range);
    let context = build_prompt_context(root_uri, file, contents, insert_range, privacy)
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

fn build_prompt_context(
    root_uri: &Uri,
    file: &str,
    contents: &str,
    range: Range,
    privacy: &AiPrivacyConfig,
) -> Option<String> {
    let (start, end) = lsp_range_to_offsets(contents, range)?;
    let selection = start..end;

    let builder = ContextBuilder::new();
    let privacy_mode = PrivacyMode::from_ai_privacy_config(privacy);
    let req = ContextRequest::for_java_source_range(
        contents,
        selection,
        /*token_budget=*/ 800,
        privacy_mode,
        /*include_doc_comments=*/ true,
    );
    Some(
        builder
            .build(enrich_context_request(root_uri, file, contents, range, req))
            .text,
    )
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

fn enrich_context_request(
    root_uri: &Uri,
    file: &str,
    contents: &str,
    range: Range,
    mut req: ContextRequest,
) -> ContextRequest {
    let Some(root_path) = nova_core::file_uri_to_path(root_uri.as_str()).ok() else {
        return req;
    };

    let file_path = root_path.as_path().join(Path::new(file));
    req.file_path = Some(file_path.display().to_string());
    req.project_context = project_context_for_path(&file_path);
    req.semantic_context = semantic_context_for_hover(&file_path, contents, range.start);

    req
}

fn project_context_for_path(path: &Path) -> Option<nova_ai::context::ProjectContext> {
    let root = nova_ide::framework_cache::project_root_for_path(path);
    let config = nova_ide::framework_cache::project_config(&root)?;

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

fn semantic_context_for_hover(path: &Path, text: &str, position: Position) -> Option<String> {
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
    use tempfile::TempDir;
    use tokio::sync::oneshot;
    use tokio::time::{timeout, Duration};

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

    struct StaticProvider {
        response: String,
    }

    #[async_trait::async_trait]
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
        started_tx: Mutex<Option<oneshot::Sender<()>>>,
        resume_rx: Mutex<Option<oneshot::Receiver<()>>>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl PromptCompletionProvider for BlockingProvider {
        async fn complete(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, PromptCompletionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(tx) = self.started_tx.lock().expect("lock started").take() {
                let _ = tx.send(());
            }
            let rx = {
                let mut guard = self.resume_rx.lock().expect("lock resume");
                guard.take()
            };
            if let Some(rx) = rx {
                let _ = rx.await;
            }
            Ok(r#"{"edits":[]}"#.to_string())
        }
    }

    #[test]
    fn build_insert_prompt_includes_project_and_semantic_context() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");

        let file_path = src_dir.join("Main.java");
        let file = "src/Main.java";
        let text = r#"class Main { void run() { String s = "hi"; } }"#;
        std::fs::write(&file_path, text).expect("write java file");

        let root_uri: Uri = url::Url::from_file_path(root)
            .expect("file url")
            .to_string()
            .parse()
            .expect("uri");

        let offset = text.find("s =").expect("variable occurrence");
        let start = crate::text_pos::lsp_position(text, offset).expect("start pos");
        let end = crate::text_pos::lsp_position(text, offset + 1).expect("end pos");
        let range = Range { start, end };

        let workspace = VirtualWorkspace::new([(file.to_string(), text.to_string())]);
        let prompt = build_insert_prompt(
            "Generate a Java method body for the marked range.",
            file,
            range,
            &workspace,
            &root_uri,
            &AiPrivacyConfig::default(),
        );

        assert!(
            prompt.contains("## Project context"),
            "expected project context in prompt: {prompt}"
        );
        assert!(
            prompt.contains("## Symbol/type info"),
            "expected semantic context in prompt: {prompt}"
        );
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
    async fn cancellation_during_model_call_aborts_quickly() {
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (resume_tx, resume_rx) = oneshot::channel::<()>();
        let provider = BlockingProvider {
            started_tx: Mutex::new(Some(started_tx)),
            resume_rx: Mutex::new(Some(resume_rx)),
            calls: AtomicUsize::new(0),
        };

        let executor = AiCodeActionExecutor::new(
            &provider,
            CodeGenerationConfig::default(),
            AiPrivacyConfig::default(),
        );
        let workspace = example_workspace();
        let cancel = CancellationToken::new();
        let root = root_uri();
        let mut fut =
            Box::pin(executor.execute(example_action(), &workspace, &root, &cancel, None));

        tokio::select! {
            started = timeout(Duration::from_secs(1), started_rx) => {
                started.expect("provider should start").expect("started");
            }
            res = &mut fut => {
                panic!("executor returned unexpectedly early: {res:?}");
            }
        }

        cancel.cancel();
        // Let the provider return so the codegen loop can observe cancellation.
        let _ = resume_tx.send(());

        let err = timeout(Duration::from_secs(1), &mut fut)
            .await
            .expect("executor should return quickly after cancellation")
            .unwrap_err();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        match err {
            CodeActionError::Codegen(CodeGenerationError::Cancelled) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn generate_tests_creates_new_file_workspace_edit_includes_create_file_op() {
        let test_file = "src/test/java/com/example/ExampleTest.java";
        let patch = format!(
            r#"{{
  "edits": [
    {{
      "file": "{test_file}",
      "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 0 }} }},
      "text": "package com.example;\n\npublic class ExampleTest {{}}\n"
    }}
  ]
}}"#
        );
        let provider = StaticProvider { response: patch };

        let mut config = CodeGenerationConfig::default();
        config.safety = PatchSafetyConfig {
            allow_new_files: true,
            allowed_path_prefixes: vec![test_file.to_string()],
            ..PatchSafetyConfig::default()
        };

        let executor = AiCodeActionExecutor::new(&provider, config, AiPrivacyConfig::default());
        let workspace = VirtualWorkspace::default();
        let cancel = CancellationToken::new();

        let outcome = executor
            .execute(
                AiCodeAction::GenerateTest {
                    file: test_file.to_string(),
                    insert_range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    target: None,
                    source_file: None,
                    source_snippet: None,
                    context: None,
                },
                &workspace,
                &root_uri(),
                &cancel,
                None,
            )
            .await
            .expect("success");

        let CodeActionOutcome::WorkspaceEdit(edit) = outcome else {
            panic!("expected workspace edit");
        };

        let doc_changes = edit.document_changes.expect("document_changes");
        let DocumentChanges::Operations(ops) = doc_changes else {
            panic!("expected operations-based document changes");
        };
        assert!(
            ops.iter()
                .any(|op| matches!(op, DocumentChangeOperation::Op(ResourceOp::Create(_)))),
            "expected CreateFile op, got: {ops:?}"
        );

        let text_edits = ops
            .iter()
            .filter_map(|op| match op {
                DocumentChangeOperation::Edit(TextDocumentEdit { edits, .. }) => Some(edits),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert!(
            text_edits.iter().any(|edit| match edit {
                OneOf::Left(TextEdit { new_text, .. }) => new_text.contains("class ExampleTest"),
                OneOf::Right(_) => false,
            }),
            "expected text edit with class contents, got: {text_edits:?}"
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
            target: None,
            source_file: None,
            source_snippet: None,
            context: None,
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
            anonymize_identifiers: None,
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
            anonymize_identifiers: Some(true),
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
            anonymize_identifiers: Some(false),
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
            anonymize_identifiers: Some(false),
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
