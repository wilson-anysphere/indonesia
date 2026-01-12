use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use lsp_types::Uri;
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{Database, FileId};
use nova_index::{InheritanceEdge, InheritanceIndex};
use nova_types::Span;

use crate::parse::{parse_file, ParsedFile, TypeDef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SymbolKey {
    pub(crate) file: FileId,
    pub(crate) span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedKind {
    LocalVar { scope: Span },
    Field,
    Method,
    Type,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedSymbol {
    pub(crate) name: String,
    pub(crate) kind: ResolvedKind,
    /// The definition this symbol resolves to.
    pub(crate) def: Definition,
}

#[derive(Clone, Debug)]
pub(crate) struct Definition {
    pub(crate) file: FileId,
    pub(crate) uri: Uri,
    pub(crate) name_span: Span,
    pub(crate) key: SymbolKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OccurrenceKind {
    MemberCall { receiver: String },
    MemberField { receiver: String },
    LocalCall,
    Ident,
}

#[derive(Clone, Debug)]
struct TypeInfo {
    file_id: FileId,
    uri: Uri,
    def: TypeDef,
}

#[derive(Debug, Default)]
struct WorkspaceIndex {
    files: HashMap<FileId, ParsedFile>,
    uri_to_file_id: HashMap<String, FileId>,
    types: HashMap<String, TypeInfo>,
    inheritance: InheritanceIndex,
}

impl WorkspaceIndex {
    fn new(db: &dyn Database) -> Self {
        let mut files = HashMap::new();
        let mut file_ids = db.all_file_ids();
        file_ids.sort_by_key(|id| id.to_raw());

        let mut uri_to_file_id = HashMap::new();
        for file_id in &file_ids {
            let is_java = db.file_path(*file_id).is_some_and(|path| {
                path.extension().and_then(|e| e.to_str()) == Some("java")
            });
            if !is_java {
                continue;
            }

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
                types
                    .entry(ty.name.clone())
                    .or_insert_with(|| TypeInfo {
                        file_id: *file_id,
                        uri: parsed_file.uri.clone(),
                        def: ty.clone(),
                    });
            }
        }

        let mut inheritance = InheritanceIndex::default();
        let mut edges: Vec<InheritanceEdge> = Vec::new();
        for parsed_file in files.values() {
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

    fn file(&self, file: FileId) -> Option<&ParsedFile> {
        self.files.get(&file)
    }

    fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }

    fn resolve_name_type(&self, parsed: &ParsedFile, offset: usize, name: &str) -> Option<String> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if let Some(method) = ty
            .methods
            .iter()
            .find(|m| m.body_span.is_some_and(|span| span_contains(span, offset)))
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

    fn local_or_field_declaration(
        &self,
        file: FileId,
        parsed: &ParsedFile,
        offset: usize,
        name: &str,
    ) -> Option<(ResolvedKind, Definition)> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if let Some(method) = ty
            .methods
            .iter()
            .find(|m| m.body_span.is_some_and(|span| span_contains(span, offset)))
        {
            if let Some(local) = method.locals.iter().find(|v| v.name == name) {
                let def = Definition {
                    file,
                    uri: parsed.uri.clone(),
                    name_span: local.name_span,
                    key: SymbolKey {
                        file,
                        span: local.name_span,
                    },
                };
                return Some((
                    ResolvedKind::LocalVar {
                        scope: method.body_span.expect("checked above"),
                    },
                    def,
                ));
            }
        }

        if let Some(field) = ty.fields.iter().find(|f| f.name == name) {
            let def = Definition {
                file,
                uri: parsed.uri.clone(),
                name_span: field.name_span,
                key: SymbolKey {
                    file,
                    span: field.name_span,
                },
            };
            return Some((ResolvedKind::Field, def));
        }

        None
    }

    fn method_in_enclosing_type(
        &self,
        file: FileId,
        parsed: &ParsedFile,
        offset: usize,
        method_name: &str,
    ) -> Option<Definition> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;
        let method = ty.methods.iter().find(|m| m.name == method_name)?;
        Some(Definition {
            file,
            uri: parsed.uri.clone(),
            name_span: method.name_span,
            key: SymbolKey {
                file,
                span: method.name_span,
            },
        })
    }

    fn resolve_method_definition(&self, ty_name: &str, method_name: &str) -> Option<Definition> {
        let type_info = self.type_info(ty_name)?;
        if let Some(method) = type_info.def.methods.iter().find(|m| m.name == method_name) {
            return Some(Definition {
                file: type_info.file_id,
                uri: type_info.uri.clone(),
                name_span: method.name_span,
                key: SymbolKey {
                    file: type_info.file_id,
                    span: method.name_span,
                },
            });
        }

        let super_name = type_info.def.super_class.as_deref()?;
        self.resolve_method_definition(super_name, method_name)
    }

    fn resolve_field_definition(&self, ty_name: &str, field_name: &str) -> Option<Definition> {
        let type_info = self.type_info(ty_name)?;
        if let Some(field) = type_info.def.fields.iter().find(|f| f.name == field_name) {
            return Some(Definition {
                file: type_info.file_id,
                uri: type_info.uri.clone(),
                name_span: field.name_span,
                key: SymbolKey {
                    file: type_info.file_id,
                    span: field.name_span,
                },
            });
        }

        let super_name = type_info.def.super_class.as_deref()?;
        self.resolve_field_definition(super_name, field_name)
    }

    fn resolve_type_definition(&self, ty_name: &str) -> Option<Definition> {
        let info = self.type_info(ty_name)?;
        Some(Definition {
            file: info.file_id,
            uri: info.uri.clone(),
            name_span: info.def.name_span,
            key: SymbolKey {
                file: info.file_id,
                span: info.def.name_span,
            },
        })
    }

    fn resolve_receiver_type(
        &self,
        parsed: &ParsedFile,
        offset: usize,
        receiver: &str,
    ) -> Option<String> {
        let containing_type = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if receiver == "this" {
            return Some(containing_type.name.clone());
        }
        if receiver == "super" {
            return containing_type.super_class.clone();
        }

        // locals/fields
        if let Some(ty) = self.resolve_name_type(parsed, offset, receiver) {
            return Some(ty);
        }

        // Type name (static access)
        if self.type_info(receiver).is_some() {
            return Some(receiver.to_string());
        }

        None
    }
}

/// Per-request core Java symbol resolver.
///
/// This is intentionally lightweight and best-effort: it uses `crate::parse::parse_file`
/// and textual context around the cursor to resolve common symbols (locals, fields,
/// types, and member calls).
pub(crate) struct Resolver {
    index: WorkspaceIndex,
}

impl Resolver {
    pub(crate) fn new(db: &dyn Database) -> Self {
        Self {
            index: WorkspaceIndex::new(db),
        }
    }

    pub(crate) fn parsed_file(&self, file: FileId) -> Option<&ParsedFile> {
        self.index.file(file)
    }

    pub(crate) fn java_file_ids_sorted(&self) -> Vec<FileId> {
        let mut ids: Vec<_> = self.index.files.keys().copied().collect();
        ids.sort_by_key(|id| id.to_raw());
        ids
    }

    pub(crate) fn resolve_at(&self, file: FileId, offset: usize) -> Option<ResolvedSymbol> {
        let parsed = self.index.file(file)?;
        let (ident, ident_span) = identifier_at(&parsed.text, offset)?;
        let occurrence = classify_occurrence(&parsed.text, ident_span)?;

        // 1) Locals/fields in the current file.
        if matches!(occurrence, OccurrenceKind::Ident) {
            if let Some((kind, def)) =
                self.index
                    .local_or_field_declaration(file, parsed, ident_span.start, &ident)
            {
                return Some(ResolvedSymbol {
                    name: ident,
                    kind,
                    def,
                });
            }
        }

        // 2) Type names.
        if matches!(occurrence, OccurrenceKind::Ident) {
            if let Some(def) = self.index.resolve_type_definition(&ident) {
                return Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Type,
                    def,
                });
            }
        }

        match occurrence {
            OccurrenceKind::MemberCall { receiver } => {
                let receiver_ty =
                    self.index
                        .resolve_receiver_type(parsed, ident_span.start, &receiver)?;
                let def = self
                    .index
                    .resolve_method_definition(&receiver_ty, &ident)?;
                Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Method,
                    def,
                })
            }
            OccurrenceKind::MemberField { receiver } => {
                let receiver_ty =
                    self.index
                        .resolve_receiver_type(parsed, ident_span.start, &receiver)?;
                let def = self.index.resolve_field_definition(&receiver_ty, &ident)?;
                Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Field,
                    def,
                })
            }
            OccurrenceKind::LocalCall => {
                let def =
                    self.index
                        .method_in_enclosing_type(file, parsed, ident_span.start, &ident)?;
                Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Method,
                    def,
                })
            }
            OccurrenceKind::Ident => {
                // Plain identifier usage could still be a type name (handled above). Anything
                // else is currently unresolved.
                None
            }
        }
    }

    pub(crate) fn scan_identifiers_in_span(
        &self,
        file: FileId,
        span: Span,
        ident: &str,
    ) -> Option<Vec<Span>> {
        let parsed = self.index.file(file)?;
        Some(scan_identifier_occurrences(&parsed.text, span, ident))
    }
}

