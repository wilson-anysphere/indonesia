use crate::ServerState;

use lsp_types::Uri as LspUri;
use nova_db::FileId as DbFileId;
use nova_metrics::MetricsRegistry;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const SEMANTIC_SEARCH_WORKSPACE_INDEX_METRIC: &str = "lsp/semantic_search/workspace_index";
const SEMANTIC_SEARCH_WORKSPACE_INDEX_FILE_METRIC: &str =
    "lsp/semantic_search/workspace_index/file";
const SEMANTIC_SEARCH_WORKSPACE_INDEX_SKIPPED_SAFE_MODE_METRIC: &str =
    "lsp/semantic_search/workspace_index/skipped_safe_mode";
const SEMANTIC_SEARCH_WORKSPACE_INDEX_SKIPPED_MISSING_ROOT_METRIC: &str =
    "lsp/semantic_search/workspace_index/skipped_missing_workspace_root";
const SEMANTIC_SEARCH_WORKSPACE_INDEX_SKIPPED_RUNTIME_UNAVAILABLE_METRIC: &str =
    "lsp/semantic_search/workspace_index/skipped_runtime_unavailable";

#[derive(Debug, Default)]
pub(super) struct SemanticSearchWorkspaceIndexStatus {
    current_run_id: AtomicU64,
    completed_run_id: AtomicU64,
    indexed_files: AtomicU64,
    indexed_bytes: AtomicU64,
}

impl SemanticSearchWorkspaceIndexStatus {
    pub(super) fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.current_run_id.load(Ordering::SeqCst),
            self.completed_run_id.load(Ordering::SeqCst),
            self.indexed_files.load(Ordering::SeqCst),
            self.indexed_bytes.load(Ordering::SeqCst),
        )
    }

    pub(super) fn reset(&self) {
        self.current_run_id.store(0, Ordering::SeqCst);
        self.completed_run_id.store(0, Ordering::SeqCst);
        self.indexed_files.store(0, Ordering::SeqCst);
        self.indexed_bytes.store(0, Ordering::SeqCst);
    }
}

impl ServerState {
    pub(super) fn semantic_search_enabled(&self) -> bool {
        self.ai_config.enabled && self.ai_config.features.semantic_search
    }

    pub(super) fn semantic_search_extension_allowed(path: &Path) -> bool {
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            return false;
        };

