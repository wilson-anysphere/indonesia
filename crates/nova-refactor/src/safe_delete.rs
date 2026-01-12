use std::collections::BTreeMap;

use nova_index::{
    Index, ReferenceCandidate, ReferenceKind, SymbolId as IndexSymbolId, SymbolKind,
    TextRange as IndexTextRange,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::edit::{
    FileId, TextEdit as WorkspaceTextEdit, TextRange as WorkspaceTextRange, WorkspaceEdit,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SafeDeleteMode {
    Safe,
    DeleteAnyway,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "camelCase")]
pub enum SafeDeleteTarget {
    Symbol(IndexSymbolId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub file: String,
    pub range: IndexTextRange,
    pub replacement: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
pub enum UsageKind {
    Call,
    FieldAccess,
    TypeUsage,
    Override,
    Implements,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub file: String,
    pub range: IndexTextRange,
    pub kind: UsageKind,
}

/// Serializable snapshot of a symbol targeted by Safe Delete.
///
/// We intentionally avoid re-exporting `nova-index`'s internal symbol types here
/// because `nova-index` contains multiple symbol representations (search index vs
/// sketch parser) and not all of them are `serde`-friendly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeDeleteSymbol {
    pub id: IndexSymbolId,
    pub kind: SymbolKind,
    pub name: String,
    pub container: Option<String>,
    pub file: String,
    pub name_range: IndexTextRange,
    pub decl_range: IndexTextRange,
    pub is_override: bool,
    pub extends: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeDeleteReport {
    pub target: SafeDeleteSymbol,
    pub usages: Vec<Usage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeDeleteOutcome {
    Applied { edit: WorkspaceEdit },
    Preview { report: SafeDeleteReport },
}

#[derive(Debug, Error)]
pub enum SafeDeleteError {
    #[error("target symbol not found")]
    TargetNotFound,
    #[error("file `{0}` not found")]
    FileNotFound(String),
    #[error("unsupported symbol kind: {0:?}")]
    UnsupportedSymbolKind(SymbolKind),
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

/// Execute Safe Delete.
///
/// The function uses `nova-index` for broad usage discovery and then performs a
/// semantic verification pass to eliminate false positives.
pub fn safe_delete(
    index: &Index,
    target: SafeDeleteTarget,
    mode: SafeDeleteMode,
) -> Result<SafeDeleteOutcome, SafeDeleteError> {
    let symbol_id = match target {
        SafeDeleteTarget::Symbol(id) => id,
    };
    let target_symbol = index
        .find_symbol(symbol_id)
        .cloned()
        .ok_or(SafeDeleteError::TargetNotFound)?;

    let target_snapshot = SafeDeleteSymbol {
        id: target_symbol.id,
        kind: target_symbol.kind,
        name: target_symbol.name.clone(),
        container: target_symbol.container.clone(),
        file: target_symbol.file.clone(),
        name_range: target_symbol.name_range,
        decl_range: target_symbol.decl_range,
        is_override: target_symbol.is_override,
        extends: target_symbol.extends.clone(),
    };

    match target_snapshot.kind {
        SymbolKind::Method => safe_delete_method(index, &target_snapshot, mode),
        other => Err(SafeDeleteError::UnsupportedSymbolKind(other)),
    }
}

fn safe_delete_method(
    index: &Index,
    target: &SafeDeleteSymbol,
    mode: SafeDeleteMode,
) -> Result<SafeDeleteOutcome, SafeDeleteError> {
    let candidates = index.find_name_candidates(&target.name);

    let mut usages: Vec<Usage> = Vec::new();

    for candidate in candidates {
        // Exclude the declaration itself.
        if candidate.file == target.file && ranges_overlap(candidate.range, target.decl_range) {
            continue;
        }

        if let Some(usage) = verify_method_usage(index, target, &candidate) {
            usages.push(usage);
        }
    }

    // Include overrides as usages (deleting base method breaks @Override sites).
    usages.extend(find_override_usages(index, target));

    usages.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    usages.dedup_by(|a, b| a.file == b.file && a.range == b.range && a.kind == b.kind);

    if usages.is_empty() {
        let mut edit = WorkspaceEdit::new(vec![delete_range_workspace_edit(
            &target.file,
            target.decl_range,
        )]);
        edit.normalize()?;
        return Ok(SafeDeleteOutcome::Applied { edit });
    }

    match mode {
        SafeDeleteMode::Safe => Ok(SafeDeleteOutcome::Preview {
            report: SafeDeleteReport {
                target: target.clone(),
                usages,
            },
        }),
        SafeDeleteMode::DeleteAnyway => {
            let mut edits: Vec<WorkspaceTextEdit> = Vec::new();
            // Best-effort: delete each usage statement (call) and then delete the declaration.
            for usage in &usages {
                if usage.file == target.file && ranges_overlap(usage.range, target.decl_range) {
                    continue;
                }
                if usage.kind == UsageKind::Override {
                    if let Some(sym) = index.symbols().iter().find(|sym| {
                        sym.kind == SymbolKind::Method
                            && sym.file == usage.file
                            && sym.name_range == usage.range
                    }) {
                        edits.push(delete_range_workspace_edit(&usage.file, sym.decl_range));
                        continue;
                    }
                }
                if let Some(text) = index.file_text(&usage.file) {
                    if let Some(range) = best_effort_delete_usage(text, usage.range) {
                        edits.push(delete_range_workspace_edit(&usage.file, range));
                    } else {
                        edits.push(delete_range_workspace_edit(&usage.file, usage.range));
                    }
                }
            }
            edits.push(delete_range_workspace_edit(&target.file, target.decl_range));

            merge_overlapping_deletes(&mut edits);

            let mut edit = WorkspaceEdit::new(edits);
            edit.normalize()?;
            Ok(SafeDeleteOutcome::Applied { edit })
        }
    }
}

fn delete_range_workspace_edit(file: &str, range: IndexTextRange) -> WorkspaceTextEdit {
    WorkspaceTextEdit::delete(
        FileId::new(file),
        WorkspaceTextRange::new(range.start, range.end),
    )
}

fn merge_overlapping_deletes(edits: &mut Vec<WorkspaceTextEdit>) {
    edits.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.range.end.cmp(&b.range.end))
    });

    let mut merged: Vec<WorkspaceTextEdit> = Vec::with_capacity(edits.len());
    for edit in edits.drain(..) {
        if edit.replacement.is_empty() {
            if let Some(last) = merged.last_mut() {
                if last.replacement.is_empty()
                    && last.file == edit.file
                    && edit.range.start <= last.range.end
                {
                    last.range.end = last.range.end.max(edit.range.end);
                    continue;
                }
            }
        }

        merged.push(edit);
    }

    *edits = merged;
}

fn ranges_overlap(a: IndexTextRange, b: IndexTextRange) -> bool {
    a.start < b.end && b.start < a.end
}

fn verify_method_usage(
    index: &Index,
    target: &SafeDeleteSymbol,
    candidate: &ReferenceCandidate,
) -> Option<Usage> {
    let text = index.file_text(&candidate.file)?;
    let kind = match candidate.kind {
        ReferenceKind::Call => UsageKind::Call,
        ReferenceKind::FieldAccess => UsageKind::FieldAccess,
        ReferenceKind::TypeUsage => UsageKind::TypeUsage,
        ReferenceKind::Override => UsageKind::Override,
        ReferenceKind::Implements => UsageKind::Implements,
        ReferenceKind::Unknown => UsageKind::Unknown,
    };

    // Semantic verification: ensure the occurrence is actually a call expression that
    // resolves to the target method's declaring class.
    if kind != UsageKind::Call {
        return None;
    }

    // `find_name_candidates` will also classify method declarations as `Call` candidates
    // because they're followed by `(`. Those are not usages.
    if index.symbols().iter().any(|sym| {
        sym.kind == SymbolKind::Method
            && sym.file == candidate.file
            && sym.name_range == candidate.range
    }) {
        return None;
    }

    let open_paren = call_open_paren_offset(text, candidate.range.end)?;
    let arg_count = parse_argument_count(text, open_paren);

    // Arity-aware verification: if we can compute both the call-site argument count and the
    // target method arity, a mismatch means this is definitely a different overload.
    if let (Some(arg_count), Some(target_arity)) = (
        arg_count,
        index.method_param_types(target.id).map(|tys| tys.len()),
    ) {
        if arg_count != target_arity {
            return None;
        }
    }

    let receiver = parse_receiver_expression(text, candidate.range.start);

    let receiver_class = match receiver {
        Receiver::ImplicitThis | Receiver::This => {
            enclosing_class_at_offset(index, &candidate.file, candidate.range.start)
        }
        Receiver::New(class_name) | Receiver::TypeName(class_name) => Some(class_name),
        Receiver::Var(var_name) => infer_var_type_in_scope(text, candidate.range.start, &var_name),
        Receiver::Unknown => None,
    }?;

    let matches_target = match arg_count {
        Some(arg_count) => {
            let candidates =
                collect_overload_candidates_by_arity(index, &receiver_class, &target.name, arg_count);
            match_overload_candidate_set(&candidates, target.id)
                .or_else(|| {
                    // Best-effort fallback: if overload lookup fails, fall back to name-only
                    // resolution.
                    let candidates =
                        collect_overload_candidates_by_name(index, &receiver_class, &target.name);
                    match_overload_candidate_set(&candidates, target.id)
                })
                .unwrap_or(false)
        }
        None => {
            // Best-effort fallback: if we can't parse argument count, fall back to name-only
            // resolution.
            let candidates = collect_overload_candidates_by_name(index, &receiver_class, &target.name);
            match_overload_candidate_set(&candidates, target.id).unwrap_or(false)
        }
    };

    if !matches_target {
        return None;
    }

    Some(Usage {
        file: candidate.file.clone(),
        range: candidate.range,
        kind,
    })
}

fn find_override_usages(index: &Index, target: &SafeDeleteSymbol) -> Vec<Usage> {
    if target.container.is_none() {
        return Vec::new();
    };
    let mut out = Vec::new();
    let target_param_types = index.method_param_types(target.id);
    let target_arity = target_param_types.map(|tys| tys.len());
    for sym in index.symbols() {
        if sym.kind != SymbolKind::Method || sym.name != target.name || !sym.is_override {
            continue;
        }

        // Only consider `@Override` declarations that match the target signature.
        let sym_param_types = index.method_param_types(sym.id);
        let sym_arity = sym_param_types.map(|tys| tys.len());
        if let (Some(target_param_types), Some(sym_param_types)) = (target_param_types, sym_param_types)
        {
            if target_param_types != sym_param_types {
                continue;
            }
        } else if let (Some(target_arity), Some(sym_arity)) = (target_arity, sym_arity)
        {
            if target_arity != sym_arity {
                continue;
            }
        }

        let Some(class_name) = sym.container.as_deref() else {
            continue;
        };
        let Some(base_class) = index.class_extends(class_name) else {
            continue;
        };

        let Some(overridden_candidates) = resolve_overridden_method_candidates(
            index,
            base_class,
            &sym.name,
            sym_param_types,
            sym_arity,
        ) else {
            continue;
        };

        let matches_target =
            match_overload_candidate_set(&overridden_candidates, target.id).unwrap_or(false);
        if !matches_target {
            continue;
        }
        out.push(Usage {
            file: sym.file.clone(),
            range: sym.name_range,
            kind: UsageKind::Override,
        });
    }
    out
}

fn call_open_paren_offset(text: &str, mut offset: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    while offset < bytes.len() && bytes[offset].is_ascii_whitespace() {
        offset += 1;
    }
    match bytes.get(offset) {
        Some(b'(') => Some(offset),
        _ => None,
    }
}

fn parse_argument_count(text: &str, open_paren: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open_paren) != Some(&b'(') {
        return None;
    }

    let mut i = open_paren + 1;
    let mut paren_depth = 1usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut angle_depth = 0usize;

    let mut count = 0usize;
    let mut seen_token = false;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                seen_token = true;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
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
            b'\'' => {
                seen_token = true;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
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
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            b'(' => {
                paren_depth += 1;
                seen_token = true;
                i += 1;
                continue;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                if paren_depth == 0 {
                    if seen_token {
                        count += 1;
                    }
                    return Some(count);
                }
                seen_token = true;
                i += 1;
                continue;
            }
            b'{' => {
                brace_depth += 1;
                seen_token = true;
                i += 1;
                continue;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                seen_token = true;
                i += 1;
                continue;
            }
            b'[' => {
                bracket_depth += 1;
                seen_token = true;
                i += 1;
                continue;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                seen_token = true;
                i += 1;
                continue;
            }
            b'<' => {
                if angle_depth > 0 {
                    angle_depth += 1;
                    seen_token = true;
                    i += 1;
                    continue;
                }

                // Only treat `<` / `>` as generic delimiters when at the top level of the call
                // argument list; nested parens/braces/brackets already suppress comma counting.
                if paren_depth == 1
                    && brace_depth == 0
                    && bracket_depth == 0
                    && looks_like_generic_argument_list(text, i)
                {
                    angle_depth = 1;
                    seen_token = true;
                    i += 1;
                    continue;
                }

                seen_token = true;
                i += 1;
                continue;
            }
            b'>' => {
                if angle_depth > 0 {
                    angle_depth = angle_depth.saturating_sub(1);
                }
                seen_token = true;
                i += 1;
                continue;
            }
            b',' if paren_depth == 1 && brace_depth == 0 && bracket_depth == 0 && angle_depth == 0 => {
                count += 1;
                seen_token = false;
                i += 1;
                continue;
            }
            b if b.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            _ => {
                seen_token = true;
                i += 1;
                continue;
            }
        }
    }

    None
}

