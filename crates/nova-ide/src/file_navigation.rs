use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use lsp_types::{Location, Position, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{Database, FileId};
use nova_index::{InheritanceEdge, InheritanceIndex};
use nova_types::Span;

use crate::lombok_intel;
use crate::parse::{parse_file, CallSite, ParsedFile, TypeDef, TypeKind};
use crate::text::{position_to_offset, span_to_lsp_range};

#[derive(Clone, Debug)]
struct TypeInfo {
    file_id: FileId,
    uri: Uri,
    def: TypeDef,
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

    fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }

    fn implementation_for_type(&self, type_name: &str) -> Vec<Location> {
        let mut out = Vec::new();
        for subtype in self.inheritance.all_subtypes(type_name) {
            let Some(type_info) = self.type_info(&subtype) else {
                continue;
            };
            let Some(parsed) = self.file(type_info.file_id) else {
                continue;
            };
            out.push(Location {
                uri: type_info.uri.clone(),
                range: span_to_lsp_range(&parsed.text, type_info.def.name_span),
            });
        }
        out
    }

    fn implementation_for_abstract_method(
        &self,
        ty_name: &str,
        method_name: &str,
    ) -> Vec<Location> {
        let Some(type_info) = self.type_info(ty_name) else {
            return Vec::new();
        };

        let method = type_info.def.methods.iter().find(|m| m.name == method_name);
        let is_abstract = type_info.def.kind == TypeKind::Interface
            || method.is_some_and(|m| m.is_abstract || m.body_span.is_none());
        if !is_abstract {
            return Vec::new();
        }

        let mut out = Vec::new();
        for subtype in self.inheritance.all_subtypes(ty_name) {
            let Some((uri, span)) = self.resolve_method_definition(&subtype, method_name) else {
                continue;
            };
            let Some(parsed) = self.file_by_uri(&uri) else {
                continue;
            };
            out.push(Location {
                uri,
                range: span_to_lsp_range(&parsed.text, span),
            });
        }

        out.sort_by(|a, b| a.uri.to_string().cmp(&b.uri.to_string()));
        out.dedup_by(|a, b| a.uri == b.uri && a.range.start == b.range.start);
        out
    }

    fn implementation_for_call(
        &self,
        db: &dyn Database,
        file: FileId,
        parsed: &ParsedFile,
        offset: usize,
        call: &CallSite,
    ) -> Vec<Location> {
        let Some(containing_type) = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))
        else {
            return Vec::new();
        };

        let receiver_ty = if call.receiver == "this" {
            Some(containing_type.name.clone())
        } else if call.receiver == "super" {
            containing_type.super_class.clone()
        } else {
            self.resolve_name_type(parsed, offset, &call.receiver)
        };

        let Some(receiver_ty) = receiver_ty else {
            return Vec::new();
        };

        let Some(receiver_info) = self.type_info(&receiver_ty) else {
            return Vec::new();
        };

        let receiver_is_final = receiver_info.def.modifiers.is_final;

        let mut candidates: Vec<(Uri, Span)> = Vec::new();

        if let Some(def) = self.resolve_method_definition(&receiver_ty, &call.method) {
            candidates.push(def);
        }

        if !receiver_is_final {
            for subtype in self.inheritance.all_subtypes(&receiver_ty) {
                if let Some(def) = self.resolve_method_definition(&subtype, &call.method) {
                    candidates.push(def);
                }
            }
        }

        if candidates.is_empty() {
            if let Some((target_file, target_span)) =
                lombok_intel::goto_virtual_member_definition(db, file, &receiver_ty, &call.method)
            {
                candidates.push((uri_for_file(db, target_file), target_span));
            }
        }

        candidates.sort_by(|a, b| {
            a.0.to_string()
                .cmp(&b.0.to_string())
                .then(a.1.start.cmp(&b.1.start))
        });
        candidates.dedup_by(|a, b| a.0 == b.0 && a.1.start == b.1.start);

        candidates
            .into_iter()
            .filter_map(|(uri, span)| {
                let parsed = self.file_by_uri(&uri)?;
                Some(Location {
                    uri,
                    range: span_to_lsp_range(&parsed.text, span),
                })
            })
            .collect()
    }

    fn resolve_method_definition(&self, ty_name: &str, method_name: &str) -> Option<(Uri, Span)> {
        let type_info = self.type_info(ty_name)?;
        if let Some(method) = type_info
            .def
            .methods
            .iter()
            .find(|m| m.name == method_name && m.body_span.is_some())
        {
            return Some((type_info.uri.clone(), method.name_span));
        }

        let super_name = type_info.def.super_class.as_deref()?;
        self.resolve_method_definition(super_name, method_name)
    }

    fn declaration_for_override(&self, ty_name: &str, method_name: &str) -> Option<Location> {
        let type_info = self.type_info(ty_name)?;

        for iface in &type_info.def.interfaces {
            if let Some(loc) = self.declaration_in_type(iface, method_name) {
                return Some(loc);
            }
        }

        let mut cur = type_info.def.super_class.clone();
        while let Some(next) = cur {
            if let Some(loc) = self.declaration_in_type(&next, method_name) {
                return Some(loc);
            }
            cur = self
                .type_info(&next)
                .and_then(|info| info.def.super_class.clone());
        }

        let parsed = self.file(type_info.file_id)?;
        let method = type_info
            .def
            .methods
            .iter()
            .find(|m| m.name == method_name)?;
        Some(Location {
            uri: type_info.uri.clone(),
            range: span_to_lsp_range(&parsed.text, method.name_span),
        })
    }

    fn declaration_in_type(&self, ty_name: &str, method_name: &str) -> Option<Location> {
        let type_info = self.type_info(ty_name)?;
        let method = type_info
            .def
            .methods
            .iter()
            .find(|m| m.name == method_name)?;

        let is_declaration = type_info.def.kind == TypeKind::Interface
            || method.is_abstract
            || method.body_span.is_none();
        if !is_declaration {
            return None;
        }

        let parsed = self.file(type_info.file_id)?;
        Some(Location {
            uri: type_info.uri.clone(),
            range: span_to_lsp_range(&parsed.text, method.name_span),
        })
    }

    fn method_decl_at(&self, parsed: &ParsedFile, offset: usize) -> Option<(String, String)> {
        for ty in &parsed.types {
            for m in &ty.methods {
                if span_contains(m.name_span, offset) {
                    return Some((ty.name.clone(), m.name.clone()));
                }
            }
        }
        None
    }

    fn resolve_name_type(&self, parsed: &ParsedFile, offset: usize, name: &str) -> Option<String> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if let Some(method) = ty
            .methods
            .iter()
            .find(|m| m.body_span.is_some_and(|r| span_contains(r, offset)))
        {
            if let Some(local) = method.locals.iter().find(|v| v.name == name) {
                return Some(local.ty.clone());
            }
        }

        if let Some(field) = ty.fields.iter().find(|f| f.name == name) {
            return Some(field.ty.clone());
        }

        None
    }

    fn variable_declaration(
        &self,
        parsed: &ParsedFile,
        offset: usize,
        name: &str,
    ) -> Option<(Uri, Span)> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if let Some(method) = ty
            .methods
            .iter()
            .find(|m| m.body_span.is_some_and(|r| span_contains(r, offset)))
        {
            if let Some(local) = method.locals.iter().find(|v| v.name == name) {
                return Some((parsed.uri.clone(), local.name_span));
            }
        }

        if let Some(field) = ty.fields.iter().find(|f| f.name == name) {
            return Some((parsed.uri.clone(), field.name_span));
        }

        None
    }

    fn file_by_uri(&self, uri: &Uri) -> Option<&ParsedFile> {
        let file_id = self.uri_to_file_id.get(uri.as_str())?;
        self.file(*file_id)
    }
}