fn classify_occurrence(text: &str, ident_span: Span) -> Option<OccurrenceKind> {
    let bytes = text.as_bytes();

    // Look backwards for `.`, allowing whitespace between `.` and the identifier.
    let mut i = ident_span.start;
    while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }

    let has_dot = i > 0 && bytes[i - 1] == b'.';
    let receiver = if has_dot {
        let dot_idx = i - 1;
        let mut recv_end = dot_idx;
        while recv_end > 0 && (bytes[recv_end - 1] as char).is_ascii_whitespace() {
            recv_end -= 1;
        }
        let mut recv_start = recv_end;
        while recv_start > 0 && is_ident_continue(bytes[recv_start - 1]) {
            recv_start -= 1;
        }
        if recv_start == recv_end {
            None
        } else {
            Some(text[recv_start..recv_end].to_string())
        }
    } else {
        None
    };

    // Look forwards for `(`, allowing whitespace between the identifier and `(`.
    let mut j = ident_span.end;
    while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
        j += 1;
    }
    let is_call = j < bytes.len() && bytes[j] == b'(';

    match (receiver, is_call) {
        (Some(receiver), true) => Some(OccurrenceKind::MemberCall { receiver }),
        (Some(receiver), false) => Some(OccurrenceKind::MemberField { receiver }),
        (None, true) => Some(OccurrenceKind::LocalCall),
        (None, false) => Some(OccurrenceKind::Ident),
    }
}

