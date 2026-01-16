use crate::rpc_out::RpcOut;
use crate::stdio_ai_context::{build_context_request, build_context_request_from_args};
use crate::stdio_ai_privacy::is_ai_excluded_path;
use crate::stdio_paths::path_from_uri;
use crate::stdio_progress::{
    chunk_utf8_by_bytes, send_log_message, send_progress_begin, send_progress_end,
    send_progress_report,
};
use crate::ServerState;

use lsp_types::ProgressToken;
use nova_ai::context::{ContextDiagnostic, ContextDiagnosticKind, ContextDiagnosticSeverity};
use nova_ide::ExplainErrorArgs;
use nova_scheduler::CancellationToken;

pub(super) fn run_ai_explain_error(
    args: ExplainErrorArgs,
    work_done_token: Option<ProgressToken>,
    state: &mut ServerState,
    rpc_out: &impl RpcOut,
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

    let uri_path = args.uri.as_deref().and_then(path_from_uri);
    let excluded = uri_path
        .as_deref()
        .is_some_and(|path| is_ai_excluded_path(state, path));

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Explain this error")?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(rpc_out, "AI: explaining error…")?;
    let mut ctx = if excluded {
        // `ai.privacy.excluded_paths` is a server-side hard stop for sending file-backed code to the
        // model. Even if a client supplies `code`, omit it and build a diagnostic-only prompt.
        //
        // Keep this conservative: don't run semantic search or attach URI/range metadata that could
        // leak excluded file paths into prompts.
        build_context_request(
            state,
            "[code context omitted due to excluded_paths]".to_string(),
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
    send_progress_report(rpc_out, work_done_token.as_ref(), "Calling model…", None)?;
    let output = runtime
        .block_on(ai.explain_error(&args.diagnostic_message, ctx, cancel.clone()))
        .map_err(|e| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(rpc_out, "AI: explanation ready")?;
    send_ai_output(rpc_out, "AI explainError", &output)?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(output))
}

pub(super) const AI_LOG_MESSAGE_CHUNK_BYTES: usize = 6 * 1024;

fn send_ai_output(out: &impl RpcOut, label: &str, output: &str) -> Result<(), (i32, String)> {
    let chunks = chunk_utf8_by_bytes(output, AI_LOG_MESSAGE_CHUNK_BYTES);
    let total = chunks.len();
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let message = if total == 1 {
            format!("{label}: {chunk}")
        } else {
            format!("{label} ({}/{total}): {chunk}", idx + 1)
        };
        send_log_message(out, &message)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use httpmock::prelude::*;
    use lsp_types::{LogMessageParams, ProgressParams, ProgressParamsValue, WorkDoneProgress};
    use std::time::Duration;
    use tempfile::TempDir;

    use lsp_types::Uri;
    use nova_ai::NovaAi;
    use nova_memory::MemoryBudgetOverrides;

    #[test]
    fn run_ai_explain_error_emits_chunked_log_messages_and_progress() {
        let server = MockServer::start();
        let long = "Nova AI output ".repeat((AI_LOG_MESSAGE_CHUNK_BYTES * 2) / 14 + 32);
        let mock = server.mock(|when, then| {
            when.method(POST).path("/complete");
            then.status(200).json_body(serde_json::Value::Object({
                let mut resp = serde_json::Map::new();
                resp.insert(
                    "completion".to_string(),
                    serde_json::Value::String(long.clone()),
                );
                resp
            }));
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

        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );
        state.ai = Some(ai);
        state.runtime = Some(runtime);

        let work_done_token = Some(ProgressToken::String("token".to_string()));
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

        let work_done_events: Vec<WorkDoneProgress> = messages
            .iter()
            .filter(|msg| msg.get("method").and_then(|m| m.as_str()) == Some("$/progress"))
            .filter_map(|msg| msg.get("params").cloned())
            .filter_map(|params| serde_json::from_value::<ProgressParams>(params).ok())
            .filter_map(|params| match params.value {
                ProgressParamsValue::WorkDone(event) => Some(event),
            })
            .collect();

        assert!(
            work_done_events
                .iter()
                .any(|event| matches!(event, WorkDoneProgress::Begin(_))),
            "expected a work-done progress begin notification"
        );

        assert!(
            work_done_events
                .iter()
                .any(|event| matches!(event, WorkDoneProgress::End(_))),
            "expected a work-done progress end notification"
        );

        let mut output_chunks = Vec::new();
        for msg in &messages {
            if msg.get("method").and_then(|m| m.as_str()) != Some("window/logMessage") {
                continue;
            }
            let Some(params) = msg.get("params").cloned() else {
                continue;
            };
            let Ok(params) = serde_json::from_value::<LogMessageParams>(params) else {
                continue;
            };
            if !params.message.starts_with("AI explainError") {
                continue;
            }
            let (_, chunk) = params
                .message
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
            when.method(POST)
                .path("/complete")
                .matches(explain_error_request_omits_excluded_code);
            then.status(200).json_body(serde_json::Value::Object({
                let mut resp = serde_json::Map::new();
                resp.insert(
                    "completion".to_string(),
                    serde_json::Value::String("mock explanation".to_string()),
                );
                resp
            }));
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
}