/// Best-effort `textDocument/implementation` for FileId-based databases.
#[must_use]
pub fn implementation(db: &dyn Database, file: FileId, position: Position) -> Vec<Location> {
    let index = FileNavigationIndex::new(db);
    let Some(parsed) = index.file(file) else {
        return Vec::new();
    };
    let Some(offset) = position_to_offset(&parsed.text, position) else {
        return Vec::new();
    };

    if let Some(call) = parsed
        .calls
        .iter()
        .find(|call| span_contains(call.method_span, offset))
    {
        return index.implementation_for_call(db, file, parsed, offset, call);
    }

    if let Some((ty_name, method_name)) = index.method_decl_at(parsed, offset) {
        return index.implementation_for_abstract_method(&ty_name, &method_name);
    }

    if let Some(type_name) = parsed
        .types
        .iter()
        .find(|ty| span_contains(ty.name_span, offset))
        .map(|ty| ty.name.clone())
    {
        return index.implementation_for_type(&type_name);
    }

    Vec::new()
}

/// Best-effort `textDocument/declaration` for FileId-based databases.
#[must_use]
pub fn declaration(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let index = FileNavigationIndex::new(db);
    let parsed = index.file(file)?;
    let offset = position_to_offset(&parsed.text, position)?;

    if let Some((ty_name, method_name)) = index.method_decl_at(parsed, offset) {
        return index.declaration_for_override(&ty_name, &method_name);
    }

    let (ident, _span) = identifier_at(&parsed.text, offset)?;
    if let Some((decl_uri, decl_span)) = index.variable_declaration(parsed, offset, &ident) {
        let decl_parsed = index.file_by_uri(&decl_uri)?;
        return Some(Location {
            uri: decl_uri,
            range: span_to_lsp_range(&decl_parsed.text, decl_span),
        });
    }

    None
}

/// Best-effort `textDocument/typeDefinition` for FileId-based databases.
#[must_use]
pub fn type_definition(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let index = FileNavigationIndex::new(db);
    let parsed = index.file(file)?;
    let offset = position_to_offset(&parsed.text, position)?;

    if let Some((ident, _ident_span)) = identifier_at(&parsed.text, offset) {
        if let Some(type_info) = index.type_info(&ident) {
            let def_file = index.file(type_info.file_id)?;
            return Some(Location {
                uri: type_info.uri.clone(),
                range: span_to_lsp_range(&def_file.text, type_info.def.name_span),
            });
        }

        let ty = index.resolve_name_type(parsed, offset, &ident)?;
        let type_info = index.type_info(&ty)?;
        let def_file = index.file(type_info.file_id)?;
        return Some(Location {
            uri: type_info.uri.clone(),
            range: span_to_lsp_range(&def_file.text, type_info.def.name_span),
        });
    }

    None
}

fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

fn identifier_at(text: &str, offset: usize) -> Option<(String, Span)> {
    if offset > text.len() {
        return None;
    }

    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            start -= 1;
        } else {
            break;
        }
    }

    let mut end = offset;
    while end < bytes.len() {
        let ch = bytes[end] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            end += 1;
        } else {
            break;
        }
    }

    if start == end {
        return None;
    }

    Some((text[start..end].to_string(), Span::new(start, end)))
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
