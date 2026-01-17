use lsp_types::request::{
    GotoDeclarationParams, GotoDeclarationResponse, GotoImplementationParams,
    GotoImplementationResponse, GotoTypeDefinitionParams, GotoTypeDefinitionResponse,
};

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    Location, ReferenceParams, TypeHierarchyItem, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams,
};

use nova_db::{Database as FileIdDatabase, FileId};
use nova_ide::{Database, DatabaseSnapshot};

fn snapshot(db: &Database) -> DatabaseSnapshot {
    db.snapshot()
}

pub fn implementation(
    db: &Database,
    params: GotoImplementationParams,
) -> Option<GotoImplementationResponse> {
    let snap = snapshot(db);
    let file = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    let locations = snap.implementation(file, pos);
    if locations.is_empty() {
        None
    } else {
        Some(GotoImplementationResponse::Array(locations))
    }
}

pub fn declaration(
    db: &Database,
    params: GotoDeclarationParams,
) -> Option<GotoDeclarationResponse> {
    let snap = snapshot(db);
    let file = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    let location = snap.declaration(file, pos)?;
    Some(GotoDeclarationResponse::Scalar(location))
}

pub fn type_definition(
    db: &Database,
    params: GotoTypeDefinitionParams,
) -> Option<GotoTypeDefinitionResponse> {
    let snap = snapshot(db);
    let file = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    let location = snap.type_definition(file, pos)?;
    Some(GotoTypeDefinitionResponse::Scalar(location))
}

pub fn references(db: &Database, params: ReferenceParams) -> Option<Vec<Location>> {
    let snap = snapshot(db);
    let file_uri = &params.text_document_position.text_document.uri;
    let file_id = snap.file_id_for_uri(file_uri)?;
    let pos = params.text_document_position.position;
    let locations =
        nova_ide::find_references(&snap, file_id, pos, params.context.include_declaration);
    if locations.is_empty() {
        None
    } else {
        Some(locations)
    }
}

pub fn prepare_call_hierarchy(
    db: &dyn FileIdDatabase,
    params: CallHierarchyPrepareParams,
) -> Option<Vec<CallHierarchyItem>> {
    let file_uri = &params.text_document_position_params.text_document.uri;
    let file = file_id_from_uri(db, file_uri)?;
    let pos = params.text_document_position_params.position;
    nova_ide::prepare_call_hierarchy(db, file, pos)
}

pub fn call_hierarchy_incoming_calls(
    db: &dyn FileIdDatabase,
    params: CallHierarchyIncomingCallsParams,
) -> Option<Vec<CallHierarchyIncomingCall>> {
    let file = file_id_from_uri(db, &params.item.uri)?;
    let calls = nova_ide::call_hierarchy_incoming_calls_for_item(db, file, &params.item);
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

pub fn call_hierarchy_outgoing_calls(
    db: &dyn FileIdDatabase,
    params: CallHierarchyOutgoingCallsParams,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    let file = file_id_from_uri(db, &params.item.uri)?;
    let calls = nova_ide::call_hierarchy_outgoing_calls_for_item(db, file, &params.item);
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

pub fn prepare_type_hierarchy(
    db: &dyn FileIdDatabase,
    params: TypeHierarchyPrepareParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let file_uri = &params.text_document_position_params.text_document.uri;
    let file = file_id_from_uri(db, file_uri)?;
    let pos = params.text_document_position_params.position;
    nova_ide::prepare_type_hierarchy(db, file, pos)
}

pub fn type_hierarchy_supertypes(
    db: &dyn FileIdDatabase,
    params: TypeHierarchySupertypesParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let file = file_id_from_uri(db, &params.item.uri)?;
    let items = nova_ide::type_hierarchy_supertypes(db, file, &params.item.name);
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

pub fn type_hierarchy_subtypes(
    db: &dyn FileIdDatabase,
    params: TypeHierarchySubtypesParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let file = file_id_from_uri(db, &params.item.uri)?;
    let items = nova_ide::type_hierarchy_subtypes(db, file, &params.item.name);
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn file_id_from_uri(db: &dyn FileIdDatabase, uri: &lsp_types::Uri) -> Option<FileId> {
    let uri_str = uri.as_str();
    let url = match url::Url::parse(uri_str) {
        Ok(url) => url,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri_str,
                err = %err,
                "failed to parse uri as url"
            );
            return None;
        }
    };
    if url.scheme() != "file" {
        return None;
    }
    let path = match url.to_file_path() {
        Ok(path) => path,
        Err(()) => {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri_str,
                "failed to decode file uri to path"
            );
            return None;
        }
    };
    db.file_id(&path)
}
