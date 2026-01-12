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
                    if let Some(sym) = index
                        .symbols_in_file(&usage.file)
                        .find(|sym| sym.kind == SymbolKind::Method && sym.name_range == usage.range)
                    {
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
    let mut kind = match candidate.kind {
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
    if index
        .symbols_in_file(&candidate.file)
        .any(|sym| sym.kind == SymbolKind::Method && sym.name_range == candidate.range)
    {
        return None;
    }

    let open_paren = call_open_paren_offset(text, candidate.range.end)?;
    let args = parse_call_args(text, open_paren)?;
    let arg_count = args.len();

    // Arity-aware verification: a mismatch means this is definitely a different overload.
    if let Some(target_arity) = index.method_param_types(target.id).map(|tys| tys.len()) {
        if arg_count != target_arity {
            return None;
        }
    }

    // Type-aware short-circuit: if we can prove this call cannot match the target overload,
    // drop it. This prevents calls to other same-arity overloads (e.g. `foo(1)` vs `foo(String)`)
    // from being treated as usages.
    if call_is_definitely_not_target_overload(index, target, text, candidate.range) {
        return None;
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

    let arg_types =
        infer_call_argument_types(index, &candidate.file, candidate.range.start, text, &args);
    let overloads = collect_effective_overload_candidates_by_arity(
        index,
        &receiver_class,
        &target.name,
        arg_count,
    );
    // Type filtering is best-effort and intentionally conservative. If we inferred argument types
    // but they don't match any overload's (lexical) signature, fall back to arity-only matching so
    // we don't miss valid calls due to subtyping (e.g. `foo(Map)` called with `new HashMap()`).
    let mut matching = filter_overloads_by_argument_types(index, &overloads, &arg_types);
    if matching.is_empty() {
        matching = overloads;
    }

    match matching.as_slice() {
        [] => return None,
        [only] => {
            if *only != target.id {
                return None;
            }
        }
        many => {
            if !many.contains(&target.id) {
                return None;
            }
            // Ambiguous overload match: keep as a usage, but mark it as `Unknown` so callers can
            // surface that Safe Delete couldn't disambiguate the call site.
            kind = UsageKind::Unknown;
        }
    }

    Some(Usage {
        file: candidate.file.clone(),
        range: candidate.range,
        kind,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgCategory {
    PrimitiveLiteral,
    StringLiteral,
    NullLiteral,
    Other,
}

fn call_is_definitely_not_target_overload(
    index: &Index,
    target: &SafeDeleteSymbol,
    candidate_text: &str,
    candidate_range: IndexTextRange,
) -> bool {
    let Some(target_param_types) = index.method_param_types(target.id) else {
        return false;
    };

    let Some(call_args) = parse_call_arg_categories(candidate_text, candidate_range.end) else {
        return false;
    };

    if !arity_compatible(target_param_types, &call_args) {
        return true;
    }

    for (param_ty, arg) in target_param_types.iter().zip(call_args.iter()) {
        if arg_definitely_incompatible_with_param(arg, param_ty) {
            return true;
        }
    }

    false
}

fn arity_compatible(param_types: &[String], args: &[ArgCategory]) -> bool {
    if param_types.len() == args.len() {
        return true;
    }

    // Very small varargs support: `T... args` can accept >= (n-1) arguments.
    if let Some(last) = param_types.last() {
        if last.trim_end().ends_with("...") {
            return args.len() + 1 >= param_types.len();
        }
    }

    false
}

fn arg_definitely_incompatible_with_param(arg: &ArgCategory, param_type: &str) -> bool {
    let base_param = normalize_type_token(param_type);

    if base_param == "String" {
        return matches!(arg, ArgCategory::PrimitiveLiteral);
    }

    if is_java_primitive_type(base_param) {
        return matches!(arg, ArgCategory::StringLiteral | ArgCategory::NullLiteral);
    }

    false
}

fn normalize_type_token(token: &str) -> &str {
    let token = token.trim();
    let token = token.strip_suffix("...").unwrap_or(token);
    let token = token.trim_end_matches("[]");
    token
}

fn is_java_primitive_type(ty: &str) -> bool {
    matches!(
        ty,
        "boolean" | "byte" | "short" | "int" | "long" | "char" | "float" | "double"
    )
}

fn parse_call_arg_categories(text: &str, name_end: usize) -> Option<Vec<ArgCategory>> {
    let open_paren = find_open_paren(text, name_end)?;
    let close_paren = find_matching_paren(text, open_paren)?;
    let inside = &text[open_paren + 1..close_paren - 1];

    let mut out = Vec::new();
    for arg in split_top_level_commas(inside) {
        out.push(classify_arg(arg));
    }
    Some(out)
}

fn find_open_paren(text: &str, mut offset: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    while offset < bytes.len() && bytes[offset].is_ascii_whitespace() {
        offset += 1;
    }
    if bytes.get(offset) == Some(&b'(') {
        Some(offset)
    } else {
        None
    }
}

fn find_matching_paren(text: &str, open_paren: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open_paren;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    // return exclusive end
                    return Some(i + 1);
                }
            }
            b'"' => {
                // Skip strings
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            b'\'' => {
                // Skip char literals
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'\'' {
                        break;
                    }
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Skip line comment
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Skip block comment
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
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    if s.trim().is_empty() {
        return Vec::new();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut i = 0usize;

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
                        break;
                    }
                    i += 1;
                }
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'\'' {
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'<' => angle_depth += 1,
            b'>' => angle_depth = angle_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b',' if paren_depth == 0
                && angle_depth == 0
                && brace_depth == 0
                && bracket_depth == 0 =>
            {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }

    out.push(s[start..].trim());
    out
}

fn classify_arg(arg: &str) -> ArgCategory {
    let s = arg.trim();
    if s.is_empty() {
        return ArgCategory::Other;
    }

    if s == "null" {
        return ArgCategory::NullLiteral;
    }

    if s.starts_with('"') {
        return ArgCategory::StringLiteral;
    }

    if s.starts_with('\'') {
        return ArgCategory::PrimitiveLiteral;
    }

    if s == "true" || s == "false" {
        return ArgCategory::PrimitiveLiteral;
    }

    if s.as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_digit() || *b == b'-')
    {
        return ArgCategory::PrimitiveLiteral;
    }

    ArgCategory::Other
}

fn find_override_usages(index: &Index, target: &SafeDeleteSymbol) -> Vec<Usage> {
    index
        .find_overrides(target.id)
        .into_iter()
        .filter_map(|id| index.find_symbol(id))
        .map(|sym| Usage {
            file: sym.file.clone(),
            range: sym.name_range,
            kind: UsageKind::Override,
        })
        .collect()
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

fn parse_call_args(text: &str, open_paren: usize) -> Option<Vec<String>> {
    let bytes = text.as_bytes();
    if bytes.get(open_paren) != Some(&b'(') {
        return None;
    }

    let mut i = open_paren + 1;
    let mut paren_depth = 1usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;

    let mut arg_start = i;
    let mut args: Vec<String> = Vec::new();

    while i < bytes.len() {
        if in_string {
            if escaped {
                escaped = false;
            } else if bytes[i] == b'\\' {
                escaped = true;
            } else if bytes[i] == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if escaped {
                escaped = false;
            } else if bytes[i] == b'\\' {
                escaped = true;
            } else if bytes[i] == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        match bytes[i] {
            b'"' => {
                in_string = true;
                i += 1;
                continue;
            }
            b'\'' => {
                in_char = true;
                i += 1;
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
                i += 1;
                continue;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                if paren_depth == 0 {
                    let last = text[arg_start..i].trim();
                    if !last.is_empty() {
                        args.push(last.to_string());
                    }
                    return Some(args);
                }
                i += 1;
                continue;
            }
            b'{' => {
                brace_depth += 1;
                i += 1;
                continue;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                i += 1;
                continue;
            }
            b'[' => {
                bracket_depth += 1;
                i += 1;
                continue;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                i += 1;
                continue;
            }
            b'<' => {
                if angle_depth > 0 {
                    angle_depth += 1;
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
                    i += 1;
                    continue;
                }

                i += 1;
                continue;
            }
            b'>' => {
                if angle_depth > 0 {
                    angle_depth = angle_depth.saturating_sub(1);
                }
                i += 1;
                continue;
            }
            b',' if paren_depth == 1
                && brace_depth == 0
                && bracket_depth == 0
                && angle_depth == 0 =>
            {
                let arg = text[arg_start..i].trim();
                if !arg.is_empty() {
                    args.push(arg.to_string());
                }
                arg_start = i + 1;
                i += 1;
                continue;
            }
            _ => {
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
    // - hit the end of the current call argument list (=> likely a comparison operator).
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
            _ => {}
        }
        i += 1;
    }

    false
}

fn infer_call_argument_types(
    index: &Index,
    file: &str,
    call_offset: usize,
    text: &str,
    args: &[String],
) -> Vec<Option<String>> {
    args.iter()
        .map(|arg| infer_expression_type(index, file, call_offset, text, arg))
        .collect()
}

fn infer_expression_type(
    index: &Index,
    file: &str,
    call_offset: usize,
    text: &str,
    expr: &str,
) -> Option<String> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }

    if expr == "null" {
        return None;
    }
    if expr == "true" || expr == "false" {
        return Some("boolean".to_string());
    }
    if expr == "this" {
        return enclosing_class_at_offset(index, file, call_offset);
    }
    if expr.as_bytes().first() == Some(&b'"') {
        return Some("String".to_string());
    }
    if expr.as_bytes().first() == Some(&b'\'') {
        return Some("char".to_string());
    }

    if let Some(num_ty) = infer_numeric_literal_type(expr) {
        return Some(num_ty);
    }

    if let Some(rest) = expr.strip_prefix("new") {
        let rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let mut end = 0usize;
        for (idx, ch) in rest.char_indices() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
                end = idx + ch.len_utf8();
                continue;
            }
            break;
        }
        let mut ty = rest[..end].trim();
        if ty.is_empty() {
            return None;
        }
        // Drop qualification (`com.foo.Bar` -> `Bar`).
        if let Some((_, last)) = ty.rsplit_once('.') {
            ty = last;
        }
        return Some(ty.to_string());
    }

    // Simple identifier: attempt to infer type from a nearby lexical declaration.
    if expr
        .bytes()
        .all(|b| (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$')
    {
        return infer_var_type_in_scope(text, call_offset, expr);
    }

    None
}

fn infer_numeric_literal_type(expr: &str) -> Option<String> {
    let expr = expr.trim();
    let expr = expr
        .strip_prefix("+")
        .or_else(|| expr.strip_prefix("-"))
        .unwrap_or(expr);
    if expr.is_empty() {
        return None;
    }

    let mut s = expr;
    let mut ty: Option<&str> = None;

    // Suffixes.
    if let Some(stripped) = s.strip_suffix("f").or_else(|| s.strip_suffix("F")) {
        s = stripped;
        ty = Some("float");
    } else if let Some(stripped) = s.strip_suffix("d").or_else(|| s.strip_suffix("D")) {
        s = stripped;
        ty = Some("double");
    } else if let Some(stripped) = s.strip_suffix("L").or_else(|| s.strip_suffix("l")) {
        s = stripped;
        ty = Some("long");
    }

    if s.is_empty() {
        return None;
    }

    if s.contains('.') || s.contains('e') || s.contains('E') {
        return Some(ty.unwrap_or("double").to_string());
    }

    if matches!(ty, Some("float") | Some("double")) {
        return Some(ty.unwrap().to_string());
    }

    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .or_else(|| s.strip_prefix("0b"))
        .or_else(|| s.strip_prefix("0B"))
        .unwrap_or(s);

    if s.bytes()
        .all(|b| (b as char).is_ascii_hexdigit() || b == b'_')
    {
        return Some(ty.unwrap_or("int").to_string());
    }

    None
}

fn collect_effective_overload_candidates_by_arity(
    index: &Index,
    receiver_class: &str,
    method_name: &str,
    arity: usize,
) -> Vec<IndexSymbolId> {
    let mut receiver_class = receiver_class.to_string();
    let mut seen_param_types: std::collections::HashSet<Vec<String>> =
        std::collections::HashSet::new();
    let mut out = Vec::new();
    loop {
        for id in index.method_overloads_by_arity(&receiver_class, method_name, arity) {
            if let Some(param_types) = index.method_param_types(id) {
                let key = param_types.to_vec();
                if !seen_param_types.insert(key) {
                    continue;
                }
            }
            out.push(id);
        }
        receiver_class = match index.class_extends(&receiver_class) {
            Some(base) => base.to_string(),
            None => break,
        };
    }
    out
}

fn filter_overloads_by_argument_types(
    index: &Index,
    overloads: &[IndexSymbolId],
    arg_types: &[Option<String>],
) -> Vec<IndexSymbolId> {
    overloads
        .iter()
        .copied()
        .filter(|id| overload_matches_arguments(index, *id, arg_types))
        .collect()
}

fn overload_matches_arguments(
    index: &Index,
    method_id: IndexSymbolId,
    arg_types: &[Option<String>],
) -> bool {
    let Some(param_types) = index.method_param_types(method_id) else {
        // No signature data available; can't filter.
        return true;
    };
    if param_types.len() != arg_types.len() {
        return false;
    }

    for (param, arg) in param_types.iter().zip(arg_types.iter()) {
        let Some(arg) = arg.as_deref() else {
            continue;
        };
        if !type_matches(param, arg) {
            return false;
        }
    }

    true
}

fn type_matches(param_type: &str, arg_type: &str) -> bool {
    let param = normalize_type_for_match(param_type);
    let arg = normalize_type_for_match(arg_type);
    if param == arg {
        return true;
    }

    // Best-effort boxing/unboxing equivalence for common primitives.
    matches!(
        (param.as_str(), arg.as_str()),
        ("boolean", "Boolean")
            | ("Boolean", "boolean")
            | ("byte", "Byte")
            | ("Byte", "byte")
            | ("short", "Short")
            | ("Short", "short")
            | ("int", "Integer")
            | ("Integer", "int")
            | ("long", "Long")
            | ("Long", "long")
            | ("float", "Float")
            | ("Float", "float")
            | ("double", "Double")
            | ("Double", "double")
            | ("char", "Character")
            | ("Character", "char")
    )
}

fn normalize_type_for_match(ty: &str) -> String {
    let ty = ty.trim();
    // Drop generic arguments (`List<String>` -> `List`).
    let ty = ty.split('<').next().unwrap_or(ty).trim();
    // Drop varargs marker (`String...` -> `String`).
    let ty = ty.strip_suffix("...").unwrap_or(ty).trim();

    // Drop qualification (`java.lang.String` -> `String`).
    let ty = ty
        .rsplit(|c: char| c == '.' || c == '$')
        .next()
        .unwrap_or(ty)
        .trim();
    ty.to_string()
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
        .symbol_at_offset(file, offset, Some(&[SymbolKind::Class]))
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
            if let Some(sym) = index
                .symbols_in_file(&usage.file)
                .find(|sym| sym.kind == SymbolKind::Method && sym.name_range == usage.range)
            {
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
