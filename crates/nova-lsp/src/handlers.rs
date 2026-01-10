use lsp_types::request::{
    GotoDeclarationParams, GotoDeclarationResponse, GotoImplementationParams,
    GotoImplementationResponse, GotoTypeDefinitionParams, GotoTypeDefinitionResponse,
};

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

pub fn declaration(db: &Database, params: GotoDeclarationParams) -> Option<GotoDeclarationResponse> {
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
