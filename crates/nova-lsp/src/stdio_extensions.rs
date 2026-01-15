use crate::stdio_paths::path_from_uri;
use crate::stdio_extensions_db::SingleFileDb;
use crate::stdio_text::offset_to_position_utf16;
use crate::ServerState;

use nova_db::Database;
use nova_ext::{ExtensionManager, ExtensionRegistry};
use nova_ide::extensions::IdeExtensions;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn extensions_status_json(state: &ServerState) -> serde_json::Value {
    let loaded = state
        .loaded_extensions
        .iter()
        .map(|ext| {
            let capabilities: Vec<&'static str> =
                ext.capabilities.iter().map(|cap| cap.as_str()).collect();
            json!({
                "id": ext.id.clone(),
                "version": ext.version.to_string(),
                "dir": ext.dir.display().to_string(),
                "name": ext.name.clone(),
                "description": ext.description.clone(),
                "authors": ext.authors.clone(),
                "homepage": ext.homepage.clone(),
                "license": ext.license.clone(),
                "abiVersion": ext.abi_version,
                "capabilities": capabilities,
            })
        })
        .collect::<Vec<_>>();

    fn provider_stats_map_json(
        map: &BTreeMap<String, nova_ext::ProviderStats>,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        for (id, stats) in map {
            let last_error = stats.last_error.map(|err| match err {
                nova_ext::ProviderLastError::Timeout => "timeout",
                nova_ext::ProviderLastError::PanicTrap => "panic_trap",
                nova_ext::ProviderLastError::InvalidResponse => "invalid_response",
            });
            out.insert(
                id.clone(),
                json!({
                    "callsTotal": stats.calls_total,
                    "timeoutsTotal": stats.timeouts_total,
                    "panicsTotal": stats.panics_total,
                    "invalidResponsesTotal": stats.invalid_responses_total,
                    "skippedTotal": stats.skipped_total,
                    "circuitOpenedTotal": stats.circuit_opened_total,
                    "consecutiveFailures": stats.consecutive_failures,
                    "circuitOpen": stats.circuit_open_until.is_some(),
                    "lastError": last_error,
                    "lastDurationMs": stats.last_duration.map(|d| d.as_millis() as u64),
                }),
            );
        }
        serde_json::Value::Object(out)
    }

    let stats = state.extensions_registry.stats();

    json!({
        "schemaVersion": nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION,
        "enabled": state.config.extensions.enabled,
        "wasmPaths": state.config.extensions.wasm_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "allow": state.config.extensions.allow.clone(),
        "deny": state.config.extensions.deny.clone(),
        "loadedExtensions": loaded,
        "loadErrors": state.extension_load_errors.clone(),
        "registerErrors": state.extension_register_errors.clone(),
        "stats": {
            "diagnostic": provider_stats_map_json(&stats.diagnostic),
            "completion": provider_stats_map_json(&stats.completion),
            "codeAction": provider_stats_map_json(&stats.code_action),
            "navigation": provider_stats_map_json(&stats.navigation),
            "inlayHint": provider_stats_map_json(&stats.inlay_hint),
        }
    })
}

