use std::collections::{BTreeSet, HashSet};

use lsp_types::{Location, Uri};
use nova_index::InheritanceIndex;
use nova_types::Span;

use crate::parse::{CallSite, ParsedFile, TypeDef, TypeKind};
use crate::text::span_to_lsp_range_with_index;

pub(crate) trait NavTypeInfo {
    fn uri(&self) -> &Uri;
    fn def(&self) -> &TypeDef;
}

impl NavTypeInfo for crate::db::TypeInfo {
    fn uri(&self) -> &Uri {
        &self.uri
    }

    fn def(&self) -> &TypeDef {
        &self.def
    }
}

#[inline]
pub(crate) fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

pub(crate) fn identifier_at(text: &str, offset: usize) -> Option<(String, Span)> {
    if offset > text.len() {
        return None;
    }

    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            start -= 1;
        } else {
            break;
        }
    }

    let mut end = offset;
    while end < bytes.len() {
        let ch = bytes[end] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
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

#[inline]
fn is_ident_char(b: u8) -> bool {
    let ch = b as char;
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}

fn receiver_before_dot(text: &str, dot_idx: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if dot_idx == 0 || dot_idx > bytes.len() {
        return None;
    }

    let mut recv_end = dot_idx;
    while recv_end > 0 && (bytes[recv_end - 1] as char).is_ascii_whitespace() {
        recv_end -= 1;
    }

    let mut recv_start = recv_end;
    while recv_start > 0 && is_ident_char(bytes[recv_start - 1]) {
        recv_start -= 1;
    }

    if recv_start == recv_end {
        None
    } else {
        Some(text[recv_start..recv_end].to_string())
    }
}

fn is_member_field_access(text: &str, ident_span: Span) -> Option<String> {
    // Best-effort member field access detection:
    // - The non-whitespace byte immediately before the identifier span is `.`
    // - Receiver is a single identifier token before that `.`
    //
    // This intentionally ignores complex receivers like `foo().bar` or `a.b.c`.
    let bytes = text.as_bytes();

    let mut i = ident_span.start;
    while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'.' {
        return None;
    }
    let dot_idx = i - 1;

    // If the identifier is followed by `(` (allowing whitespace), it's a member call, not a field.
    let mut j = ident_span.end;
    while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'(' {
        return None;
    }

    receiver_before_dot(text, dot_idx)
}

pub(crate) fn method_decl_at(parsed: &ParsedFile, offset: usize) -> Option<(String, String)> {
    for ty in &parsed.types {
        for m in &ty.methods {
            if span_contains(m.name_span, offset) {
                return Some((ty.name.clone(), m.name.clone()));
            }
        }
    }
    None
}

pub(crate) fn resolve_name_type(parsed: &ParsedFile, offset: usize, name: &str) -> Option<String> {
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

pub(crate) fn resolve_receiver_type_best_effort<'a, TI, FTypeInfo>(
    lookup_type_info: &'a FTypeInfo,
    parsed: &ParsedFile,
    offset: usize,
    receiver: &str,
) -> Option<String>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
{
    let containing_type = parsed
        .types
        .iter()
        .find(|ty| span_contains(ty.body_span, offset));

    if receiver == "this" {
        return containing_type.map(|ty| ty.name.clone());
    }
    if receiver == "super" {
        return containing_type.and_then(|ty| ty.super_class.clone());
    }

    // Locals/fields
    if let Some(ty) = resolve_name_type(parsed, offset, receiver) {
        return Some(ty);
    }

    // Type name (static access)
    if lookup_type_info(receiver).is_some() {
        return Some(receiver.to_string());
    }

    None
}

pub(crate) fn resolve_field_declared_type_best_effort<'a, TI, FTypeInfo>(
    lookup_type_info: &'a FTypeInfo,
    receiver_ty: &str,
    field_name: &str,
) -> Option<String>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
{
    let mut cur = receiver_ty.to_string();
    let mut seen: HashSet<String> = HashSet::new();

    loop {
        if !seen.insert(cur.clone()) {
            // Cycle in the superclass chain.
            return None;
        }

        let info = lookup_type_info(&cur)?;
        if let Some(field) = info.def().fields.iter().find(|f| f.name == field_name) {
            return Some(field.ty.clone());
        }

        let Some(next) = info.def().super_class.clone() else {
            return None;
        };
        cur = next;
    }
}

