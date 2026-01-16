use crate::ServerState;

use lsp_types::{
    Location as LspLocation, Position as LspTypesPosition, Range as LspTypesRange,
    SymbolInformation, SymbolKind as LspSymbolKind, Uri as LspUri, WorkspaceSymbolParams,
};
use nova_workspace::Workspace;
use serde_json::Value as JsonValue;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn json_string<'a>(value: &'a JsonValue, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

fn json_opt_string(value: &JsonValue, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| json_string(value, key))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn json_u32(value: &JsonValue, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
}

fn json_location(value: &JsonValue) -> Option<(String, u32, u32)> {
    // Task 19: `WorkspaceSymbol` becomes flat and stores a single `location`.
    // For compatibility with older shape, also accept `locations[0]`.
    let loc = value
        .get("location")
        .or_else(|| value.get("locations").and_then(|v| v.get(0)))?;

    let file = json_string(loc, "file")?.to_string();
    let line = json_u32(loc, "line").unwrap_or(0);
    let column = json_u32(loc, "column").unwrap_or(0);
    Some((file, line, column))
}

fn kind_to_lsp(kind: Option<&JsonValue>) -> LspSymbolKind {
    let Some(kind) = kind else {
        return LspSymbolKind::OBJECT;
    };

    match kind {
        JsonValue::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "file" => LspSymbolKind::FILE,
            "module" => LspSymbolKind::MODULE,
            "namespace" => LspSymbolKind::NAMESPACE,
            "package" => LspSymbolKind::PACKAGE,
            "class" => LspSymbolKind::CLASS,
            "record" => LspSymbolKind::STRUCT,
            // LSP's `SymbolKind` does not have a dedicated annotation kind; treat as interface.
            "annotation" => LspSymbolKind::INTERFACE,
            "method" => LspSymbolKind::METHOD,
            "property" => LspSymbolKind::PROPERTY,
            "field" => LspSymbolKind::FIELD,
            "constructor" => LspSymbolKind::CONSTRUCTOR,
            "enum" => LspSymbolKind::ENUM,
            "interface" => LspSymbolKind::INTERFACE,
            "function" => LspSymbolKind::FUNCTION,
            "variable" => LspSymbolKind::VARIABLE,
            "constant" => LspSymbolKind::CONSTANT,
            "string" => LspSymbolKind::STRING,
            "number" => LspSymbolKind::NUMBER,
            "boolean" => LspSymbolKind::BOOLEAN,
            "array" => LspSymbolKind::ARRAY,
            "object" => LspSymbolKind::OBJECT,
            "key" => LspSymbolKind::KEY,
            "null" => LspSymbolKind::NULL,
            "enumconstant" | "enum_constant" | "enummember" | "enum_member" => {
                LspSymbolKind::ENUM_MEMBER
            }
            "struct" => LspSymbolKind::STRUCT,
            "event" => LspSymbolKind::EVENT,
            "operator" => LspSymbolKind::OPERATOR,
            "typeparam" | "type_parameter" => LspSymbolKind::TYPE_PARAMETER,
            _ => LspSymbolKind::OBJECT,
        },
        JsonValue::Number(n) => match n.as_u64() {
            Some(1) => LspSymbolKind::FILE,
            Some(2) => LspSymbolKind::MODULE,
            Some(3) => LspSymbolKind::NAMESPACE,
            Some(4) => LspSymbolKind::PACKAGE,
            Some(5) => LspSymbolKind::CLASS,
            Some(6) => LspSymbolKind::METHOD,
            Some(7) => LspSymbolKind::PROPERTY,
            Some(8) => LspSymbolKind::FIELD,
            Some(9) => LspSymbolKind::CONSTRUCTOR,
            Some(10) => LspSymbolKind::ENUM,
            Some(11) => LspSymbolKind::INTERFACE,
            Some(12) => LspSymbolKind::FUNCTION,
            Some(13) => LspSymbolKind::VARIABLE,
            Some(14) => LspSymbolKind::CONSTANT,
            Some(15) => LspSymbolKind::STRING,
            Some(16) => LspSymbolKind::NUMBER,
            Some(17) => LspSymbolKind::BOOLEAN,
            Some(18) => LspSymbolKind::ARRAY,
            Some(19) => LspSymbolKind::OBJECT,
            Some(20) => LspSymbolKind::KEY,
            Some(21) => LspSymbolKind::NULL,
            Some(22) => LspSymbolKind::ENUM_MEMBER,
            Some(23) => LspSymbolKind::STRUCT,
            Some(24) => LspSymbolKind::EVENT,
            Some(25) => LspSymbolKind::OPERATOR,
            Some(26) => LspSymbolKind::TYPE_PARAMETER,
            _ => LspSymbolKind::OBJECT,
        },
        _ => LspSymbolKind::OBJECT,
    }
}

