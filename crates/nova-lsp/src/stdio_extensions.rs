use crate::stdio_extensions_db::SingleFileDb;
use crate::stdio_paths::path_from_uri;
use crate::stdio_text::offset_to_position_utf16;
use crate::ServerState;

use nova_db::Database;
use nova_ext::{ExtensionManager, ExtensionRegistry};
use nova_ide::extensions::IdeExtensions;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn extensions_status_json(state: &ServerState) -> serde_json::Value {
    fn provider_stats_map(map: &BTreeMap<String, nova_ext::ProviderStats>) -> Value {
        Value::Object(
            map.iter()
                .map(|(id, stats)| {
                    let last_error = stats.last_error.map(|err| match err {
                        nova_ext::ProviderLastError::Timeout => "timeout",
                        nova_ext::ProviderLastError::PanicTrap => "panic_trap",
                        nova_ext::ProviderLastError::InvalidResponse => "invalid_response",
                    });

                    let mut out = serde_json::Map::new();
                    out.insert(
                        "callsTotal".to_string(),
                        Value::from(stats.calls_total as u64),
                    );
                    out.insert(
                        "timeoutsTotal".to_string(),
                        Value::from(stats.timeouts_total as u64),
                    );
                    out.insert(
                        "panicsTotal".to_string(),
                        Value::from(stats.panics_total as u64),
                    );
                    out.insert(
                        "invalidResponsesTotal".to_string(),
                        Value::from(stats.invalid_responses_total as u64),
                    );
                    out.insert(
                        "skippedTotal".to_string(),
                        Value::from(stats.skipped_total as u64),
                    );
                    out.insert(
                        "circuitOpenedTotal".to_string(),
                        Value::from(stats.circuit_opened_total as u64),
                    );
                    out.insert(
                        "consecutiveFailures".to_string(),
                        Value::from(stats.consecutive_failures),
                    );
                    out.insert(
                        "circuitOpen".to_string(),
                        Value::Bool(stats.circuit_open_until.is_some()),
                    );
                    out.insert(
                        "lastError".to_string(),
                        last_error.map_or(Value::Null, |s| Value::String(s.to_string())),
                    );
                    out.insert(
                        "lastDurationMs".to_string(),
                        stats
                            .last_duration
                            .map_or(Value::Null, |d| Value::from(d.as_millis() as u64)),
                    );

                    (id.clone(), Value::Object(out))
                })
                .collect(),
        )
    }

    let stats = state.extensions_registry.stats();

    let mut status = serde_json::Map::new();
    status.insert(
        "schemaVersion".to_string(),
        Value::from(nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION),
    );
    status.insert(
        "enabled".to_string(),
        Value::Bool(state.config.extensions.enabled),
    );
    status.insert(
        "wasmPaths".to_string(),
        Value::Array(
            state
                .config
                .extensions
                .wasm_paths
                .iter()
                .map(|p| Value::String(p.display().to_string()))
                .collect(),
        ),
    );
    status.insert(
        "allow".to_string(),
        state
            .config
            .extensions
            .allow
            .as_ref()
            .map_or(Value::Null, |allow| {
                Value::Array(allow.iter().cloned().map(Value::String).collect())
            }),
    );
    status.insert(
        "deny".to_string(),
        Value::Array(
            state
                .config
                .extensions
                .deny
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    status.insert(
        "loadedExtensions".to_string(),
        Value::Array(
            state
                .loaded_extensions
                .iter()
                .map(|ext| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("id".to_string(), Value::String(ext.id.clone()));
                    obj.insert(
                        "version".to_string(),
                        Value::String(ext.version.to_string()),
                    );
                    obj.insert(
                        "dir".to_string(),
                        Value::String(ext.dir.display().to_string()),
                    );
                    obj.insert(
                        "name".to_string(),
                        ext.name.clone().map_or(Value::Null, Value::String),
                    );
                    obj.insert(
                        "description".to_string(),
                        ext.description.clone().map_or(Value::Null, Value::String),
                    );
                    obj.insert(
                        "authors".to_string(),
                        Value::Array(ext.authors.iter().cloned().map(Value::String).collect()),
                    );
                    obj.insert(
                        "homepage".to_string(),
                        ext.homepage.clone().map_or(Value::Null, Value::String),
                    );
                    obj.insert(
                        "license".to_string(),
                        ext.license.clone().map_or(Value::Null, Value::String),
                    );
                    obj.insert("abiVersion".to_string(), Value::from(ext.abi_version));
                    obj.insert(
                        "capabilities".to_string(),
                        Value::Array(
                            ext.capabilities
                                .iter()
                                .map(|cap| Value::String(cap.as_str().to_string()))
                                .collect(),
                        ),
                    );
                    Value::Object(obj)
                })
                .collect(),
        ),
    );
    status.insert(
        "loadErrors".to_string(),
        Value::Array(
            state
                .extension_load_errors
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    status.insert(
        "registerErrors".to_string(),
        Value::Array(
            state
                .extension_register_errors
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    status.insert(
        "stats".to_string(),
        Value::Object({
            let mut out = serde_json::Map::new();
            out.insert(
                "diagnostic".to_string(),
                provider_stats_map(&stats.diagnostic),
            );
            out.insert(
                "completion".to_string(),
                provider_stats_map(&stats.completion),
            );
            out.insert(
                "codeAction".to_string(),
                provider_stats_map(&stats.code_action),
            );
            out.insert(
                "navigation".to_string(),
                provider_stats_map(&stats.navigation),
            );
            out.insert(
                "inlayHint".to_string(),
                provider_stats_map(&stats.inlay_hint),
            );
            out
        }),
    );

    Value::Object(status)
}

pub(super) fn handle_extensions_navigation(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let params: Map<String, Value> = crate::stdio_jsonrpc::decode_params(params)?;
    let schema_version = match params.get("schemaVersion").and_then(|v| v.as_u64()) {
        None => None,
        Some(raw) => match u32::try_from(raw) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    schema_version = raw,
                    error = %err,
                    "extensions/navigation schemaVersion is out of range; ignoring"
                );
                None
            }
        },
    };
    if let Some(version) = schema_version {
        if version != nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported schemaVersion {version} (expected {})",
                nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION
            ));
        }
    }

    let text_document = params
        .get("textDocument")
        .cloned()
        .ok_or_else(|| "missing required `textDocument`".to_string())?;
    let text_document: lsp_types::TextDocumentIdentifier =
        serde_json::from_value(text_document).map_err(|e| e.to_string())?;
    let uri = text_document.uri;
    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        let mut response = serde_json::Map::new();
        response.insert(
            "schemaVersion".to_string(),
            Value::from(nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION),
        );
        response.insert("targets".to_string(), Value::Array(Vec::new()));
        return Ok(Value::Object(response));
    }

    let text = state.analysis.file_content(file_id).to_string();
    let path = state
        .analysis
        .file_path(file_id)
        .map(|p| p.to_path_buf())
        .or_else(|| path_from_uri(uri.as_str()));
    let Some(path) = path else {
        tracing::debug!(
            target = "nova.lsp",
            uri = uri.as_str(),
            "skipping extensions navigation for non-file uri"
        );
        let mut response = serde_json::Map::new();
        response.insert(
            "schemaVersion".to_string(),
            Value::from(nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION),
        );
        response.insert("targets".to_string(), Value::Array(Vec::new()));
        return Ok(Value::Object(response));
    };
    let ext_db = Arc::new(SingleFileDb::new(file_id, Some(path), text.clone()));
    let ide_extensions = IdeExtensions::with_registry(
        ext_db,
        Arc::clone(&state.config),
        nova_ext::ProjectId::new(0),
        state.extensions_registry.clone(),
    );

    let mut targets = Vec::new();
    for target in ide_extensions
        .navigation(cancel, nova_ext::Symbol::File(file_id))
        .into_iter()
    {
        if target.file != file_id {
            continue;
        }

        let (range, span) = match target.span {
            Some(span) => {
                let start = offset_to_position_utf16(&text, span.start);
                let end = offset_to_position_utf16(&text, span.end);

                let mut start_obj = serde_json::Map::new();
                start_obj.insert("line".to_string(), Value::from(start.line));
                start_obj.insert("character".to_string(), Value::from(start.character));

                let mut end_obj = serde_json::Map::new();
                end_obj.insert("line".to_string(), Value::from(end.line));
                end_obj.insert("character".to_string(), Value::from(end.character));

                let mut range_obj = serde_json::Map::new();
                range_obj.insert("start".to_string(), Value::Object(start_obj));
                range_obj.insert("end".to_string(), Value::Object(end_obj));

                let mut span_obj = serde_json::Map::new();
                span_obj.insert("start".to_string(), Value::from(span.start as u64));
                span_obj.insert("end".to_string(), Value::from(span.end as u64));

                (Value::Object(range_obj), Value::Object(span_obj))
            }
            None => (Value::Null, Value::Null),
        };

        let mut obj = serde_json::Map::new();
        obj.insert("label".to_string(), Value::String(target.label));
        obj.insert("uri".to_string(), Value::String(uri.as_str().to_string()));
        obj.insert("fileId".to_string(), Value::from(target.file.to_raw()));
        obj.insert("range".to_string(), range);
        obj.insert("span".to_string(), span);
        targets.push(Value::Object(obj));
    }

    let mut response = serde_json::Map::new();
    response.insert(
        "schemaVersion".to_string(),
        Value::from(nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION),
    );
    response.insert("targets".to_string(), Value::Array(targets));
    Ok(Value::Object(response))
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

        let base_dir = match self.project_root.clone() {
            Some(root) => Some(root),
            None => match env::current_dir() {
                Ok(dir) => Some(dir),
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        error = %err,
                        "failed to determine current directory for extension search paths"
                    );
                    None
                }
            },
        };
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
            let register_report =
                ExtensionManager::register_all_best_effort(&mut registry, &loaded);
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
