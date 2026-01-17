use crate::rename_lsp;
use crate::stdio_text::{ident_range_at, offset_to_position_utf16, position_to_offset_utf16};
use crate::ServerState;

use lsp_types::{
    Range as LspTypesRange, RenameParams as LspRenameParams, TextDocumentPositionParams,
    WorkspaceEdit as LspWorkspaceEdit,
};
use nova_refactor::{
    rename as semantic_rename, FileId as RefactorFileId, JavaSymbolKind, RefactorDatabase,
    RenameParams as RefactorRenameParams, SemanticRefactorError,
};

pub(super) fn handle_prepare_rename(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;
    let snapshot = match state.refactor_snapshot(&uri) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                error = ?err,
                "failed to build refactor snapshot for prepareRename; returning null"
            );
            return Ok(serde_json::Value::Null);
        }
    };

    let file = RefactorFileId::new(uri.to_string());
    let db = snapshot.db();
    let Some(source) = db.file_text(&file) else {
        return Ok(serde_json::Value::Null);
    };

    let Some(offset) = position_to_offset_utf16(source, params.position) else {
        return Ok(serde_json::Value::Null);
    };

    let symbol = db.symbol_at(&file, offset).or_else(|| {
        offset
            .checked_sub(1)
            .and_then(|offset| db.symbol_at(&file, offset))
    });
    let Some(symbol) = symbol else {
        return Ok(serde_json::Value::Null);
    };

    let (start, end) = match db.symbol_kind(symbol) {
        Some(JavaSymbolKind::Package) => {
            let Some(def) = db.symbol_definition(symbol) else {
                return Ok(serde_json::Value::Null);
            };
            (def.name_range.start, def.name_range.end)
        }
        Some(
            JavaSymbolKind::Local
            | JavaSymbolKind::Parameter
            | JavaSymbolKind::Field
            | JavaSymbolKind::Method
            | JavaSymbolKind::Type
            | JavaSymbolKind::TypeParameter,
        ) => {
            // Prepare rename should only succeed when there is an identifier *and* a refactorable
            // symbol at (or adjacent to) the cursor. The identifier check is important because some
            // clients call prepareRename opportunistically and expect a null result when the cursor
            // isn't on an identifier.
            let Some((start, end)) = ident_range_at(source, offset) else {
                return Ok(serde_json::Value::Null);
            };
            (start, end)
        }
        _ => return Ok(serde_json::Value::Null),
    };

    let range = LspTypesRange::new(
        offset_to_position_utf16(source, start),
        offset_to_position_utf16(source, end),
    );
    serde_json::to_value(range).map_err(|e| e.to_string())
}

pub(super) fn handle_rename(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<LspWorkspaceEdit, (i32, String)> {
    let params: LspRenameParams = crate::stdio_jsonrpc::decode_params_with_code(params)?;
    let uri = params.text_document_position.text_document.uri;
    let snapshot = state
        .refactor_snapshot(&uri)
        .map_err(|e| (-32602, e.to_string()))?;

    let file = RefactorFileId::new(uri.to_string());
    let db = snapshot.db();
    let source = db.file_text(&file).ok_or_else(|| {
        (
            -32602,
            format!("missing document text for `{}`", uri.as_str()),
        )
    })?;

    let Some(offset) = position_to_offset_utf16(source, params.text_document_position.position)
    else {
        return Err((-32602, "position out of bounds".to_string()));
    };

    let symbol = db.symbol_at(&file, offset).or_else(|| {
        offset
            .checked_sub(1)
            .and_then(|offset| db.symbol_at(&file, offset))
    });
    let Some(symbol) = symbol else {
        // If the cursor is on an identifier but we can't resolve it to a refactor symbol, prefer a
        // "rename not supported" error over "no symbol" to avoid confusing clients that attempt
        // rename at arbitrary identifier-like positions.
        if ident_range_at(source, offset).is_some() {
            return Err((
                -32602,
                SemanticRefactorError::RenameNotSupported { kind: None }.to_string(),
            ));
        }
        return Err((-32602, "no symbol at cursor".to_string()));
    };

    let edit = semantic_rename(
        snapshot.db(),
        RefactorRenameParams {
            symbol,
            new_name: params.new_name,
        },
    )
    .map_err(|err| match err {
        SemanticRefactorError::Conflicts(conflicts) => {
            (-32602, format!("rename conflicts: {conflicts:?}"))
        }
        err @ SemanticRefactorError::RenameNotSupported { .. } => (-32602, err.to_string()),
        err @ SemanticRefactorError::MoveJava(_) => (-32602, err.to_string()),
        err @ SemanticRefactorError::InvalidFileId { .. } => (-32602, err.to_string()),
        other => (-32603, other.to_string()),
    })?;

    rename_lsp::rename_workspace_edit_to_lsp(snapshot.db(), &edit).map_err(|e| (-32603, e))
}