pub(super) fn handle_workspace_symbol(
    params: JsonValue,
    state: &mut ServerState,
    cancel: &CancellationToken,
) -> Result<JsonValue, (i32, String)> {
    let params: WorkspaceSymbolParams = crate::stdio_jsonrpc::decode_params_with_code(params)?;

    let query = params.query.trim();

    if let Some(dist) = state.distributed.as_mut() {
        if dist.initial_index.is_some() {
            let cancel = cancel.clone();
            let join_result = {
                let handle = dist
                    .initial_index
                    .as_mut()
                    .expect("checked initial_index.is_some");
                dist.runtime.block_on(async {
                    tokio::select! {
                        _ = cancel.cancelled() => None,
                        res = handle => Some(res),
                    }
                })
            };

            let join_result = match join_result {
                Some(value) => value,
                None => return Err((-32800, "Request cancelled".to_string())),
            };

            dist.initial_index = None;

            match join_result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err((-32603, err.to_string())),
                Err(err) => return Err((-32603, err.to_string())),
            }
        }

        let frontend = Arc::clone(&dist.frontend);
        let cancel = cancel.clone();
        let symbols = dist.runtime.block_on(async {
            tokio::select! {
                _ = cancel.cancelled() => None,
                syms = frontend.workspace_symbols(query) => Some(syms),
            }
        });
        let symbols = match symbols {
            Some(symbols) => symbols,
            None => return Err((-32800, "Request cancelled".to_string())),
        };

        let mut out = Vec::new();
        for symbol in symbols {
            let mut path = PathBuf::from(&symbol.path);
            if !path.is_absolute() {
                path = dist.workspace_root.join(path);
            }
            let abs = nova_core::AbsPathBuf::try_from(path).map_err(|e| (-32603, e.to_string()))?;
            let uri = nova_core::path_to_file_uri(&abs)
                .map_err(|e| (-32603, e.to_string()))?
                .parse::<LspUri>()
                .map_err(|e| (-32603, format!("invalid uri: {e}")))?;

            let position = LspTypesPosition {
                line: symbol.line,
                character: symbol.column,
            };
            let location = LspLocation {
                uri,
                range: LspTypesRange::new(position, position),
            };

            out.push(SymbolInformation {
                name: symbol.name,
                kind: LspSymbolKind::OBJECT,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
                location,
                container_name: Some(symbol.path),
            });
        }

        return serde_json::to_value(out).map_err(|e| (-32603, e.to_string()));
    }

    if state.workspace.is_none() {
        let project_root = state.project_root.clone().ok_or_else(|| {
            (
                -32602,
                "missing project root (initialize.rootUri)".to_string(),
            )
        })?;
        // Reuse the server's shared memory manager so all workspace components
        // (Salsa memo evictor, symbol search index, etc.) account/evict together.
        let workspace = Workspace::open_with_memory_manager(project_root, state.memory.clone())
            .map_err(|e| (-32603, e.to_string()))?;
        state.workspace = Some(workspace);
    }

    let workspace = state.workspace.as_ref().expect("workspace initialized");
    let symbols = workspace
        .workspace_symbols_cancelable(query, cancel)
        .map_err(|e| (-32603, e.to_string()))?;

    let mut out = Vec::new();
    for symbol in symbols {
        let value =
            serde_json::to_value(&symbol).map_err(|e| (-32603, format!("symbol json: {e}")))?;
        let Some((file, line, column)) = json_location(&value) else {
            continue;
        };
        let mut path = PathBuf::from(&file);
        if !path.is_absolute() {
            path = workspace.root().join(path);
        }

        let abs = nova_core::AbsPathBuf::try_from(path).map_err(|e| (-32603, e.to_string()))?;
        let uri = nova_core::path_to_file_uri(&abs)
            .map_err(|e| (-32603, e.to_string()))?
            .parse::<LspUri>()
            .map_err(|e| (-32603, format!("invalid uri: {e}")))?;

        let position = LspTypesPosition {
            line,
            character: column,
        };
        let location = LspLocation {
            uri,
            range: LspTypesRange::new(position, position),
        };

        let kind = kind_to_lsp(value.get("kind"));

        let container_name = json_opt_string(&value, &["container_name", "containerName"])
            .or_else(|| Some(file.clone()));

        out.push(SymbolInformation {
            name: symbol.name.clone(),
            kind,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            location,
            container_name,
        });
    }

    serde_json::to_value(out).map_err(|e| (-32603, e.to_string()))
}