fn type_info_name_location<'a, TI, FTypeInfo, FFile>(
    lookup_type_info: &'a FTypeInfo,
    file: &'a FFile,
    ty_name: &str,
) -> Option<Location>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
    FFile: Fn(&Uri) -> Option<&'a ParsedFile>,
{
    let info = lookup_type_info(ty_name)?;
    let def_file = file(info.uri())?;
    Some(Location {
        uri: info.uri().clone(),
        range: span_to_lsp_range_with_index(
            &def_file.line_index,
            &def_file.text,
            info.def().name_span,
        ),
    })
}

/// Best-effort `textDocument/typeDefinition` shared between FileId-based databases and
/// `DatabaseSnapshot`.
pub(crate) fn type_definition_best_effort<'a, TI, FTypeInfo, FFile>(
    lookup_type_info: &'a FTypeInfo,
    file: &'a FFile,
    parsed: &ParsedFile,
    offset: usize,
) -> Option<Location>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
    FFile: Fn(&Uri) -> Option<&'a ParsedFile>,
{
    let (ident, ident_span) = identifier_at(&parsed.text, offset)?;

    // Member field access: `recv.field` -> type definition of the field's declared type.
    if let Some(receiver) = is_member_field_access(&parsed.text, ident_span) {
        if let Some(receiver_ty) = resolve_receiver_type_best_effort::<TI, FTypeInfo>(
            lookup_type_info,
            parsed,
            ident_span.start,
            &receiver,
        ) {
            if let Some(field_ty) = resolve_field_declared_type_best_effort::<TI, FTypeInfo>(
                lookup_type_info,
                &receiver_ty,
                &ident,
            ) {
                // If the field's type is external/JDK, return `None` so the LSP layer can
                // provide a separate fallback.
                return type_info_name_location::<TI, FTypeInfo, FFile>(
                    lookup_type_info,
                    file,
                    &field_ty,
                );
            }
        }
    }

    // Type name under cursor.
    if let Some(loc) =
        type_info_name_location::<TI, FTypeInfo, FFile>(lookup_type_info, file, &ident)
    {
        return Some(loc);
    }

    // Local/field variable under cursor.
    let ty = resolve_name_type(parsed, offset, &ident)?;
    type_info_name_location::<TI, FTypeInfo, FFile>(lookup_type_info, file, &ty)
}

pub(crate) fn variable_declaration(
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

pub(crate) fn resolve_method_definition<'a, TI, FTypeInfo>(
    lookup_type_info: &'a FTypeInfo,
    ty_name: &str,
    method_name: &str,
) -> Option<(Uri, Span)>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
{
    let mut visited = BTreeSet::new();
    resolve_method_definition_inner::<TI, FTypeInfo>(
        lookup_type_info,
        ty_name,
        method_name,
        &mut visited,
    )
}

fn resolve_method_definition_inner<'a, TI, FTypeInfo>(
    lookup_type_info: &'a FTypeInfo,
    ty_name: &str,
    method_name: &str,
    visited: &mut BTreeSet<String>,
) -> Option<(Uri, Span)>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
{
    if !visited.insert(ty_name.to_string()) {
        return None;
    }

    let info = lookup_type_info(ty_name)?;
    if let Some(method) = info
        .def()
        .methods
        .iter()
        .find(|m| m.name == method_name && m.body_span.is_some())
    {
        return Some((info.uri().clone(), method.name_span));
    }

    // Prefer walking the superclass chain before interfaces (class methods win over
    // interface default methods in Java).
    if let Some(super_name) = info.def().super_class.as_deref() {
        if let Some(def) = resolve_method_definition_inner::<TI, FTypeInfo>(
            lookup_type_info,
            super_name,
            method_name,
            visited,
        ) {
            return Some(def);
        }
    }

    // If the method isn't found on the class/superclass chain, try interfaces.
    // For interfaces, `TypeDef.interfaces` represents extended interfaces.
    for iface in &info.def().interfaces {
        if let Some(def) = resolve_method_definition_inner::<TI, FTypeInfo>(
            lookup_type_info,
            iface,
            method_name,
            visited,
        ) {
            return Some(def);
        }
    }

    None
}