fn looks_like_generic_argument_list(text: &str, lt_offset: usize) -> bool {
    let bytes = text.as_bytes();
    if bytes.get(lt_offset) != Some(&b'<') {
        return false;
    }

    // Scan forward until we either:
    // - find a matching `>` (=> looks like generics), or
    // - hit a top-level `,` / `)` (=> likely a comparison operator).
    let mut i = lt_offset + 1;
    let mut paren_depth = 1usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut angle_depth = 1usize;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
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
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
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
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            b'(' => paren_depth += 1,
            b')' => {
                if paren_depth == 1 && brace_depth == 0 && bracket_depth == 0 {
                    // The call argument list ended before we saw a closing `>`.
                    return false;
                }
                paren_depth = paren_depth.saturating_sub(1);
            }
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'<' => angle_depth += 1,
            b'>' => {
                angle_depth = angle_depth.saturating_sub(1);
                if angle_depth == 0 {
                    return true;
                }
            }
            b',' if paren_depth == 1 && brace_depth == 0 && bracket_depth == 0 => {
                // We hit a top-level argument separator before seeing a closing `>`.
                return false;
            }
            _ => {}
        }
        i += 1;
    }

    false
}

fn collect_overload_candidates_by_arity(
    index: &Index,
    receiver_class: &str,
    method_name: &str,
    arity: usize,
) -> Vec<IndexSymbolId> {
    let mut receiver_class = receiver_class.to_string();
    let mut out = Vec::new();
    loop {
        for id in index.method_overloads_by_arity(&receiver_class, method_name, arity) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        receiver_class = match index.class_extends(&receiver_class) {
            Some(base) => base.to_string(),
            None => break,
        };
    }
    out
}