fn identifier_at(text: &str, offset: usize) -> Option<(String, Span)> {
    let bytes = text.as_bytes();
    let mut offset = offset.min(bytes.len());

    if offset == bytes.len() {
        if offset > 0 && is_ident_continue(bytes[offset - 1]) {
            offset -= 1;
        }
    } else if !is_ident_continue(bytes[offset]) {
        if offset > 0 && is_ident_continue(bytes[offset - 1]) {
            offset -= 1;
        }
    }

    if offset >= bytes.len() || !is_ident_continue(bytes[offset]) {
        return None;
    }

    let mut start = offset;
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }

    let mut end = offset + 1;
    while end < bytes.len() && is_ident_continue(bytes[end]) {
        end += 1;
    }

    if start == end {
        return None;
    }

    Some((text[start..end].to_string(), Span::new(start, end)))
}

fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

fn is_ident_continue(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
}

fn is_ident_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$')
}

pub(crate) fn scan_identifier_occurrences(text: &str, span: Span, ident: &str) -> Vec<Span> {
    let bytes = text.as_bytes();
    let start = span.start.min(bytes.len());
    let end = span.end.min(bytes.len());

    let mut out = Vec::new();
    let mut i = start;
    while i < end {
        let b = bytes[i];

        // Line comment
        if b == b'/' && i + 1 < end && bytes[i + 1] == b'/' {
            i += 2;
            while i < end && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment
        if b == b'/' && i + 1 < end && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < end {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // String literal
        if b == b'"' {
            i += 1;
            while i < end {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(end);
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Char literal (best-effort)
        if b == b'\'' {
            i += 1;
            while i < end {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(end);
                    continue;
                }
                if bytes[i] == b'\'' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if is_ident_start(b) {
            let tok_start = i;
            i += 1;
            while i < end && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let tok_end = i;
            if text.get(tok_start..tok_end) == Some(ident) {
                out.push(Span::new(tok_start, tok_end));
            }
            continue;
        }

        i += 1;
    }

    out
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
