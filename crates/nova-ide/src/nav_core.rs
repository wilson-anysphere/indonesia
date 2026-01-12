use std::collections::{HashSet, VecDeque};

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
fn is_ident_continue(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
}

#[inline]
fn is_ident_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$')
}

fn receiver_before_dot(text: &str, dot_idx: usize) -> Option<String> {
    // Best-effort support for *chained* receivers like:
    // - `a.b.c` (receiver for `c` is `a.b`)
    // - `a.b().c` (receiver for `c` is `a.b()`, but only for empty-arg calls)
    // - `new C().field` (treat `new C()` as receiver type `C`)
    //
    // We intentionally do *not* attempt to parse arbitrary expressions (calls with args,
    // parenthesized expressions, indexing, etc.).
    let bytes = text.as_bytes();
    if dot_idx == 0 || dot_idx > bytes.len() {
        return None;
    }

    let mut recv_end = dot_idx;
    while recv_end > 0 && (bytes[recv_end - 1] as char).is_ascii_whitespace() {
        recv_end -= 1;
    }

    let mut segments_rev: Vec<String> = Vec::new();
    let mut end = recv_end;
    loop {
        if end == 0 {
            return None;
        }

        // Segment can be either:
        // - identifier (`foo`)
        // - empty-arg call (`foo()`)
        let (seg_start, seg_end, seg_is_call) = if bytes.get(end - 1) == Some(&b')') {
            // Best-effort parse `foo()` (no args; allow whitespace inside parens).
            let close_paren_idx = end - 1;
            let mut open_search = close_paren_idx;
            while open_search > 0 && (bytes[open_search - 1] as char).is_ascii_whitespace() {
                open_search -= 1;
            }
            if open_search == 0 || bytes[open_search - 1] != b'(' {
                return None;
            }
            let open_paren_idx = open_search - 1;

            let mut name_end = open_paren_idx;
            while name_end > 0 && (bytes[name_end - 1] as char).is_ascii_whitespace() {
                name_end -= 1;
            }
            let mut name_start = name_end;
            while name_start > 0 && is_ident_continue(bytes[name_start - 1]) {
                name_start -= 1;
            }
            if name_start == name_end {
                return None;
            }
            if !is_ident_start(bytes[name_start]) {
                return None;
            }

            // Special-case constructor receivers like `new C().m()`:
            // treat `new C()` as a receiver of type `C`, not as a call `C()`.
            let mut is_constructor_call = false;
            let mut kw_end = name_start;
            while kw_end > 0 && (bytes[kw_end - 1] as char).is_ascii_whitespace() {
                kw_end -= 1;
            }
            if kw_end < name_start {
                let mut kw_start = kw_end;
                while kw_start > 0 && is_ident_continue(bytes[kw_start - 1]) {
                    kw_start -= 1;
                }
                let kw = text.get(kw_start..kw_end).unwrap_or("");
                if kw == "new" && (kw_start == 0 || !is_ident_continue(bytes[kw_start - 1])) {
                    is_constructor_call = true;
                }
            }

            (name_start, name_end, !is_constructor_call)
        } else {
            let mut start = end;
            while start > 0 && is_ident_continue(bytes[start - 1]) {
                start -= 1;
            }
            if start == end {
                return None;
            }
            if !is_ident_start(bytes[start]) {
                return None;
            }
            (start, end, false)
        };

        let seg = &text[seg_start..seg_end];
        segments_rev.push(if seg_is_call {
            format!("{seg}()")
        } else {
            seg.to_string()
        });

        // Skip whitespace before this segment.
        let mut i = seg_start;
        while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
            i -= 1;
        }

        // Continue only if there's a dot before the segment (with optional whitespace).
        if i > 0 && bytes[i - 1] == b'.' {
            i -= 1;
            while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
                i -= 1;
            }
            end = i;
            continue;
        }

        segments_rev.reverse();
        return Some(segments_rev.join("."));
    }
}

fn is_member_field_access(text: &str, ident_span: Span) -> Option<String> {
    // Best-effort member field access detection:
    // - The non-whitespace byte immediately before the identifier span is `.`
    // - Receiver is an identifier chain before that `.`, e.g. `a.b` in `a.b.c`
    //
    // This intentionally ignores complex receivers like calls with arguments or
    // parenthesized expressions.
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

    let segments: Vec<(&str, bool)> = receiver
        .split('.')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|seg| seg.strip_suffix("()").map(|name| (name, true)).unwrap_or((seg, false)))
        .filter(|(name, _)| !name.is_empty())
        .collect();
    let &(first, first_is_call) = segments.first()?;

    let mut cur_ty = if first_is_call {
        // Best-effort: treat receiverless calls like `foo().bar` as `this.foo().bar`.
        let this_ty = containing_type.map(|ty| ty.name.clone())?;
        resolve_method_return_type_best_effort::<TI, FTypeInfo>(lookup_type_info, &this_ty, first)?
    } else {
        match first {
            "this" => containing_type.map(|ty| ty.name.clone())?,
            "super" => containing_type.and_then(|ty| ty.super_class.clone())?,
            name => {
                // Locals/fields
                if let Some(ty) = resolve_name_type(parsed, offset, name) {
                    ty
                } else if lookup_type_info(name).is_some() {
                    // Type name (static access)
                    name.to_string()
                } else {
                    return None;
                }
            }
        }
    };

    for (seg, is_call) in segments.into_iter().skip(1) {
        cur_ty = if is_call {
            resolve_method_return_type_best_effort::<TI, FTypeInfo>(lookup_type_info, &cur_ty, seg)?
        } else {
            resolve_field_declared_type_best_effort::<TI, FTypeInfo>(lookup_type_info, &cur_ty, seg)?
        };
    }

    Some(cur_ty)
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
    fn go<'a, TI, FTypeInfo>(
        lookup_type_info: &'a FTypeInfo,
        ty_name: &str,
        field_name: &str,
        seen: &mut HashSet<String>,
    ) -> Option<String>
    where
        TI: NavTypeInfo + 'a,
        FTypeInfo: Fn(&str) -> Option<&'a TI>,
    {
        if !seen.insert(ty_name.to_string()) {
            return None;
        }

        let info = lookup_type_info(ty_name)?;
        if let Some(field) = info.def().fields.iter().find(|f| f.name == field_name) {
            return Some(field.ty.clone());
        }

        if let Some(super_name) = info.def().super_class.as_deref() {
            if let Some(found) = go::<TI, FTypeInfo>(lookup_type_info, super_name, field_name, seen)
            {
                return Some(found);
            }
        }

        for iface in &info.def().interfaces {
            if let Some(found) = go::<TI, FTypeInfo>(lookup_type_info, iface, field_name, seen) {
                return Some(found);
            }
        }

        None
    }

    go::<TI, FTypeInfo>(
        lookup_type_info,
        receiver_ty,
        field_name,
        &mut HashSet::new(),
    )
}

