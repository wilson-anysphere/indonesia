use crate::rpc_out::RpcOut;
use crate::ServerState;
use crate::stdio_paths::path_from_uri;

use lsp_types::{
    DidChangeWatchedFilesParams as LspDidChangeWatchedFilesParams,
    FileChangeType as LspFileChangeType,
    Uri as LspUri,
};
use nova_ai::{ExcludedPathMatcher, NovaAi};
use nova_vfs::{ChangeEvent, VfsPath};
use serde::Deserialize;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "ai")]
use nova_ai::{
    AiClient, CloudMultiTokenCompletionProvider, CompletionContextBuilder, MultiTokenCompletionProvider,
};
#[cfg(feature = "ai")]
use nova_ide::{CompletionConfig, CompletionEngine};

pub(super) fn handle_notification(
    method: &str,
    params: serde_json::Value,
    state: &mut ServerState,
    _out: &impl RpcOut,
) -> std::io::Result<()> {
    // LSP lifecycle: after `shutdown`, the client should only send `exit`. Ignore any
    // other notifications to avoid doing unnecessary work during teardown.
    if state.shutdown_requested {
        return Ok(());
    }

    match method {
        // Handled in the router/main loop.
        "$/cancelRequest" | "exit" => {}
        "textDocument/didOpen" => {
            // Some of Nova's integration tests (and older clients) omit the required
            // `languageId` / `version` fields in `didOpen`. Be lenient and apply
            // reasonable defaults so the server remains usable.
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DidOpenTextDocumentParams {
                text_document: DidOpenTextDocumentItem,
            }

            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DidOpenTextDocumentItem {
                uri: LspUri,
                text: String,
                #[serde(default)]
                version: Option<i32>,
            }

            let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(params) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            let uri_string = uri.to_string();
            let version = params.text_document.version.unwrap_or(0);
            let text = params.text_document.text;
            if let (Some(dist), Some(path)) =
                (state.distributed.as_ref(), path_from_uri(uri.as_str()))
            {
                if dist.contains_path(&path) {
                    let frontend = Arc::clone(&dist.frontend);
                    let text_for_router = text.clone();
                    let _ = dist.runtime.spawn(async move {
                        if let Err(err) = frontend.did_change_file(path, text_for_router).await {
                            tracing::warn!(
                                target = "nova.lsp",
                                error = ?err,
                                "distributed router update failed for didOpen"
                            );
                        }
                    });
                }
            }
            let file_id = state.analysis.open_document(uri.clone(), text, version);
            state.semantic_search_mark_file_open(file_id);
            state.semantic_search_index_open_document(file_id);
            let canonical_uri = state
                .analysis
                .vfs
                .path_for_id(file_id)
                .and_then(|p| p.to_uri())
                .unwrap_or(uri_string);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
            state.queue_publish_diagnostics(uri);
        }
        "textDocument/didChange" => {
            let Ok(params) =
                serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(params)
            else {
                return Ok(());
            };
            let uri_string = params.text_document.uri.to_string();
            let evt = state.analysis.apply_document_changes(
                &params.text_document.uri,
                params.text_document.version,
                &params.content_changes,
            );
            if let Err(err) = evt {
                tracing::warn!(
                    target = "nova.lsp",
                    uri = uri_string,
                    "failed to apply document changes: {err}"
                );
                return Ok(());
            }
            if let Ok(ChangeEvent::DocumentChanged { file_id, .. }) = &evt {
                state.semantic_search_index_open_document(*file_id);
                if let (Some(dist), Some(path)) = (
                    state.distributed.as_ref(),
                    path_from_uri(params.text_document.uri.as_str()),
                ) {
                    if dist.contains_path(&path) {
                        if let Some(text) = state
                            .analysis
                            .file_contents
                            .get(file_id)
                            .map(|text| text.as_str().to_owned())
                        {
                            let frontend = Arc::clone(&dist.frontend);
                            let _ = dist.runtime.spawn(async move {
                                if let Err(err) = frontend.did_change_file(path, text).await {
                                    tracing::warn!(
                                        target = "nova.lsp",
                                        error = ?err,
                                        "distributed router update failed for didChange"
                                    );
                                }
                            });
                        }
                    }
                }
            }
            let canonical_uri = VfsPath::from(&params.text_document.uri)
                .to_uri()
                .unwrap_or_else(|| uri_string);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
            state.queue_publish_diagnostics(params.text_document.uri);
        }
        "textDocument/willSave" => {
            let Ok(_params) =
                serde_json::from_value::<lsp_types::WillSaveTextDocumentParams>(params)
            else {
                return Ok(());
            };

            // Best-effort support: today we don't need to do anything on will-save, but parsing the
            // message keeps the server compatible with clients that send it.
        }
        "textDocument/didSave" => {
            let Ok(params) = serde_json::from_value::<lsp_types::DidSaveTextDocumentParams>(params)
            else {
                return Ok(());
            };

            let uri = params.text_document.uri;
            let uri_string = uri.to_string();
            let path = VfsPath::from(&uri);
            let is_open = state.analysis.vfs.overlay().is_open(&path);

            match params.text {
                Some(text) => {
                    if is_open {
                        // `didSave` does not include a document version. Best-effort: replace the
                        // overlay contents while keeping the document open; subsequent `didChange`
                        // notifications will provide versioned edits again.
                        let file_id = state.analysis.open_document(uri.clone(), text, 0);
                        state.semantic_search_index_open_document(file_id);
                    } else {
                        // If the document is not open, record the saved contents as our best view
                        // of the file until we receive a file-watch refresh.
                        let (file_id, _path) = state.analysis.file_id_for_uri(&uri);
                        let text = Arc::new(text);
                        state.analysis.file_exists.insert(file_id, true);
                        state
                            .analysis
                            .file_contents
                            .insert(file_id, Arc::clone(&text));
                        state.analysis.salsa.set_file_exists(file_id, true);
                        state.analysis.salsa.set_file_text_arc(file_id, text);
                    }
                }
                None => {
                    // Without `text`, fall back to disk when possible. Avoid overriding the in-memory
                    // overlay for open documents.
                    if !is_open {
                        state.analysis.refresh_from_disk(&uri);
                    }
                }
            }

            let canonical_uri = path.to_uri().unwrap_or(uri_string);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
            if is_open {
                state.queue_publish_diagnostics(uri);
            }
        }
        "textDocument/didClose" => {
            let Ok(params) =
                serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(params)
            else {
                return Ok(());
            };
            let (file_id, _) = state.analysis.file_id_for_uri(&params.text_document.uri);
            state.semantic_search_mark_uri_closed(&params.text_document.uri);
            let canonical_uri = VfsPath::from(&params.text_document.uri)
                .to_uri()
                .unwrap_or_else(|| params.text_document.uri.to_string());
            state.analysis.close_document(&params.text_document.uri);
            state.semantic_search_sync_file_id(file_id);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
            state.queue_clear_diagnostics(params.text_document.uri);
        }
        "workspace/didChangeWatchedFiles" => {
            let Ok(params) = serde_json::from_value::<LspDidChangeWatchedFilesParams>(params)
            else {
                return Ok(());
            };

            // `workspace/didChangeWatchedFiles` is the only reliable signal some clients provide
            // when non-Java inputs change (build files, framework config, Nova config, etc).
            //
            // Reload `nova_config` when the watched changes include the active config file. We use
            // `NOVA_CONFIG_PATH` when present (set at startup via `--config` / config discovery),
            // but also fall back to standard config filenames so creating/removing `nova.toml`
            // takes effect without requiring a server restart.
            let configured_config_path = env::var_os("NOVA_CONFIG_PATH")
                .map(PathBuf::from)
                .map(|path| path.canonicalize().unwrap_or(path));
            let mut config_changed = false;
            let legacy_config_suffix = Path::new(".nova").join("config.toml");
            let mut changed_local_paths: Vec<PathBuf> = Vec::new();

            for change in params.changes {
                let uri = change.uri;
                let vfs_path = VfsPath::from(&uri);
                let local_path = vfs_path.as_local_path().map(|p| p.to_path_buf());
                if let Some(path) = &local_path {
                    changed_local_paths.push(path.clone());
                }

                if !config_changed {
                    let is_standard_config_name = local_path
                        .as_ref()
                        .and_then(|path| path.file_name().and_then(|name| name.to_str()))
                        .is_some_and(|name| matches!(name, "nova.toml" | ".nova.toml" | "nova.config.toml"));

                    let is_legacy_config_path = local_path
                        .as_ref()
                        .is_some_and(|path| path.ends_with(&legacy_config_suffix));

                    let matches_configured_path = match (&configured_config_path, &local_path) {
                        (Some(configured), Some(path)) => {
                            path == configured
                                || path.canonicalize().ok().is_some_and(|resolved| {
                                    resolved.as_path() == configured.as_path()
                                })
                        }
                        _ => false,
                    };

                    if matches_configured_path || is_standard_config_name || is_legacy_config_path {
                        config_changed = true;
                    }
                }

                if state.analysis.vfs.overlay().is_open(&vfs_path) {
                    continue;
                }

                let (file_id, _) = state.analysis.file_id_for_uri(&uri);
                let distributed_update = match change.typ {
                    LspFileChangeType::CREATED | LspFileChangeType::CHANGED => {
                        state.analysis.refresh_from_disk(&uri);
                        state.semantic_search_sync_file_id(file_id);
                        match local_path {
                            Some(path) => {
                                let is_java = path
                                    .extension()
                                    .and_then(|ext| ext.to_str())
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("java"));
                                if !is_java {
                                    None
                                } else {
                                    state
                                        .analysis
                                        .file_contents
                                        .get(&file_id)
                                        .map(|text| (path, text.as_str().to_owned()))
                                }
                            }
                            None => None,
                        }
                    }
                    LspFileChangeType::DELETED => {
                        state.analysis.mark_missing(&uri);
                        state.semantic_search_sync_file_id(file_id);
                        match local_path {
                            Some(path) => {
                                let is_java = path
                                    .extension()
                                    .and_then(|ext| ext.to_str())
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("java"));
                                if is_java {
                                    Some((path, String::new()))
                                } else {
                                    None
                                }
                            }
                            None => None,
                        }
                    }
                    _ => None,
                };

                if let Some((path, text)) = distributed_update {
                    if let Some(dist) = state.distributed.as_ref() {
                        if dist.contains_path(&path) {
                            let frontend = Arc::clone(&dist.frontend);
                            let _ = dist.runtime.spawn(async move {
                                if let Err(err) = frontend.did_change_file(path, text).await {
                                    tracing::warn!(
                                        target = "nova.lsp",
                                        error = ?err,
                                        "distributed router update failed for didChangeWatchedFiles"
                                    );
                                }
                            });
                        }
                    }
                }
            }

            if !changed_local_paths.is_empty() {
                nova_lsp::extensions::build::invalidate_bazel_workspaces(&changed_local_paths);
            }

            if config_changed {
                match crate::stdio_config::reload_config_best_effort(state.project_root.as_deref()) {
                    Ok(config) => {
                        // Keep the AI subsystem in sync with config reloads.
                        if let Err(err) = apply_reloaded_config(state, config) {
                            tracing::warn!(target = "nova.lsp", "failed to apply config reload: {err}");
                        }
                        // Best-effort: extensions configuration is sourced from `nova_config`, so keep
                        // the registry in sync when users edit `nova.toml`.
                        state.load_extensions();
                        // JDK resolution reads `nova_config` (e.g. `[jdk].home`). Clear the cached
                        // index so changes take effect without requiring a restart.
                        state.jdk_index = None;
                    }
                    Err(err) => {
                        tracing::warn!(target = "nova.lsp", "failed to reload config: {err}");
                    }
                }
            }
        }
        "workspace/didChangeWorkspaceFolders" => {
            let Ok(params) =
                serde_json::from_value::<lsp_types::DidChangeWorkspaceFoldersParams>(params)
            else {
                return Ok(());
            };

            let previous_root = state.project_root.clone();

            // LSP sends a delta. Today we treat the first added workspace folder as the new
            // active project root.
            let new_root = params
                .event
                .added
                .iter()
                .filter_map(|folder| path_from_uri(folder.uri.as_str()))
                .next();

            let mut next_root = previous_root.clone();
            if let Some(root) = new_root {
                next_root = Some(root);
            } else if let Some(current_root) = previous_root.as_ref() {
                // Best-effort: if the current root was removed and there are no added folders,
                // clear it so subsequent requests fail with a clear "missing project root" error
                // instead of using a stale workspace.
                let removed_current = params
                    .event
                    .removed
                    .iter()
                    .filter_map(|folder| path_from_uri(folder.uri.as_str()))
                    .any(|path| path == *current_root);
                if removed_current {
                    next_root = None;
                }
            }

            if next_root != previous_root {
                state.cancel_semantic_search_workspace_indexing();
                state.reset_semantic_search_workspace_index_status();
                state.clear_semantic_search_index();

                state.project_root = next_root;
                state.workspace = None;
                state.load_extensions();

                if state.project_root.is_some() {
                    state.start_semantic_search_workspace_indexing();
                }
            }
        }
        "workspace/didChangeConfiguration" => {
            let Ok(_params) =
                serde_json::from_value::<lsp_types::DidChangeConfigurationParams>(params)
            else {
                return Ok(());
            };

            match crate::stdio_config::reload_config_best_effort(state.project_root.as_deref()) {
                Ok(config) => {
                    // Keep the AI subsystem in sync with config reloads.
                    if let Err(err) = apply_reloaded_config(state, config) {
                        tracing::warn!(target = "nova.lsp", "failed to apply config reload: {err}");
                    }
                    // Best-effort: extensions configuration is sourced from `nova_config`, so keep
                    // the registry in sync when users toggle settings.
                    state.load_extensions();
                    state.jdk_index = None;
                }
                Err(err) => {
                    tracing::warn!(target = "nova.lsp", "failed to reload config: {err}");
                }
            }
        }
        "workspace/didCreateFiles" => {
            let Ok(params) = serde_json::from_value::<lsp_types::CreateFilesParams>(params) else {
                return Ok(());
            };

            for file in params.files {
                let Ok(uri) = file.uri.parse::<LspUri>() else {
                    continue;
                };
                let path = VfsPath::from(&uri);
                if state.analysis.vfs.overlay().is_open(&path) {
                    continue;
                }
                state.analysis.refresh_from_disk(&uri);
            }
        }
        "workspace/didDeleteFiles" => {
            let Ok(params) = serde_json::from_value::<lsp_types::DeleteFilesParams>(params) else {
                return Ok(());
            };

            for file in params.files {
                let Ok(uri) = file.uri.parse::<LspUri>() else {
                    continue;
                };
                state.semantic_search_remove_uri(&uri);

                let path = VfsPath::from(&uri);
                if state.analysis.vfs.overlay().is_open(&path) {
                    continue;
                }

                state.analysis.mark_missing(&uri);
            }
        }
        "workspace/didRenameFiles" => {
            let Ok(params) = serde_json::from_value::<lsp_types::RenameFilesParams>(params) else {
                return Ok(());
            };

            for file in params.files {
                let (Ok(old_uri), Ok(new_uri)) = (file.old_uri.parse::<LspUri>(), file.new_uri.parse::<LspUri>()) else {
                    continue;
                };
                state.semantic_search_remove_uri(&old_uri);
                state.semantic_search_mark_uri_closed(&old_uri);
                let file_id = state.analysis.rename_uri(&old_uri, &new_uri);
                let to_path = VfsPath::from(&new_uri);
                if !state.analysis.vfs.overlay().is_open(&to_path) {
                    state.analysis.refresh_from_disk(&new_uri);
                    state.semantic_search_sync_file_id(file_id);
                } else {
                    // Rename of an open document: update the semantic search path key.
                    state.semantic_search_mark_file_open(file_id);
                    state.semantic_search_index_open_document(file_id);
                }
            }
        }
        nova_lsp::WORKSPACE_RENAME_PATH_NOTIFICATION => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct RenamePathParams {
                from: LspUri,
                to: LspUri,
            }

            let Ok(params) = serde_json::from_value::<RenamePathParams>(params) else {
                return Ok(());
            };

            // If the source buffer is open, treat the rename as a pure path move; the in-memory
            // overlay remains the source of truth.
            state.semantic_search_remove_uri(&params.from);
            state.semantic_search_mark_uri_closed(&params.from);
            let file_id = state.analysis.rename_uri(&params.from, &params.to);
            let to_path = VfsPath::from(&params.to);
            if !state.analysis.vfs.overlay().is_open(&to_path) {
                state.analysis.refresh_from_disk(&params.to);
                state.semantic_search_sync_file_id(file_id);
            } else {
                state.semantic_search_mark_file_open(file_id);
                state.semantic_search_index_open_document(file_id);
            }
        }
        _ => {}
    }
    Ok(())
}

