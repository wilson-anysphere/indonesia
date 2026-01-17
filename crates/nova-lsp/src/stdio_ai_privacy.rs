use crate::ServerState;

use nova_ai::{AiError, ExcludedPathMatcher};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn is_ai_excluded_path(state: &ServerState, path: &Path) -> bool {
    if !state.ai_config.enabled {
        return false;
    }

    is_excluded_by_matcher(state.ai_privacy_excluded_matcher.as_ref(), path)
}

pub(super) fn is_excluded_by_matcher(
    matcher: &Result<ExcludedPathMatcher, AiError>,
    path: &Path,
) -> bool {
    match matcher {
        Ok(matcher) => matcher.is_match(path),
        // Best-effort fail-closed: if privacy configuration is invalid, avoid starting any AI work
        // based on potentially sensitive files.
        Err(err) => {
            static INVALID_MATCHER_LOGGED: AtomicBool = AtomicBool::new(false);
            if !INVALID_MATCHER_LOGGED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    target = "nova.lsp",
                    error = %err,
                    "AI excluded-path matcher is invalid; treating all paths as excluded"
                );
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use lsp_types::{
        CodeActionContext, CodeActionParams, CompletionList, CompletionParams, Diagnostic,
        Position, Range, TextDocumentIdentifier, TextDocumentPositionParams, Uri,
    };
    use nova_ide::{
        CODE_ACTION_KIND_AI_GENERATE, CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN,
    };
    use nova_memory::MemoryBudgetOverrides;
    use nova_scheduler::CancellationToken;
    use serde_json::Value;
    use tempfile::TempDir;

    fn code_action_params(uri: &Uri) -> Value {
        let pos = Position::new(0, 0);
        let range = Range::new(pos, pos);
        let diagnostic = Diagnostic {
            range: range.clone(),
            message: "boom".to_string(),
            ..Diagnostic::default()
        };

        serde_json::to_value(CodeActionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range,
            context: CodeActionContext {
                diagnostics: vec![diagnostic],
                ..CodeActionContext::default()
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("code action params")
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
            code_action_params(&secret_uri),
            &mut state,
            CancellationToken::new(),
        )
        .expect("code action response");
        let excluded_actions = excluded_actions.as_array().expect("array");

        let explain = excluded_actions
            .iter()
            .find(|action| {
                action.get("kind").and_then(|k| k.as_str()) == Some(CODE_ACTION_KIND_EXPLAIN)
            })
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
                    .is_some_and(|kind| {
                        kind == CODE_ACTION_KIND_AI_GENERATE || kind == CODE_ACTION_KIND_AI_TESTS
                    })
            }),
            "expected no AI code-edit actions for excluded file"
        );

        let allowed_actions = crate::stdio_code_action::handle_code_action(
            code_action_params(&main_uri),
            &mut state,
            CancellationToken::new(),
        )
        .expect("code action response");
        let allowed_actions = allowed_actions.as_array().expect("array");
        let explain = allowed_actions
            .iter()
            .find(|action| {
                action.get("kind").and_then(|k| k.as_str()) == Some(CODE_ACTION_KIND_EXPLAIN)
            })
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
}