pub(crate) fn resolve_method_return_type_best_effort<'a, TI, FTypeInfo>(
    lookup_type_info: &'a FTypeInfo,
    receiver_ty: &str,
    method_name: &str,
) -> Option<String>
where
    TI: NavTypeInfo + 'a,
    FTypeInfo: Fn(&str) -> Option<&'a TI>,
{
    fn go<'a, TI, FTypeInfo>(
        lookup_type_info: &'a FTypeInfo,
        ty_name: &str,
        method_name: &str,
        seen: &mut HashSet<String>,
    ) -> Option<String>
    where
        TI: NavTypeInfo + 'a,
        FTypeInfo: Fn(&str) -> Option<&'a TI>,
    {
        if !seen.insert(ty_name.to_string()) {
            return None;
        }

        let info = lookup_type_info(ty_name)?;
        if let Some(method) = info.def().methods.iter().find(|m| m.name == method_name) {
            let ret = method.ret_ty.clone()?;
            if ret == "void" {
                return None;
            }
            return Some(ret);
        }

        if let Some(super_name) = info.def().super_class.as_deref() {
            if let Some(found) =
                go::<TI, FTypeInfo>(lookup_type_info, super_name, method_name, seen)
            {
                return Some(found);
            }
        }

        for iface in &info.def().interfaces {
            if let Some(found) = go::<TI, FTypeInfo>(lookup_type_info, iface, method_name, seen) {
                return Some(found);
            }
        }

        None
    }

    go::<TI, FTypeInfo>(
        lookup_type_info,
        receiver_ty,
        method_name,
        &mut HashSet::new(),
    )
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
    // 1) Walk the class/superclass chain first, collecting implemented interfaces.
    //
    // We only treat methods with bodies as "definitions" (this matches prior behavior
    // and is critical for interface `default` methods).
    //
    // If no class/superclass definition exists, we fall back to searching interfaces
    // transitively (superinterfaces) in a deterministic breadth-first order.
    let mut seen_types: HashSet<String> = HashSet::new();
    let mut interface_roots: Vec<String> = Vec::new();

    let mut cur = ty_name.to_string();
    loop {
        if !seen_types.insert(cur.clone()) {
            // Cycle in the superclass chain.
            break;
        }

        let Some(info) = lookup_type_info(&cur) else {
            break;
        };

        if let Some(method) = info
            .def()
            .methods
            .iter()
            .find(|m| m.name == method_name && m.body_span.is_some())
        {
            return Some((info.uri().clone(), method.name_span));
        }

        // Record interfaces for later lookup (after class-chain resolution fails),
        // preserving declaration order across the class chain.
        interface_roots.extend(info.def().interfaces.iter().cloned());

        let Some(next) = info.def().super_class.clone() else {
            break;
        };
        cur = next;
    }

    // 2) No class/superclass definition was found. Search interfaces transitively for
    // a `default` method definition.
    let mut visited_ifaces: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    for iface in interface_roots {
        if visited_ifaces.insert(iface.clone()) {
            queue.push_back(iface);
        }
    }

    while let Some(iface) = queue.pop_front() {
        let Some(info) = lookup_type_info(&iface) else {
            continue;
        };

        if let Some(method) = info
            .def()
            .methods
            .iter()
            .find(|m| m.name == method_name && m.body_span.is_some())
        {
            return Some((info.uri().clone(), method.name_span));
        }

        for super_iface in &info.def().interfaces {
            if visited_ifaces.insert(super_iface.clone()) {
                queue.push_back(super_iface.clone());
            }
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

    let receiver_is_super = call.receiver == "super";
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
    //
    // `super.method()` is also statically dispatched (to a specific superclass implementation),
    // so treat it like a final receiver here.
    let receiver_is_final =
        receiver_is_static_type || receiver_is_super || receiver_info.def().modifiers.is_final;

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
    let mut queue: VecDeque<String> = type_info.def().interfaces.clone().into();
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
    let mut seen: HashSet<String> = HashSet::new();
    let mut cur = type_info.def().super_class.clone();
    while let Some(next) = cur {
        if !seen.insert(next.clone()) {
            // Cycle in the superclass chain.
            break;
        }

        // For best-effort `textDocument/declaration` on overrides, consider any matching
        // superclass method a valid target, even if it has a body (i.e. is "concrete").
        if let Some(loc) =
            method_in_type_any::<TI, FTypeInfo, FFile>(lookup_type_info, file, &next, method_name)
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

fn method_in_type_any<'a, TI, FTypeInfo, FFile>(
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
    let parsed = file(type_info.uri())?;
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