pub(crate) fn implementation_for_type<'a, TI, FTypeInfo, FFile>(
    inheritance: &InheritanceIndex,
    lookup_type_info: &'a FTypeInfo,
    file: &'a FFile,
    type_name: &str,
) -> Vec<Location>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
    FFile: Fn(&Uri) -> Option<&'a ParsedFile>,
{
    let mut out = Vec::new();
    for subtype in inheritance.all_subtypes(type_name) {
        let Some(type_info) = lookup_type_info(&subtype) else {
            continue;
        };
        let Some(parsed) = file(type_info.uri()) else {
            continue;
        };
        out.push(Location {
            uri: type_info.uri().clone(),
            range: span_to_lsp_range_with_index(
                &parsed.line_index,
                &parsed.text,
                type_info.def().name_span,
            ),
        });
    }
    out
}

pub(crate) fn implementation_for_abstract_method<'a, TI, FTypeInfo, FFile>(
    inheritance: &InheritanceIndex,
    lookup_type_info: &'a FTypeInfo,
    file: &'a FFile,
    ty_name: &str,
    method_name: &str,
) -> Vec<Location>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
    FFile: Fn(&Uri) -> Option<&'a ParsedFile>,
{
    let Some(type_info) = lookup_type_info(ty_name) else {
        return Vec::new();
    };

    // A final type cannot have subtypes, so it cannot have overrides.
    if type_info.def().modifiers.is_final {
        return Vec::new();
    }

    // We treat "implementations" on a method declaration as *overrides* in subtypes.
    // This applies to abstract/interface methods as well as concrete methods (including
    // interface `default` methods).
    //
    // Note: We intentionally do not include the base method itself in the results.
    let base_span = type_info
        .def()
        .methods
        .iter()
        .find(|m| m.name == method_name)
        .map(|m| (type_info.uri().clone(), m.name_span));

    let mut out = Vec::new();
    for subtype in inheritance.all_subtypes(ty_name) {
        let Some((uri, span)) =
            resolve_method_definition::<TI, FTypeInfo>(lookup_type_info, &subtype, method_name)
        else {
            continue;
        };
        if base_span
            .as_ref()
            .is_some_and(|(base_uri, base_span)| *base_uri == uri && base_span.start == span.start)
        {
            continue;
        }
        let Some(parsed) = file(&uri) else {
            continue;
        };
        out.push(Location {
            uri,
            range: span_to_lsp_range_with_index(&parsed.line_index, &parsed.text, span),
        });
    }

    out.sort_by(|a, b| a.uri.to_string().cmp(&b.uri.to_string()));
    out.dedup_by(|a, b| a.uri == b.uri && a.range.start == b.range.start);
    out
}

