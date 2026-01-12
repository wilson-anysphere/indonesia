use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use lsp_types::{Location, Position, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{Database, FileId};
use nova_index::{InheritanceEdge, InheritanceIndex};

use crate::lombok_intel;
use crate::nav_core;
use crate::parse::{parse_file, ParsedFile, TypeDef};
use crate::text::{position_to_offset_with_index, span_to_lsp_range_with_index};

#[derive(Clone, Debug)]
struct TypeInfo {
    file_id: FileId,
    uri: Uri,
    def: TypeDef,
}

impl nav_core::NavTypeInfo for TypeInfo {
    fn uri(&self) -> &Uri {
        &self.uri
    }

    fn def(&self) -> &TypeDef {
        &self.def
    }
}

#[derive(Debug, Default)]
struct FileNavigationIndex {
    files: HashMap<FileId, ParsedFile>,
    uri_to_file_id: HashMap<String, FileId>,
    types: HashMap<String, TypeInfo>,
    inheritance: InheritanceIndex,
}

impl FileNavigationIndex {
    fn new(db: &dyn Database) -> Self {
        let mut files = HashMap::new();
        let mut file_ids = db.all_file_ids();
        file_ids.sort_by_key(|id| id.to_raw());

        let mut uri_to_file_id = HashMap::new();
        for file_id in &file_ids {
            let uri = uri_for_file(db, *file_id);
            let text = db.file_content(*file_id).to_string();
            let parsed = parse_file(uri, text);
            uri_to_file_id.insert(parsed.uri.to_string(), *file_id);
            files.insert(*file_id, parsed);
        }

        let mut types: HashMap<String, TypeInfo> = HashMap::new();
        for file_id in &file_ids {
            let Some(parsed_file) = files.get(file_id) else {
                continue;
            };
            for ty in &parsed_file.types {
                types.entry(ty.name.clone()).or_insert_with(|| TypeInfo {
                    file_id: *file_id,
                    uri: parsed_file.uri.clone(),
                    def: ty.clone(),
                });
            }
        }

        let mut inheritance = InheritanceIndex::default();
        let mut edges: Vec<InheritanceEdge> = Vec::new();
        for file_id in &file_ids {
            let Some(parsed_file) = files.get(file_id) else {
                continue;
            };
            for ty in &parsed_file.types {
                if let Some(super_class) = &ty.super_class {
                    edges.push(InheritanceEdge {
                        file: parsed_file.uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: super_class.clone(),
                    });
                }
                for iface in &ty.interfaces {
                    edges.push(InheritanceEdge {
                        file: parsed_file.uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: iface.clone(),
                    });
                }
            }
        }
        inheritance.extend(edges);

        Self {
            files,
            uri_to_file_id,
            types,
            inheritance,
        }
    }

    fn file(&self, file_id: FileId) -> Option<&ParsedFile> {
        self.files.get(&file_id)
    }

    fn file_by_uri(&self, uri: &Uri) -> Option<&ParsedFile> {
        let file_id = self.uri_to_file_id.get(uri.as_str())?;
        self.file(*file_id)
    }

    fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }
}

/// Best-effort `textDocument/implementation` for FileId-based databases.
#[must_use]
pub fn implementation(db: &dyn Database, file: FileId, position: Position) -> Vec<Location> {
    let index = FileNavigationIndex::new(db);
    let Some(parsed) = index.file(file) else {
        return Vec::new();
    };
    let Some(offset) =
        position_to_offset_with_index(&parsed.line_index, &parsed.text, position)
    else {
        return Vec::new();
    };

    let lookup_type_info = |name: &str| index.type_info(name);
    let lookup_file = |uri: &Uri| index.file_by_uri(uri);
    let lombok_fallback = |receiver_ty: &str, method_name: &str| {
        lombok_intel::goto_virtual_member_definition(db, file, receiver_ty, method_name).map(
            |(target_file, target_span)| (uri_for_file(db, target_file), target_span),
        )
    };

    if let Some(call) = parsed
        .calls
        .iter()
        .find(|call| nav_core::span_contains(call.method_span, offset))
    {
        return nav_core::implementation_for_call(
            &index.inheritance,
            &lookup_type_info,
            &lookup_file,
            parsed,
            offset,
            call,
            &lombok_fallback,
        );
    }

    if let Some((ty_name, method_name)) = nav_core::method_decl_at(parsed, offset) {
        return nav_core::implementation_for_abstract_method(
            &index.inheritance,
            &lookup_type_info,
            &lookup_file,
            &ty_name,
            &method_name,
        );
    }

    if let Some(type_name) = parsed
        .types
        .iter()
        .find(|ty| nav_core::span_contains(ty.name_span, offset))
        .map(|ty| ty.name.clone())
    {
        return nav_core::implementation_for_type(
            &index.inheritance,
            &lookup_type_info,
            &lookup_file,
            &type_name,
        );
    }

    Vec::new()
}

/// Best-effort `textDocument/declaration` for FileId-based databases.
#[must_use]
pub fn declaration(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let index = FileNavigationIndex::new(db);
    let parsed = index.file(file)?;
    let offset = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)?;

    let lookup_type_info = |name: &str| index.type_info(name);
    let lookup_file = |uri: &Uri| index.file_by_uri(uri);

    if let Some((ty_name, method_name)) = nav_core::method_decl_at(parsed, offset) {
        return nav_core::declaration_for_override(
            &lookup_type_info,
            &lookup_file,
            &ty_name,
            &method_name,
        );
    }

    let (ident, _span) = nav_core::identifier_at(&parsed.text, offset)?;
    if let Some((decl_uri, decl_span)) = nav_core::variable_declaration(parsed, offset, &ident) {
        let decl_parsed = index.file_by_uri(&decl_uri)?;
        return Some(Location {
            uri: decl_uri,
            range: span_to_lsp_range_with_index(&decl_parsed.line_index, &decl_parsed.text, decl_span),
        });
    }

    None
}

/// Best-effort `textDocument/typeDefinition` for FileId-based databases.
#[must_use]
pub fn type_definition(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let index = FileNavigationIndex::new(db);
    let parsed = index.file(file)?;
    let offset = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)?;

    if let Some((ident, _ident_span)) = nav_core::identifier_at(&parsed.text, offset) {
        if let Some(type_info) = index.type_info(&ident) {
            let def_file = index.file(type_info.file_id)?;
            return Some(Location {
                uri: type_info.uri.clone(),
                range: span_to_lsp_range_with_index(
                    &def_file.line_index,
                    &def_file.text,
                    type_info.def.name_span,
                ),
            });
        }

        let ty = nav_core::resolve_name_type(parsed, offset, &ident)?;
        let type_info = index.type_info(&ty)?;
        let def_file = index.file(type_info.file_id)?;
        return Some(Location {
            uri: type_info.uri.clone(),
            range: span_to_lsp_range_with_index(
                &def_file.line_index,
                &def_file.text,
                type_info.def.name_span,
            ),
        });
    }

    None
}

fn uri_for_file(db: &dyn Database, file_id: FileId) -> Uri {
    if let Some(path) = db.file_path(file_id) {
        if let Some(uri) = uri_for_path(path) {
            return uri;
        }
    }

    Uri::from_str(&format!("file:///unknown/{}.java", file_id.to_raw()))
        .expect("fallback URI is valid")
}

fn uri_for_path(path: &Path) -> Option<Uri> {
    let abs = AbsPathBuf::new(path.to_path_buf()).ok()?;
    let uri = path_to_file_uri(&abs).ok()?;
    Uri::from_str(&uri).ok()
}