fn apply_reloaded_config(
    state: &mut ServerState,
    mut config: nova_config::NovaConfig,
) -> Result<(), String> {
    let (ai_env, privacy_override) = match crate::stdio_ai::load_ai_config_from_env() {
        Ok(value) => match value {
            Some((ai, privacy)) => (Some(ai), Some(privacy)),
            None => (None, None),
        },
        Err(err) => {
            tracing::warn!(target = "nova.lsp", "failed to configure AI from env: {err}");
            (None, None)
        }
    };
    if let Some(ai) = ai_env {
        config.ai = ai;
    }

    // Match startup behavior: audit logging forces AI enabled so the audit channel is active.
    let audit_logging = matches!(
        std::env::var("NOVA_AI_AUDIT_LOGGING").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    if audit_logging {
        config.ai.enabled = true;
        config.ai.audit_log.enabled = true;
    }

    // ---------------------------------------------------------------------
    // Server-side AI overrides (privacy / cost controls)
    // ---------------------------------------------------------------------
    let disable_ai = matches!(
        std::env::var("NOVA_DISABLE_AI").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let disable_ai_completions = matches!(
        std::env::var("NOVA_DISABLE_AI_COMPLETIONS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let disable_ai_code_actions = matches!(
        std::env::var("NOVA_DISABLE_AI_CODE_ACTIONS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let disable_ai_code_review = matches!(
        std::env::var("NOVA_DISABLE_AI_CODE_REVIEW").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    if disable_ai {
        config.ai.enabled = false;
        config.ai.features.completion_ranking = false;
        config.ai.features.semantic_search = false;
        config.ai.features.multi_token_completion = false;
        config.ai.features.explain_errors = false;
        config.ai.features.code_actions = false;
        config.ai.features.code_review = false;
    } else if disable_ai_completions {
        config.ai.features.completion_ranking = false;
        config.ai.features.multi_token_completion = false;
    }
    if disable_ai_code_actions {
        config.ai.features.explain_errors = false;
        config.ai.features.code_actions = false;
    }
    if disable_ai_code_review {
        config.ai.features.code_review = false;
    }

    // Swap in the newly-loaded config.
    let new_ai_config = config.ai.clone();
    state.config = Arc::new(config);
    apply_ai_config_update(state, new_ai_config, privacy_override);
    Ok(())
}

fn apply_ai_config_update(
    state: &mut ServerState,
    new_ai_config: nova_config::AiConfig,
    privacy_override: Option<nova_ai::PrivacyMode>,
) {
    // Cancel any in-flight workspace indexing before we potentially drop the Tokio runtime.
    state.semantic_search_workspace_index_cancel.cancel();

    let prev_ai_config = state.ai_config.clone();
    let enabled_toggled = prev_ai_config.enabled != new_ai_config.enabled;
    let provider_settings_changed = prev_ai_config.provider != new_ai_config.provider
        || prev_ai_config.api_key != new_ai_config.api_key
        || prev_ai_config.privacy != new_ai_config.privacy
        || prev_ai_config.audit_log != new_ai_config.audit_log
        || prev_ai_config.features.code_review_max_diff_chars
            != new_ai_config.features.code_review_max_diff_chars
        || prev_ai_config.cache_enabled != new_ai_config.cache_enabled
        || prev_ai_config.cache_max_entries != new_ai_config.cache_max_entries
        || prev_ai_config.cache_ttl_secs != new_ai_config.cache_ttl_secs;
    let needs_ai_reinit = enabled_toggled || provider_settings_changed;

    // Apply the config + privacy settings first so path-exclusion checks use the updated policy
    // even if provider initialization fails.
    state.ai_config = new_ai_config;
    state.privacy = privacy_override.unwrap_or_else(|| {
        nova_ai::PrivacyMode::from_ai_privacy_config(&state.ai_config.privacy)
    });
    state.ai_privacy_excluded_matcher =
        Arc::new(ExcludedPathMatcher::from_config(&state.ai_config.privacy));

    // Drop / rebuild the AI provider + runtime as needed.
    if !state.ai_config.enabled {
        state.ai = None;
        state.runtime = None;
    } else if needs_ai_reinit || state.ai.is_none() || state.runtime.is_none() {
        // Ensure the old runtime is dropped before creating a new one.
        state.ai = None;
        state.runtime = None;

        match NovaAi::new(&state.ai_config) {
            Ok(ai) => {
                let worker_threads = state
                    .ai_config
                    .provider
                    .effective_concurrency()
                    .clamp(1, 4);
                match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(worker_threads)
                    .max_blocking_threads(worker_threads)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        state.ai = Some(ai);
                        state.runtime = Some(runtime);
                    }
                    Err(err) => {
                        tracing::warn!(
                            target = "nova.lsp",
                            "failed to build Tokio runtime for AI tasks: {err}"
                        );
                        state.ai = None;
                        state.runtime = None;
                    }
                }
            }
            Err(err) => {
                tracing::warn!(target = "nova.lsp", "failed to configure AI: {err}");
                state.ai = None;
                state.runtime = None;
            }
        }
    }

    // Update completion engine state to reflect the new config/env overrides.
    #[cfg(feature = "ai")]
    {
        state.completion_service = build_completion_service(&state.ai_config, &state.privacy, state.runtime.is_some());
    }

    // Update semantic search engine + workspace indexing state.
    state.semantic_search_workspace_index_cancel = CancellationToken::new();
    state.semantic_search_workspace_index_status.reset();
    {
        let mut search = state
            .semantic_search
            .write()
            .unwrap_or_else(|err| err.into_inner());
        *search = nova_ai::semantic_search_from_config(&state.ai_config).unwrap_or_else(|err| {
            tracing::warn!(target = "nova.lsp", "failed to configure semantic search: {err}");
            Box::new(nova_ai::TrigramSemanticSearch::new())
        });
        search.clear();
    }

    // Restart workspace indexing if enabled and we have a usable workspace root.
    state.start_semantic_search_workspace_indexing();

    // Best-effort: ensure open documents are indexed into the new semantic-search implementation.
    for file_id in state.analysis.vfs.open_documents().snapshot() {
        state.semantic_search_index_open_document(file_id);
    }
}

#[cfg(feature = "ai")]
fn build_completion_service(
    ai_config: &nova_config::AiConfig,
    privacy: &nova_ai::PrivacyMode,
    runtime_available: bool,
) -> nova_lsp::NovaCompletionService {
    let ai_max_items_override = match std::env::var("NOVA_AI_COMPLETIONS_MAX_ITEMS") {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                match trimmed.parse::<usize>() {
                    Ok(max_items) => Some(max_items.min(32)),
                    Err(_) => {
                        tracing::warn!(
                            target = "nova.lsp",
                            "invalid NOVA_AI_COMPLETIONS_MAX_ITEMS={value:?}; expected a non-negative integer"
                        );
                        None
                    }
                }
            }
        }
        Err(_) => None,
    };

    let multi_token_enabled = runtime_available && ai_config.enabled && ai_config.features.multi_token_completion;
    // `nova.aiCompletions.maxItems` is surfaced to the server via `NOVA_AI_COMPLETIONS_MAX_ITEMS`.
    // Treat `0` as a hard disable so the server doesn't spawn background AI completion tasks or
    // mark results as `is_incomplete`.
    let multi_token_enabled = multi_token_enabled && ai_max_items_override.unwrap_or(1) > 0;

    let ai_provider = if multi_token_enabled {
        match AiClient::from_config(ai_config) {
            Ok(client) => {
                let provider: Arc<dyn MultiTokenCompletionProvider> = Arc::new(
                    CloudMultiTokenCompletionProvider::new(Arc::new(client))
                        .with_privacy_mode(privacy.clone()),
                );
                Some(provider)
            }
            Err(err) => {
                tracing::warn!(target = "nova.lsp", "failed to configure AI completions: {err}");
                None
            }
        }
    } else {
        None
    };

    let mut completion_config = CompletionConfig::default();
    completion_config.ai_enabled = multi_token_enabled;
    if let Some(max_items) = ai_max_items_override {
        completion_config.ai_max_items = max_items;
    }
    completion_config.ai_timeout_ms = ai_config.timeouts.multi_token_completion_ms.max(1);

    let engine = CompletionEngine::new(
        completion_config,
        CompletionContextBuilder::new(10_000),
        ai_provider,
    );
    nova_lsp::NovaCompletionService::with_config(
        engine,
        nova_lsp::CompletionMoreConfig::from_provider_config(&ai_config.provider),
    )
}
