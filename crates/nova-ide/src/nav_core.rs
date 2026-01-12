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
    let info = lookup_type_info(ty_name)?;
    if let Some(method) = info
        .def()
        .methods
        .iter()
        .find(|m| m.name == method_name && m.body_span.is_some())
    {
        return Some((info.uri().clone(), method.name_span));
    }

    let super_name = info.def().super_class.as_deref()?;
    resolve_method_definition::<TI, FTypeInfo>(lookup_type_info, super_name, method_name)
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

    let method = type_info.def().methods.iter().find(|m| m.name == method_name);
    let is_abstract = type_info.def().kind == TypeKind::Interface
        || method.is_some_and(|m| m.is_abstract || m.body_span.is_none());
    if !is_abstract {
        return Vec::new();
    }

    let mut out = Vec::new();
    for subtype in inheritance.all_subtypes(ty_name) {
        let Some((uri, span)) =
            resolve_method_definition::<TI, FTypeInfo>(lookup_type_info, &subtype, method_name)
        else {
            continue;
        };
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

    let receiver_ty = if call.receiver == "this" {
        Some(containing_type.name.clone())
    } else if call.receiver == "super" {
        containing_type.super_class.clone()
    } else {
        resolve_name_type(parsed, offset, &call.receiver)
    };

    let Some(receiver_ty) = receiver_ty else {
        return Vec::new();
    };

    let Some(receiver_info) = lookup_type_info(&receiver_ty) else {
        return Vec::new();
    };

    let receiver_is_final = receiver_info.def().modifiers.is_final;

    let mut candidates: Vec<(Uri, Span)> = Vec::new();

    if let Some(def) = resolve_method_definition::<TI, FTypeInfo>(
        lookup_type_info,
        &receiver_ty,
        &call.method,
    )
    {
        candidates.push(def);
    }

    if !receiver_is_final {
        for subtype in inheritance.all_subtypes(&receiver_ty) {
            if let Some(def) = resolve_method_definition::<TI, FTypeInfo>(
                lookup_type_info,
                &subtype,
                &call.method,
            )
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
    for iface in &type_info.def().interfaces {
        if let Some(loc) = declaration_in_type::<TI, FTypeInfo, FFile>(
            lookup_type_info,
            file,
            iface,
            method_name,
        )
        {
            return Some(loc);
        }
    }

    // Then walk the superclass chain.
    let mut cur = type_info.def().super_class.clone();
    while let Some(next) = cur {
        if let Some(loc) = declaration_in_type::<TI, FTypeInfo, FFile>(
            lookup_type_info,
            file,
            &next,
            method_name,
        )
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

    let is_declaration =
        type_info.def().kind == TypeKind::Interface || method.is_abstract || method.body_span.is_none();
    if !is_declaration {
        return None;
    }

    let parsed = file(type_info.uri())?;
    Some(Location {
        uri: type_info.uri().clone(),
        range: span_to_lsp_range_with_index(&parsed.line_index, &parsed.text, method.name_span),
    })
}