fn collect_overload_candidates_by_name(
    index: &Index,
    receiver_class: &str,
    method_name: &str,
) -> Vec<IndexSymbolId> {
    let mut receiver_class = receiver_class.to_string();
    let mut out = Vec::new();
    loop {
        for id in index.method_overloads(&receiver_class, method_name) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        receiver_class = match index.class_extends(&receiver_class) {
            Some(base) => base.to_string(),
            None => break,
        };
    }
    out
}

fn match_overload_candidate_set(
    candidates: &[IndexSymbolId],
    target_id: IndexSymbolId,
) -> Option<bool> {
    match candidates.len() {
        0 => None,
        1 => Some(candidates[0] == target_id),
        _ => Some(candidates.contains(&target_id)),
    }
}

fn resolve_overridden_method_candidates(
    index: &Index,
    base_class: &str,
    method_name: &str,
    param_types: Option<&[String]>,
    arity: Option<usize>,
) -> Option<Vec<IndexSymbolId>> {
    let mut base_class = base_class.to_string();
    loop {
        if let Some(param_types) = param_types {
            if let Some(id) =
                index.method_overload_by_param_types(&base_class, method_name, param_types)
            {
                return Some(vec![id]);
            }
        } else if let Some(arity) = arity {
            let ids = index.method_overloads_by_arity(&base_class, method_name, arity);
            if !ids.is_empty() {
                return Some(ids);
            }
        } else {
            let ids = index.method_overloads(&base_class, method_name);
            if !ids.is_empty() {
                return Some(ids);
            }
        }

        base_class = index.class_extends(&base_class)?.to_string();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Receiver {
    ImplicitThis,
    This,
    New(String),
    TypeName(String),
    Var(String),
    Unknown,
}

fn parse_receiver_expression(text: &str, ident_start: usize) -> Receiver {
    let bytes = text.as_bytes();
    // Look left for `.` ignoring whitespace.
    let mut i = ident_start;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'.' {
        return Receiver::ImplicitThis;
    }

    // We have `something . ident(`. Extract `something` token.
    let mut end = i - 1;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return Receiver::Unknown;
    }

    // If the receiver ends with a call expression (e.g. `new Foo()` or `factory()`),
    // walk back to the identifier preceding the `(` so `new Foo().bar()` resolves to `Foo`.
    let (start, token_end) = if bytes.get(end - 1) == Some(&b')') {
        let mut depth: i32 = 0;
        let mut k = end - 1;
        loop {
            match bytes[k] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            if k == 0 {
                return Receiver::Unknown;
            }
            k -= 1;
        }

        let mut token_end = k;
        while token_end > 0 && bytes[token_end - 1].is_ascii_whitespace() {
            token_end -= 1;
        }
        if token_end == 0 {
            return Receiver::Unknown;
        }

        let mut start = token_end;
        while start > 0 && is_ident_continue(bytes[start - 1]) {
            start -= 1;
        }
        (start, token_end)
    } else {
        let mut start = end;
        while start > 0 && is_ident_continue(bytes[start - 1]) {
            start -= 1;
        }
        (start, end)
    };

    let token = &text[start..token_end];
    if token.is_empty() {
        return Receiver::Unknown;
    }
    if token == "this" {
        return Receiver::This;
    }

    let prefix = text[..start].trim_end();
    let prefix_bytes = prefix.as_bytes();
    if prefix_bytes.ends_with(b"new") {
        if prefix_bytes.len() == 3 || !is_ident_continue(prefix_bytes[prefix_bytes.len() - 4]) {
            return Receiver::New(token.to_string());
        }
    }

    // Heuristic: capitalized identifier => type name, otherwise variable.
    if token
        .chars()
        .next()
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
    {
        Receiver::TypeName(token.to_string())
    } else {
        Receiver::Var(token.to_string())
    }
}
fn is_ident_continue(b: u8) -> bool {
    (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn enclosing_class_at_offset(index: &Index, file: &str, offset: usize) -> Option<String> {
    index
        .symbols()
        .iter()
        .filter(|sym| sym.kind == SymbolKind::Class && sym.file == file)
        .filter(|sym| offset >= sym.decl_range.start && offset < sym.decl_range.end)
        // Choose the most nested class (smallest range).
        .min_by_key(|sym| sym.decl_range.len())
        .map(|sym| sym.name.clone())
}

fn infer_var_type_in_scope(text: &str, offset: usize, var_name: &str) -> Option<String> {
    // Very small heuristic: search backwards in the same file for `<Type> <var_name>`
    // before the usage site.
    let before = &text[..offset.min(text.len())];
    let needle = format!(" {}", var_name);
    let mut search_pos = before.len();
    while let Some(pos) = before[..search_pos].rfind(&needle) {
        // Grab token before ` var_name`.
        let prefix = &before[..pos];
        let prefix = prefix.trim_end();
        let type_start = prefix
            .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
            .map(|p| p + 1)
            .unwrap_or(0);
        let ty = &prefix[type_start..];
        if !ty.is_empty()
            && ty
                .chars()
                .next()
                .map(|c| c.is_ascii_uppercase())
                .unwrap_or(false)
        {
            return Some(ty.to_string());
        }
        search_pos = pos;
    }
    None
}

fn best_effort_delete_usage(text: &str, range: IndexTextRange) -> Option<IndexTextRange> {
    // Try to delete the entire statement `...;` containing the usage.
    let bytes = text.as_bytes();
    let mut start = range.start;
    while start > 0 {
        let b = bytes[start - 1];
        if b == b'\n' || b == b';' || b == b'{' || b == b'}' {
            break;
        }
        start -= 1;
    }

    let mut end = range.end;
    while end < bytes.len() && bytes[end] != b';' {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b';' {
        end += 1;
        // Also remove trailing whitespace/newline.
        while end < bytes.len() && (bytes[end] == b' ' || bytes[end] == b'\t') {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'\n' {
            end += 1;
        }
        return Some(IndexTextRange::new(start, end));
    }

    None
}

/// Convert a [`SafeDeleteReport`] into a preview by calculating the "delete anyway"
/// edit set and then running it through the diff preview generator.
pub fn safe_delete_preview(
    index: &Index,
    report: &SafeDeleteReport,
) -> Result<crate::RefactoringPreview, SafeDeleteError> {
    let edit = safe_delete_delete_anyway_edit(index, report)?;
    Ok(crate::generate_preview(index, &edit)?)
}

/// Calculate the "delete anyway" edit set corresponding to a [`SafeDeleteReport`].
///
/// This is useful for clients that show a preview/report first but still want to
/// render a diff for the destructive follow-up action.
pub fn safe_delete_delete_anyway_edit(
    index: &Index,
    report: &SafeDeleteReport,
) -> Result<WorkspaceEdit, SafeDeleteError> {
    let target = &report.target;

    let mut edits: Vec<WorkspaceTextEdit> = Vec::new();
    for usage in &report.usages {
        if usage.file == target.file && ranges_overlap(usage.range, target.decl_range) {
            continue;
        }
        if usage.kind == UsageKind::Override {
            if let Some(sym) = index.symbols().iter().find(|sym| {
                sym.kind == SymbolKind::Method
                    && sym.file == usage.file
                    && sym.name_range == usage.range
            }) {
                edits.push(delete_range_workspace_edit(&usage.file, sym.decl_range));
                continue;
            }
        }
        if let Some(text) = index.file_text(&usage.file) {
            let delete_range = best_effort_delete_usage(text, usage.range).unwrap_or(usage.range);
            edits.push(delete_range_workspace_edit(&usage.file, delete_range));
        }
    }
    edits.push(delete_range_workspace_edit(&target.file, target.decl_range));

    merge_overlapping_deletes(&mut edits);
    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

/// Apply a set of edits to a workspace. This helper is used by tests.
pub fn apply_edits(
    files: &BTreeMap<String, String>,
    edits: &[TextEdit],
) -> BTreeMap<String, String> {
    let mut out = files.clone();

    // Group by file and apply from end to start for stable offsets.
    let mut grouped: BTreeMap<&str, Vec<&TextEdit>> = BTreeMap::new();
    for edit in edits {
        grouped.entry(&edit.file).or_default().push(edit);
    }

    for (file, mut file_edits) in grouped {
        if let Some(text) = out.get_mut(file) {
            file_edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
            for edit in file_edits {
                text.replace_range(edit.range.start..edit.range.end, &edit.replacement);
            }
        }
    }

    out
}