        ext.eq_ignore_ascii_case("java")
            || ext.eq_ignore_ascii_case("kt")
            || ext.eq_ignore_ascii_case("kts")
            || ext.eq_ignore_ascii_case("gradle")
            || ext.eq_ignore_ascii_case("md")
    }

    pub(super) fn semantic_search_is_excluded(&self, path: &Path) -> bool {
        // Keep semantic search consistent with LLM privacy filtering. In particular, this ensures
        // that any file excluded from AI prompts is also excluded from the semantic-search index
        // (which is later used to construct AI context).
        crate::stdio_ai::is_ai_excluded_path(self, path)
    }

    pub(super) fn semantic_search_should_index_path(&self, path: &Path) -> bool {
        Self::semantic_search_extension_allowed(path) && !self.semantic_search_is_excluded(path)
    }

    pub(super) fn semantic_search_mark_file_open(&mut self, file_id: DbFileId) {
        if !self.semantic_search_enabled() {
            return;
        }

        let Some(path) = self.analysis.file_paths.get(&file_id).cloned() else {
            return;
        };
        self.semantic_search_open_files
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(path);
    }

    pub(super) fn semantic_search_mark_uri_closed(&mut self, uri: &LspUri) {
        if !self.semantic_search_enabled() {
            return;
        }

        let path = self.analysis.path_for_uri(uri);
        let Some(local) = path.as_local_path() else {
            return;
        };

        self.semantic_search_open_files
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(local);
    }

    pub(super) fn semantic_search_index_open_document(&mut self, file_id: DbFileId) {
        if !self.semantic_search_enabled() {
            return;
        }

        let Some(path) = self.analysis.file_paths.get(&file_id).cloned() else {
            return;
        };
        if !self.semantic_search_should_index_path(&path) {
            let mut search = self
                .semantic_search
                .write()
                .unwrap_or_else(|err| err.into_inner());
            search.remove_file(&path);
            return;
        }
        let Some(text) = self.analysis.file_contents.get(&file_id) else {
            return;
        };

        let mut search = self
            .semantic_search
            .write()
            .unwrap_or_else(|err| err.into_inner());
        search.index_file(path, text.as_str().to_owned());
    }

    pub(super) fn semantic_search_sync_file_id(&mut self, file_id: DbFileId) {
        if !self.semantic_search_enabled() {
            return;
        }

        let Some(path) = self.analysis.file_paths.get(&file_id).cloned() else {
            return;
        };

        if !self.semantic_search_should_index_path(&path) || !self.analysis.exists(file_id) {
            let mut search = self
                .semantic_search
                .write()
                .unwrap_or_else(|err| err.into_inner());
            search.remove_file(&path);
            return;
        }

        let Some(text) = self.analysis.file_contents.get(&file_id) else {
            return;
        };

        let mut search = self
            .semantic_search
            .write()
            .unwrap_or_else(|err| err.into_inner());
        search.index_file(path, text.as_str().to_owned());
    }

    pub(super) fn semantic_search_remove_uri(&mut self, uri: &LspUri) {
        if !self.semantic_search_enabled() {
            return;
        }

        let path = self.analysis.path_for_uri(uri);
        let Some(local) = path.as_local_path() else {
            return;
        };

        let mut search = self
            .semantic_search
            .write()
            .unwrap_or_else(|err| err.into_inner());
        search.remove_file(local);
    }

    pub(super) fn cancel_semantic_search_workspace_indexing(&mut self) {
        if !self.semantic_search_enabled() {
            return;
        }

        self.semantic_search_workspace_index_cancel.cancel();
    }

    pub(super) fn reset_semantic_search_workspace_index_status(&mut self) {
        if !self.semantic_search_enabled() {
            return;
        }

        self.semantic_search_workspace_index_status
            .current_run_id
            .store(0, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .completed_run_id
            .store(0, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .indexed_files
            .store(0, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .indexed_bytes
            .store(0, Ordering::SeqCst);
    }

    pub(super) fn clear_semantic_search_index(&mut self) {
        if !self.semantic_search_enabled() {
            return;
        }

        // Refresh the open-file set to reflect current overlay state.
        {
            let mut open = self
                .semantic_search_open_files
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            open.clear();
            for file_id in self.analysis.vfs.open_documents().snapshot() {
                if let Some(path) = self.analysis.file_paths.get(&file_id) {
                    open.insert(path.clone());
                }
            }
        }

        {
            let mut search = self
                .semantic_search
                .write()
                .unwrap_or_else(|err| err.into_inner());
            search.clear();
        }

        // Preserve open-document overlays after clearing.
        for file_id in self.analysis.vfs.open_documents().snapshot() {
            self.semantic_search_index_open_document(file_id);
        }
    }

    pub(super) fn semantic_search_workspace_index_status_json(&self) -> serde_json::Value {
        let (current, completed, files, bytes) =
            self.semantic_search_workspace_index_status.snapshot();
        let done = current != 0 && current == completed;

        let enabled = self.semantic_search_enabled();
        let reason = if !enabled {
            Some("disabled")
        } else if current == 0 {
            // Mirror the gating logic of `start_semantic_search_workspace_indexing` so callers can
            // understand why workspace indexing has not started.
            let (safe_mode, _) = nova_lsp::hardening::safe_mode_snapshot();
            if safe_mode {
                Some("safe_mode")
            } else if self.project_root.as_ref().is_none_or(|root| !root.is_dir()) {
                Some("missing_workspace_root")
            } else if self.runtime.is_none() {
                Some("runtime_unavailable")
            } else {
                None
            }
        } else {
            None
        };

        let mut value = json!({
            "currentRunId": current,
            "completedRunId": completed,
            "done": done,
            "indexedFiles": files,
            "indexedBytes": bytes,
            "enabled": enabled,
        });

        if let (Some(reason), serde_json::Value::Object(obj)) = (reason, &mut value) {
            obj.insert("reason".to_string(), serde_json::Value::String(reason.to_string()));
        }

        value
    }

    pub(super) fn start_semantic_search_workspace_indexing(&mut self) {
        if !self.semantic_search_enabled() {
            return;
        }

        let metrics = MetricsRegistry::global();

        let (safe_mode, _) = nova_lsp::hardening::safe_mode_snapshot();
        if safe_mode {
            metrics.record_request(
                SEMANTIC_SEARCH_WORKSPACE_INDEX_SKIPPED_SAFE_MODE_METRIC,
                Duration::ZERO,
            );
            return;
        }

        let Some(root) = self.project_root.clone() else {
            metrics.record_request(
                SEMANTIC_SEARCH_WORKSPACE_INDEX_SKIPPED_MISSING_ROOT_METRIC,
                Duration::ZERO,
            );
            return;
        };
        if !root.is_dir() {
            metrics.record_request(
                SEMANTIC_SEARCH_WORKSPACE_INDEX_SKIPPED_MISSING_ROOT_METRIC,
                Duration::ZERO,
            );
            return;
        }

        if self.runtime.is_none() {
            // Semantic search can still work without the AI runtime, but workspace indexing is
            // intentionally best-effort. Callers without a runtime (e.g. AI misconfigured) will
            // fall back to open-document indexing.
            metrics.record_request(
                SEMANTIC_SEARCH_WORKSPACE_INDEX_SKIPPED_RUNTIME_UNAVAILABLE_METRIC,
                Duration::ZERO,
            );
            return;
        };

        // Cancel any in-flight indexing task and start a new run.
        self.semantic_search_workspace_index_cancel.cancel();
        self.semantic_search_workspace_index_cancel = CancellationToken::new();
        self.semantic_search_workspace_index_run_id =
            self.semantic_search_workspace_index_run_id.wrapping_add(1);

        let run_id = self.semantic_search_workspace_index_run_id;
        self.semantic_search_workspace_index_status
            .current_run_id
            .store(run_id, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .completed_run_id
            .store(0, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .indexed_files
            .store(0, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .indexed_bytes
            .store(0, Ordering::SeqCst);

        {
            let mut open = self
                .semantic_search_open_files
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            open.clear();
            for file_id in self.analysis.vfs.open_documents().snapshot() {
                if let Some(path) = self.analysis.file_paths.get(&file_id) {
                    open.insert(path.clone());
                }
            }
        }

        // Clear the existing index so removed files do not linger across runs.
        {
            let mut search = self
                .semantic_search
                .write()
                .unwrap_or_else(|err| err.into_inner());
            search.clear();
        }

        // Ensure any already-open overlays remain indexed after the clear.
        for file_id in self.analysis.vfs.open_documents().snapshot() {
            self.semantic_search_index_open_document(file_id);
        }

        const MAX_INDEXED_FILES: u64 = 2_000;
        const MAX_INDEXED_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB
        const MAX_FILE_BYTES: u64 = 256 * 1024; // 256 KiB

        let semantic_search = Arc::clone(&self.semantic_search);
        let open_files = Arc::clone(&self.semantic_search_open_files);
        let excluded_matcher = Arc::clone(&self.ai_privacy_excluded_matcher);
        let status = Arc::clone(&self.semantic_search_workspace_index_status);
        let cancel = self.semantic_search_workspace_index_cancel.clone();
        let runtime = self.runtime.as_ref().expect("checked runtime");
        runtime.spawn_blocking(move || {
            let run_started_at = Instant::now();
            let mut indexed_files = 0u64;
            let mut indexed_bytes = 0u64;

            let mut walk = walkdir::WalkDir::new(&root).follow_links(false).into_iter();
            while let Some(entry) = walk.next() {
                if cancel.is_cancelled() {
                    break;
                }
                if status.current_run_id.load(Ordering::SeqCst) != run_id {
                    break;
                }

                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };

                // Skip common build/VCS output directories early.
                if entry.file_type().is_dir() {
                    let name = entry.file_name().to_string_lossy();
                    if matches!(
                        name.as_ref(),
                        ".git" | ".hg" | ".svn" | "target" | "build" | "out" | "node_modules"
                    ) {
                        walk.skip_current_dir();
                        continue;
                    }
                }

                if !entry.file_type().is_file() {
                    continue;
                }

                let path = entry.path().to_path_buf();
                if !ServerState::semantic_search_extension_allowed(&path) {
                    continue;
                }

                // Respect privacy exclusions.
                let is_excluded = match excluded_matcher.as_ref() {
                    Ok(matcher) => matcher.is_match(&path),
                    // Fail-closed: invalid privacy config means we should not index anything.
                    Err(_) => true,
                };
                if is_excluded {
                    continue;
                }

                // Avoid overwriting open-document overlays with on-disk content.
                if open_files
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .contains(&path)
                {
                    continue;
                }

                let meta_len = entry.metadata().ok().map(|m| m.len()).unwrap_or(0);
                if meta_len > MAX_FILE_BYTES {
                    continue;
                }

                if indexed_files >= MAX_INDEXED_FILES || indexed_bytes >= MAX_INDEXED_BYTES {
                    break;
                }

                let file_started_at = Instant::now();
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(_) => continue,
                };

                let len = text.len() as u64;
                if len > MAX_FILE_BYTES {
                    continue;
                }
                if indexed_files + 1 > MAX_INDEXED_FILES
                    || indexed_bytes.saturating_add(len) > MAX_INDEXED_BYTES
                {
                    break;
                }

                if cancel.is_cancelled() {
                    break;
                }
                if status.current_run_id.load(Ordering::SeqCst) != run_id {
                    break;
                }

                // Re-check open-document overlays: the file may have been opened after we started
                // reading it. Skip indexing to avoid overwriting the in-memory version.
                if open_files
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .contains(&path)
                {
                    continue;
                }

                {
                    let mut search = semantic_search
                        .write()
                        .unwrap_or_else(|err| err.into_inner());
                    if cancel.is_cancelled() {
                        break;
                    }
                    if status.current_run_id.load(Ordering::SeqCst) != run_id {
                        break;
                    }
                    search.index_file(path, text);
                }

                if cancel.is_cancelled() {
                    break;
                }
                if status.current_run_id.load(Ordering::SeqCst) != run_id {
                    break;
                }

                indexed_files += 1;
                indexed_bytes = indexed_bytes.saturating_add(len);
                status.indexed_files.store(indexed_files, Ordering::SeqCst);
                status.indexed_bytes.store(indexed_bytes, Ordering::SeqCst);
                MetricsRegistry::global().record_request(
                    SEMANTIC_SEARCH_WORKSPACE_INDEX_FILE_METRIC,
                    file_started_at.elapsed(),
                );
            }

            // Avoid races with reindexing: if a newer run has been started, do not overwrite the
            // `completedRunId` for the new run.
            if status.current_run_id.load(Ordering::SeqCst) == run_id {
                if !cancel.is_cancelled() {
                    let search = semantic_search
                        .write()
                        .unwrap_or_else(|err| err.into_inner());
                    search.finalize_indexing();
                }
                MetricsRegistry::global().record_request(
                    SEMANTIC_SEARCH_WORKSPACE_INDEX_METRIC,
                    run_started_at.elapsed(),
                );
                status.completed_run_id.store(run_id, Ordering::SeqCst);
            }
        });
    }
}
