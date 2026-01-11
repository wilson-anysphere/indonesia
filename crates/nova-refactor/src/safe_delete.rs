use std::collections::BTreeMap;

use nova_index::{Index, ReferenceCandidate, ReferenceKind, SymbolId, SymbolKind, TextRange};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeDeleteMode {
    Safe,
    DeleteAnyway,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeDeleteTarget {
    Symbol(SymbolId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub file: String,
    pub range: TextRange,
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
    pub range: TextRange,
    pub kind: UsageKind,
}

/// Serializable snapshot of a symbol targeted by Safe Delete.
///
/// We intentionally avoid re-exporting `nova-index`'s internal symbol types here
/// because `nova-index` contains multiple symbol representations (search index vs
/// sketch parser) and not all of them are `serde`-friendly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeDeleteSymbol {
    pub id: SymbolId,
    pub kind: SymbolKind,
    pub name: String,
    pub container: Option<String>,
    pub file: String,
    pub name_range: TextRange,
    pub decl_range: TextRange,
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
    Applied { edits: Vec<TextEdit> },
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
        let edit = delete_range_edit(&target.file, target.decl_range);
        return Ok(SafeDeleteOutcome::Applied { edits: vec![edit] });
    }

    match mode {
        SafeDeleteMode::Safe => Ok(SafeDeleteOutcome::Preview {
            report: SafeDeleteReport {
                target: target.clone(),
                usages,
            },
        }),
        SafeDeleteMode::DeleteAnyway => {
            let mut edits = Vec::new();
            // Best-effort: delete each usage statement (call) and then delete the declaration.
            for usage in &usages {
                if usage.file == target.file && ranges_overlap(usage.range, target.decl_range) {
                    continue;
                }
                if let Some(text) = index.file_text(&usage.file) {
                    if let Some(range) = best_effort_delete_usage(text, usage.range) {
                        edits.push(delete_range_edit(&usage.file, range));
                    } else {
                        edits.push(delete_range_edit(&usage.file, usage.range));
                    }
                }
            }
            edits.push(delete_range_edit(&target.file, target.decl_range));
            edits.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then_with(|| a.range.start.cmp(&b.range.start))
            });
            Ok(SafeDeleteOutcome::Applied { edits })
        }
    }
}

fn delete_range_edit(file: &str, range: TextRange) -> TextEdit {
    TextEdit {
        file: file.to_string(),
        range,
        replacement: String::new(),
    }
}

fn ranges_overlap(a: TextRange, b: TextRange) -> bool {
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

    if !is_followed_by_paren(text, candidate.range.end) {
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

    let resolved_method = resolve_method_call(index, &receiver_class, &target.name)?;
    if resolved_method != target.id {
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
    for sym in index.symbols() {
        if sym.kind != SymbolKind::Method || sym.name != target.name || !sym.is_override {
            continue;
        }
        let Some(class_name) = sym.container.as_deref() else {
            continue;
        };
        let Some(base_class) = index.class_extends(class_name) else {
            continue;
        };
        let Some(overridden) = resolve_method_call(index, base_class, &sym.name) else {
            continue;
        };
        if overridden != target.id {
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

fn is_followed_by_paren(text: &str, mut offset: usize) -> bool {
    let bytes = text.as_bytes();
    while offset < bytes.len() && bytes[offset].is_ascii_whitespace() {
        offset += 1;
    }
    bytes.get(offset) == Some(&b'(')
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

    // Handle `new Foo()`
    // Find start of the receiver expression by scanning back through identifier chars.
    let mut start = end;
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }
    let token = &text[start..end];
    if token == "this" {
        return Receiver::This;
    }

    // Check if receiver is `new <Type>()` by looking further left for `new`.
    // This is simplistic but good enough for tests.
    let prefix = &text[..start];
    let trimmed = prefix.trim_end();
    if trimmed.ends_with("new") {
        return Receiver::New(token.to_string());
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

fn resolve_method_call<'a>(
    index: &'a Index,
    mut receiver_class: &'a str,
    method_name: &str,
) -> Option<SymbolId> {
    loop {
        if let Some(id) = index.method_symbol_id(receiver_class, method_name) {
            return Some(id);
        }
        receiver_class = index.class_extends(receiver_class)?;
    }
}

fn best_effort_delete_usage(text: &str, range: TextRange) -> Option<TextRange> {
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
        return Some(TextRange::new(start, end));
    }

    None
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