pub(super) fn handle_extensions_navigation(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ExtensionsNavigationParams {
        #[serde(default)]
        schema_version: Option<u32>,
        text_document: lsp_types::TextDocumentIdentifier,
    }

    let params: ExtensionsNavigationParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    if let Some(version) = params.schema_version {
        if version != nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported schemaVersion {version} (expected {})",
                nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION
            ));
        }
    }

    let uri = params.text_document.uri;
    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!({
            "schemaVersion": nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION,
            "targets": []
        }));
    }

    let text = state.analysis.file_content(file_id).to_string();
    let path = state
        .analysis
        .file_path(file_id)
        .map(|p| p.to_path_buf())
        .or_else(|| path_from_uri(uri.as_str()));
    let ext_db = Arc::new(SingleFileDb::new(file_id, path, text.clone()));
    let ide_extensions = IdeExtensions::with_registry(
        ext_db,
        Arc::clone(&state.config),
        nova_ext::ProjectId::new(0),
        state.extensions_registry.clone(),
    );

    let targets = ide_extensions
        .navigation(cancel, nova_ext::Symbol::File(file_id))
        .into_iter()
        .filter_map(|target| {
            if target.file != file_id {
                return None;
            }
            let range = target.span.map(|span| lsp_types::Range {
                start: offset_to_position_utf16(&text, span.start),
                end: offset_to_position_utf16(&text, span.end),
            });
            Some(json!({
                "label": target.label,
                "uri": uri.as_str(),
                "fileId": target.file.to_raw(),
                "range": range,
                "span": target.span.map(|span| json!({ "start": span.start, "end": span.end })),
            }))
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "schemaVersion": nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION,
        "targets": targets,
    }))
}

impl ServerState {
    pub(super) fn load_extensions(&mut self) {
        self.extensions_registry = ExtensionRegistry::default();
        self.loaded_extensions.clear();
        self.extension_load_errors.clear();
        self.extension_register_errors.clear();

        if !self.config.extensions.enabled {
            tracing::debug!(target = "nova.lsp", "extensions disabled via config");
            return;
        }

        if self.config.extensions.wasm_paths.is_empty() {
            tracing::debug!(
                target = "nova.lsp",
                "no wasm_paths configured; skipping extension load"
            );
            return;
        }

        let base_dir = self.project_root.clone().or_else(|| env::current_dir().ok());
        let search_paths: Vec<PathBuf> = self
            .config
            .extensions
            .wasm_paths
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else if let Some(base) = base_dir.as_ref() {
                    base.join(path)
                } else {
                    path.clone()
                }
            })
            .collect();

        let (loaded, load_errors) = ExtensionManager::load_all_filtered(
            &search_paths,
            self.config.extensions.allow.as_deref(),
            &self.config.extensions.deny,
        );
        self.extension_load_errors = load_errors.iter().map(|err| err.to_string()).collect();
        for err in &load_errors {
            tracing::warn!(target = "nova.lsp", error = %err, "failed to load extension bundle");
        }

        #[cfg(feature = "wasm-extensions")]
        {
            let mut registry = ExtensionRegistry::<SingleFileDb>::default();
            let register_report = ExtensionManager::register_all_best_effort(&mut registry, &loaded);
            self.extension_register_errors = register_report
                .errors
                .iter()
                .map(|failure| failure.error.to_string())
                .collect();
            for failure in &register_report.errors {
                tracing::warn!(
                    target = "nova.lsp",
                    extension_id = %failure.extension.id,
                    error = %failure.error,
                    "failed to register extension provider"
                );
            }
            self.loaded_extensions = register_report.registered;

            self.extensions_registry = registry;

            tracing::info!(
                target = "nova.lsp",
                loaded = self.loaded_extensions.len(),
                "loaded wasm extensions"
            );
        }

        #[cfg(not(feature = "wasm-extensions"))]
        {
            // We can still read extension bundle manifests, but without the Wasmtime-based
            // runtime we cannot register any providers.
            self.loaded_extensions = ExtensionManager::list(&loaded);
            self.extension_register_errors = if self.loaded_extensions.is_empty() {
                Vec::new()
            } else {
                vec![format!(
                    "nova-lsp was built without WASM extension support; rebuild with `--features wasm-extensions` to enable loading {} extension(s)",
                    self.loaded_extensions.len()
                )]
            };
            for error in &self.extension_register_errors {
                tracing::warn!(target = "nova.lsp", error = %error, "extensions not registered");
            }
            tracing::info!(
                target = "nova.lsp",
                loaded = self.loaded_extensions.len(),
                "loaded extension bundle metadata (WASM runtime disabled)"
            );
        }
    }
}