pub(crate) fn implementation_for_call<'a, TI, FTypeInfo, FFile, FFallback>(
    inheritance: &InheritanceIndex,
    lookup_type_info: &'a FTypeInfo,
    file: &'a FFile,
    parsed: &ParsedFile,
    offset: usize,
    call: &CallSite,
    fallback: &FFallback,
) -> Vec<Location>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
    FFile: Fn(&Uri) -> Option<&'a ParsedFile>,
    FFallback: Fn(&str, &str) -> Option<(Uri, Span)>,
{
    let Some(containing_type) = parsed
        .types
        .iter()
        .find(|ty| span_contains(ty.body_span, offset))
    else {
        return Vec::new();
    };

    let mut receiver_is_static_type = false;
    let receiver_ty = if call.receiver == "this" {
        Some(containing_type.name.clone())
    } else if call.receiver == "super" {
        containing_type.super_class.clone()
    } else {
        resolve_name_type(parsed, offset, &call.receiver).or_else(|| {
            if lookup_type_info(&call.receiver).is_some() {
                receiver_is_static_type = true;
                Some(call.receiver.clone())
            } else {
                None
            }
        })
    };

    let Some(receiver_ty) = receiver_ty else {
        return Vec::new();
    };

    let Some(receiver_info) = lookup_type_info(&receiver_ty) else {
        return Vec::new();
    };

    // Static dispatch: `Type.method()` is resolved at compile-time, so we should not
    // walk subtypes as we do for virtual dispatch.
    let receiver_is_final = receiver_is_static_type || receiver_info.def().modifiers.is_final;

    let mut candidates: Vec<(Uri, Span)> = Vec::new();

    if let Some(def) =
        resolve_method_definition::<TI, FTypeInfo>(lookup_type_info, &receiver_ty, &call.method)
    {
        candidates.push(def);
    }

    if !receiver_is_final {
        for subtype in inheritance.all_subtypes(&receiver_ty) {
            if let Some(def) =
                resolve_method_definition::<TI, FTypeInfo>(lookup_type_info, &subtype, &call.method)
            {
                candidates.push(def);
            }
        }
    }

    if candidates.is_empty() {
        if let Some((uri, span)) = fallback(&receiver_ty, &call.method) {
            candidates.push((uri, span));
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
            let file = file(&uri)?;
            Some(Location {
                uri,
                range: span_to_lsp_range_with_index(&file.line_index, &file.text, span),
            })
        })
        .collect()
}

pub(crate) fn declaration_for_override<'a, TI, FTypeInfo, FFile>(
    lookup_type_info: &'a FTypeInfo,
    file: &'a FFile,
    ty_name: &str,
    method_name: &str,
) -> Option<Location>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
    FFile: Fn(&Uri) -> Option<&'a ParsedFile>,
{
    let type_info = lookup_type_info(ty_name)?;

    // Prefer interface declarations.
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: std::collections::VecDeque<String> = type_info.def().interfaces.clone().into();
    visited.extend(queue.iter().cloned());

    while let Some(iface) = queue.pop_front() {
        // Prefer the closest declaration (walk interfaces breadth-first).
        if let Some(loc) =
            declaration_in_type::<TI, FTypeInfo, FFile>(lookup_type_info, file, &iface, method_name)
        {
            return Some(loc);
        }

        // Recursively search extended interfaces (transitively).
        let Some(iface_info) = lookup_type_info(&iface) else {
            continue;
        };
        for super_iface in &iface_info.def().interfaces {
            if visited.insert(super_iface.clone()) {
                queue.push_back(super_iface.clone());
            }
        }
    }

    // Then walk the superclass chain.
    let mut cur = type_info.def().super_class.clone();
    while let Some(next) = cur {
        if let Some(loc) =
            declaration_in_type::<TI, FTypeInfo, FFile>(lookup_type_info, file, &next, method_name)
        {
            return Some(loc);
        }
        cur = lookup_type_info(&next).and_then(|info| info.def().super_class.clone());
    }

    // Locals/fields/methods default: declaration == definition.
    let parsed = file(type_info.uri())?;
    let method = type_info
        .def()
        .methods
        .iter()
        .find(|m| m.name == method_name)?;
    Some(Location {
        uri: type_info.uri().clone(),
        range: span_to_lsp_range_with_index(&parsed.line_index, &parsed.text, method.name_span),
    })
}

pub(crate) fn declaration_in_type<'a, TI, FTypeInfo, FFile>(
    lookup_type_info: &'a FTypeInfo,
    file: &'a FFile,
    ty_name: &str,
    method_name: &str,
) -> Option<Location>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
    FFile: Fn(&Uri) -> Option<&'a ParsedFile>,
{
    let type_info = lookup_type_info(ty_name)?;
    let method = type_info
        .def()
        .methods
        .iter()
        .find(|m| m.name == method_name)?;

    let is_declaration = type_info.def().kind == TypeKind::Interface
        || method.is_abstract
        || method.body_span.is_none();
    if !is_declaration {
        return None;
    }

    let parsed = file(type_info.uri())?;
    Some(Location {
        uri: type_info.uri().clone(),
        range: span_to_lsp_range_with_index(&parsed.line_index, &parsed.text, method.name_span),
    })
}
