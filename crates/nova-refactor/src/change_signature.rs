use std::collections::{BTreeMap, HashMap, HashSet};

use crate::edit::{
    EditError, FileId, TextEdit as WorkspaceTextEdit, TextRange as WorkspaceTextRange,
    WorkspaceEdit,
};
use nova_index::{Index, ReferenceKind, SymbolId, SymbolKind, TextRange};
use nova_types::MethodId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum HierarchyPropagation {
    None,
    Overrides,
    Overridden,
    Both,
}

impl HierarchyPropagation {
    fn include_overrides(self) -> bool {
        matches!(
            self,
            HierarchyPropagation::Overrides | HierarchyPropagation::Both
        )
    }

    fn include_overridden(self) -> bool {
        matches!(
            self,
            HierarchyPropagation::Overridden | HierarchyPropagation::Both
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ParameterOperation {
    /// Use an existing parameter from the old signature, optionally changing its name/type.
    Existing {
        old_index: usize,
        new_name: Option<String>,
        new_type: Option<String>,
    },
    /// Add a new parameter.
    Add {
        name: String,
        ty: String,
        /// Expression inserted into updated call sites.
        ///
        /// Java doesn't have default parameters; we treat this as the *call-site default*.
        default_value: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangeSignature {
    pub target: MethodId,

    pub new_name: Option<String>,
    pub parameters: Vec<ParameterOperation>,
    pub new_return_type: Option<String>,
    pub new_throws: Option<Vec<String>>,

    #[serde(default = "HierarchyPropagation::default_for_serde")]
    pub propagate_hierarchy: HierarchyPropagation,
}

impl HierarchyPropagation {
    fn default_for_serde() -> HierarchyPropagation {
        HierarchyPropagation::Both
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeSignatureConflict {
    MissingTarget(MethodId),
    TargetNotAMethod(MethodId),
    InvalidParameterIndex {
        index: usize,
        param_len: usize,
    },
    AddedParameterMissingDefault {
        name: String,
    },
    RemovedParameterStillUsed {
        method: MethodId,
        param_name: String,
    },
    OverloadCollision {
        method: MethodId,
        collides_with: MethodId,
    },
    AmbiguousCallSite {
        file: String,
        range: TextRange,
        candidates: Vec<MethodId>,
    },
    ReturnTypeIncompatible {
        file: String,
        range: TextRange,
        expected: String,
        actual: String,
    },
    InvalidDocumentUri {
        file: String,
    },
    OverlappingEdits {
        file: String,
        first: TextRange,
        second: TextRange,
    },
    ParseError {
        file: String,
        context: &'static str,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("change signature conflicts")]
pub struct ChangeSignatureError {
    pub conflicts: Vec<ChangeSignatureConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParamDecl {
    ty: String,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedMethod {
    file: String,
    method_id: MethodId,
    class: String,
    name: String,
    prefix: String,
    return_type: String,
    params: Vec<ParamDecl>,
    throws: Vec<String>,
    header_range: TextRange,
    body_range: Option<TextRange>,
    brace: char,
}

pub fn change_signature(
    index: &Index,
    change: &ChangeSignature,
) -> Result<WorkspaceEdit, ChangeSignatureError> {
    let mut conflicts = Vec::new();

    let target_symbol_id = SymbolId(change.target.0);
    let target_sym = match index.find_symbol(target_symbol_id) {
        Some(sym) => sym,
        None => {
            return Err(ChangeSignatureError {
                conflicts: vec![ChangeSignatureConflict::MissingTarget(change.target)],
            })
        }
    };
    if target_sym.kind != SymbolKind::Method {
        return Err(ChangeSignatureError {
            conflicts: vec![ChangeSignatureConflict::TargetNotAMethod(change.target)],
        });
    }
    let Some(target_class) = target_sym.container.as_deref() else {
        return Err(ChangeSignatureError {
            conflicts: vec![ChangeSignatureConflict::ParseError {
                file: target_sym.file.clone(),
                context: "missing method container",
            }],
        });
    };

    let target_parsed = match parse_method(index, target_sym, change.target) {
        Ok(m) => m,
        Err(c) => {
            return Err(ChangeSignatureError { conflicts: vec![c] });
        }
    };

    // Validate the plan against the target parameters.
    let _ = compute_new_params(&target_parsed.params, &change.parameters, &mut conflicts);
    if !conflicts.is_empty() {
        return Err(ChangeSignatureError { conflicts });
    }

    let affected = collect_affected_methods(
        index,
        target_class,
        &target_parsed,
        change.propagate_hierarchy,
    );
    let affected_ids: HashSet<MethodId> = affected.iter().map(|m| m.method_id).collect();

    // Conflicts: removed parameter still referenced in each affected body.
    for m in &affected {
        detect_removed_parameter_usage(index, m, &change.parameters, &mut conflicts);
    }

    // Conflicts: overload collisions in each affected class.
    for m in &affected {
        detect_overload_collisions(index, m, &affected_ids, change, &mut conflicts);
    }

    // Call site updates (semantic verification).
    let call_updates =
        collect_call_site_updates(index, &target_parsed, &affected_ids, change, &mut conflicts);

    if !conflicts.is_empty() {
        return Err(ChangeSignatureError { conflicts });
    }

    // Materialize edits.
    let mut edits: Vec<(String, TextRange, String)> = Vec::new();

    for m in &affected {
        let new_sig = compute_new_signature_for_method(m, change, &mut Vec::new());
        edits.push((
            m.file.clone(),
            m.header_range,
            format_method_header(&m.prefix, &new_sig, m.brace),
        ));
        edits.extend(parameter_rename_edits(
            index,
            m,
            &new_sig,
            &change.parameters,
        ));
    }

    edits.extend(call_updates);

    build_workspace_edit(edits).map_err(|c| ChangeSignatureError { conflicts: vec![c] })
}

fn collect_affected_methods(
    index: &Index,
    target_class: &str,
    target: &ParsedMethod,
    propagation: HierarchyPropagation,
) -> Vec<ParsedMethod> {
    let mut out = Vec::new();
    out.push(target.clone());

    let target_param_types: Vec<String> = target.params.iter().map(|p| p.ty.clone()).collect();

    if propagation.include_overridden() {
        let mut cur = index.class_extends(target_class);
        while let Some(super_name) = cur {
            out.extend(find_methods_by_signature(
                index,
                super_name,
                &target.name,
                &target_param_types,
            ));
            cur = index.class_extends(super_name);
        }
    }

    if propagation.include_overrides() {
        for sym in index.symbols() {
            if sym.kind != SymbolKind::Method || sym.name != target.name {
                continue;
            }
            let Some(class_name) = sym.container.as_deref() else {
                continue;
            };
            if class_name == target_class {
                continue;
            }
            if !is_subclass_of(index, class_name, target_class) {
                continue;
            }

            let parsed = match parse_method(index, sym, MethodId(sym.id.0)) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let parsed_types: Vec<String> = parsed.params.iter().map(|p| p.ty.clone()).collect();
            if parsed_types == target_param_types {
                out.push(parsed);
            }
        }
    }

    out.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.header_range.start.cmp(&b.header_range.start))
    });
    out.dedup_by(|a, b| a.method_id == b.method_id);
    out
}

fn is_subclass_of<'a>(index: &'a Index, mut sub: &'a str, sup: &'a str) -> bool {
    if sub == sup {
        return true;
    }
    while let Some(next) = index.class_extends(sub) {
        if next == sup {
            return true;
        }
        sub = next;
    }
    false
}

fn find_methods_by_signature(
    index: &Index,
    class: &str,
    name: &str,
    param_types: &[String],
) -> Vec<ParsedMethod> {
    let mut out = Vec::new();
    for sym in index.symbols() {
        if sym.kind != SymbolKind::Method {
            continue;
        }
        if sym.name != name {
            continue;
        }
        if sym.container.as_deref() != Some(class) {
            continue;
        }
        let parsed = match parse_method(index, sym, MethodId(sym.id.0)) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let parsed_types: Vec<String> = parsed.params.iter().map(|p| p.ty.clone()).collect();
        if parsed_types == param_types {
            out.push(parsed);
        }
    }
    out
}

fn compute_new_params(
    old: &[ParamDecl],
    ops: &[ParameterOperation],
    conflicts: &mut Vec<ChangeSignatureConflict>,
) -> Vec<ParamDecl> {
    let mut params = Vec::new();
    for op in ops {
        match op {
            ParameterOperation::Existing {
                old_index,
                new_name,
                new_type,
            } => {
                if *old_index >= old.len() {
                    conflicts.push(ChangeSignatureConflict::InvalidParameterIndex {
                        index: *old_index,
                        param_len: old.len(),
                    });
                    continue;
                }
                let old_p = &old[*old_index];
                params.push(ParamDecl {
                    name: new_name.clone().unwrap_or_else(|| old_p.name.clone()),
                    ty: new_type.clone().unwrap_or_else(|| old_p.ty.clone()),
                });
            }
            ParameterOperation::Add {
                name,
                ty,
                default_value,
            } => {
                if default_value.is_none() {
                    conflicts.push(ChangeSignatureConflict::AddedParameterMissingDefault {
                        name: name.clone(),
                    });
                }
                params.push(ParamDecl {
                    name: name.clone(),
                    ty: ty.clone(),
                });
            }
        }
    }
    params
}

fn compute_new_signature_for_method(
    old: &ParsedMethod,
    change: &ChangeSignature,
    conflicts: &mut Vec<ChangeSignatureConflict>,
) -> ParsedMethodSig {
    let name = change.new_name.clone().unwrap_or_else(|| old.name.clone());
    let return_type = change
        .new_return_type
        .clone()
        .unwrap_or_else(|| old.return_type.clone());
    let throws = change
        .new_throws
        .clone()
        .unwrap_or_else(|| old.throws.clone());
    let params = compute_new_params(&old.params, &change.parameters, conflicts);
    ParsedMethodSig {
        name,
        return_type,
        params,
        throws,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedMethodSig {
    name: String,
    return_type: String,
    params: Vec<ParamDecl>,
    throws: Vec<String>,
}

fn detect_removed_parameter_usage(
    index: &Index,
    method: &ParsedMethod,
    ops: &[ParameterOperation],
    conflicts: &mut Vec<ChangeSignatureConflict>,
) {
    let Some(body_range) = method.body_range else {
        return;
    };
    let Some(text) = index.file_text(&method.file) else {
        return;
    };
    let body = &text[body_range.start..body_range.end];

    let mut retained = vec![false; method.params.len()];
    for op in ops {
        if let ParameterOperation::Existing { old_index, .. } = op {
            if *old_index < retained.len() {
                retained[*old_index] = true;
            }
        }
    }
    for (idx, keep) in retained.into_iter().enumerate() {
        if keep {
            continue;
        }
        let name = &method.params[idx].name;
        if find_identifier(body, name).is_some() {
            conflicts.push(ChangeSignatureConflict::RemovedParameterStillUsed {
                method: method.method_id,
                param_name: name.clone(),
            });
        }
    }
}

fn detect_overload_collisions(
    index: &Index,
    method: &ParsedMethod,
    affected: &HashSet<MethodId>,
    change: &ChangeSignature,
    conflicts: &mut Vec<ChangeSignatureConflict>,
) {
    let new_sig = compute_new_signature_for_method(method, change, &mut Vec::new());
    let new_param_types: Vec<String> = new_sig.params.iter().map(|p| p.ty.clone()).collect();

    for sym in index.symbols() {
        if sym.kind != SymbolKind::Method {
            continue;
        }
        if sym.container.as_deref() != Some(method.class.as_str()) {
            continue;
        }
        let other_id = MethodId(sym.id.0);
        if affected.contains(&other_id) {
            continue;
        }
        let Ok(other) = parse_method(index, sym, other_id) else {
            continue;
        };
        let other_types: Vec<String> = other.params.iter().map(|p| p.ty.clone()).collect();
        if other.name == new_sig.name && other_types == new_param_types {
            conflicts.push(ChangeSignatureConflict::OverloadCollision {
                method: method.method_id,
                collides_with: other_id,
            });
        }
    }
}

fn collect_call_site_updates(
    index: &Index,
    target: &ParsedMethod,
    affected_ids: &HashSet<MethodId>,
    change: &ChangeSignature,
    conflicts: &mut Vec<ChangeSignatureConflict>,
) -> Vec<(String, TextRange, String)> {
    let old_name = &target.name;
    let old_param_types: Vec<String> = target.params.iter().map(|p| p.ty.clone()).collect();
    let old_arity = target.params.len();

    let new_name = change.new_name.clone().unwrap_or_else(|| old_name.clone());
    let new_param_count = change.parameters.len();
    let new_param_types = compute_new_params(&target.params, &change.parameters, &mut Vec::new())
        .into_iter()
        .map(|p| p.ty)
        .collect::<Vec<_>>();

    // Exclude occurrences that live inside any affected declaration header. The index's
    // candidate collection is intentionally lexical and will report method declarations
    // as call candidates (identifier followed by `(`).
    let mut header_spans_by_file: HashMap<String, Vec<TextRange>> = HashMap::new();
    for sym in index.symbols() {
        if sym.kind != SymbolKind::Method {
            continue;
        }
        let id = MethodId(sym.id.0);
        if !affected_ids.contains(&id) {
            continue;
        }
        if let Ok(parsed) = parse_method(index, sym, id) {
            header_spans_by_file
                .entry(parsed.file.clone())
                .or_default()
                .push(parsed.header_range);
        }
    }

    let mut updates = Vec::new();
    for candidate in index.find_name_candidates(old_name) {
        if let Some(spans) = header_spans_by_file.get(&candidate.file) {
            if spans
                .iter()
                .any(|span| ranges_overlap(candidate.range, *span))
            {
                continue;
            }
        }
        if candidate.kind != ReferenceKind::Call {
            continue;
        }
        let Some(text) = index.file_text(&candidate.file) else {
            continue;
        };
        if !is_followed_by_paren(text, candidate.range.end) {
            continue;
        }

        let (call_range, args) = match parse_call_args(text, candidate.range) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if args.len() != old_arity {
            continue;
        }

        let Some(receiver_class) =
            infer_receiver_class(index, &candidate.file, candidate.range.start, text)
        else {
            continue;
        };
        let Some(resolved) =
            resolve_method_in_hierarchy(index, &receiver_class, old_name, &old_param_types)
        else {
            continue;
        };
        if !affected_ids.contains(&resolved) {
            continue;
        }

        // Ambiguity (best-effort): multiple overload candidates after the change.
        let overloads = overload_candidates_after_change(
            index,
            &receiver_class,
            affected_ids,
            &new_name,
            new_param_count,
            &new_param_types,
        );
        if overloads.len() > 1 {
            conflicts.push(ChangeSignatureConflict::AmbiguousCallSite {
                file: candidate.file.clone(),
                range: call_range,
                candidates: overloads,
            });
            continue;
        }

        // Return type compatibility (best-effort): `Type x = call(...)`
        if let Some(new_return) = change.new_return_type.as_deref() {
            if new_return != target.return_type {
                if let Some((expected, actual)) =
                    check_return_type_compatibility(text, call_range.start, new_return)
                {
                    conflicts.push(ChangeSignatureConflict::ReturnTypeIncompatible {
                        file: candidate.file.clone(),
                        range: call_range,
                        expected,
                        actual,
                    });
                    continue;
                }
            }
        }

        let new_args = rewrite_args(&args, &change.parameters);
        updates.push((
            candidate.file.clone(),
            call_range,
            format!("{new_name}({})", new_args.join(", ")),
        ));
    }

    updates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start.cmp(&b.1.start)));
    updates
}

fn overload_candidates_after_change(
    index: &Index,
    receiver_class: &str,
    affected: &HashSet<MethodId>,
    new_name: &str,
    new_param_count: usize,
    new_param_types: &[String],
) -> Vec<MethodId> {
    let mut by_sig: HashMap<Vec<String>, MethodId> = HashMap::new();
    let mut cur = Some(receiver_class);
    while let Some(class) = cur {
        for sym in index.symbols() {
            if sym.kind != SymbolKind::Method {
                continue;
            }
            if sym.container.as_deref() != Some(class) {
                continue;
            }
            let id = MethodId(sym.id.0);
            let parsed = match parse_method(index, sym, id) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let (name, param_types) = if affected.contains(&id) {
                (new_name, new_param_types.to_vec())
            } else {
                (
                    parsed.name.as_str(),
                    parsed.params.iter().map(|p| p.ty.clone()).collect(),
                )
            };

            if name == new_name && param_types.len() == new_param_count {
                by_sig.entry(param_types).or_insert(id);
            }
        }
        cur = index.class_extends(class);
    }

    let mut out: Vec<_> = by_sig.into_values().collect();
    out.sort_by_key(|id| id.0);
    out
}

fn ranges_overlap(a: TextRange, b: TextRange) -> bool {
    a.start < b.end && b.start < a.end
}

fn parse_call_args(text: &str, name_range: TextRange) -> Result<(TextRange, Vec<String>), ()> {
    let bytes = text.as_bytes();
    let mut open = name_range.end;
    while open < bytes.len() && bytes[open].is_ascii_whitespace() {
        open += 1;
    }
    if bytes.get(open) != Some(&b'(') {
        return Err(());
    }
    let close = find_matching_paren(text, open).ok_or(())?;
    let args_src = &text[open + 1..close - 1];
    let args = split_top_level(args_src, ',')
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    Ok((TextRange::new(name_range.start, close), args))
}

fn rewrite_args(old_args: &[String], ops: &[ParameterOperation]) -> Vec<String> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            ParameterOperation::Existing { old_index, .. } => {
                if *old_index < old_args.len() {
                    out.push(old_args[*old_index].trim().to_string());
                }
            }
            ParameterOperation::Add { default_value, .. } => {
                if let Some(v) = default_value {
                    out.push(v.trim().to_string());
                }
            }
        }
    }
    out
}

fn infer_receiver_class(
    index: &Index,
    file: &str,
    ident_start: usize,
    text: &str,
) -> Option<String> {
    let receiver = parse_receiver_expression(text, ident_start);
    match receiver {
        Receiver::ImplicitThis | Receiver::This => {
            enclosing_class_at_offset(index, file, ident_start)
        }
        Receiver::New(name) | Receiver::TypeName(name) => Some(name),
        Receiver::Var(name) => infer_var_type_in_scope(text, ident_start, &name),
        Receiver::Unknown => None,
    }
}

fn resolve_method_in_hierarchy(
    index: &Index,
    receiver_class: &str,
    name: &str,
    param_types: &[String],
) -> Option<MethodId> {
    // We intentionally use an owned string here because the starting class name
    // may come from a call-site receiver expression rather than the index's own
    // class table.
    let mut class = receiver_class.to_string();
    loop {
        for sym in index.symbols() {
            if sym.kind != SymbolKind::Method {
                continue;
            }
            if sym.container.as_deref() != Some(class.as_str()) {
                continue;
            }
            if sym.name != name {
                continue;
            }
            let id = MethodId(sym.id.0);
            let parsed = parse_method(index, sym, id).ok()?;
            let parsed_types: Vec<String> = parsed.params.iter().map(|p| p.ty.clone()).collect();
            if parsed_types == param_types {
                return Some(id);
            }
        }
        let next = index.class_extends(&class)?;
        class = next.to_string();
    }
}

fn enclosing_class_at_offset(index: &Index, file: &str, offset: usize) -> Option<String> {
    index
        .symbols()
        .iter()
        .filter(|sym| sym.kind == SymbolKind::Class && sym.file == file)
        .filter(|sym| offset >= sym.decl_range.start && offset < sym.decl_range.end)
        .min_by_key(|sym| sym.decl_range.len())
        .map(|sym| sym.name.clone())
}

fn infer_var_type_in_scope(text: &str, offset: usize, var_name: &str) -> Option<String> {
    let before = &text[..offset.min(text.len())];
    let needle = format!(" {}", var_name);
    let mut search_pos = before.len();
    while let Some(pos) = before[..search_pos].rfind(&needle) {
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

    let mut end = i - 1;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return Receiver::Unknown;
    }

    // Handle receivers like `new Foo().bar()` where the token before `.` is `)`.
    if bytes.get(end - 1) == Some(&b')') {
        let close_paren = end - 1;
        let mut depth: i32 = 0;
        let mut j = close_paren;
        while j > 0 {
            match bytes[j] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j -= 1;
        }
        if depth == 0 && bytes.get(j) == Some(&b'(') {
            let mut token_end = j;
            while token_end > 0 && bytes[token_end - 1].is_ascii_whitespace() {
                token_end -= 1;
            }
            let mut token_start = token_end;
            while token_start > 0 && is_ident_continue(bytes[token_start - 1]) {
                token_start -= 1;
            }
            let token = &text[token_start..token_end];
            if token == "this" {
                return Receiver::This;
            }
            if !token.is_empty() {
                let prefix = &text[..token_start];
                let trimmed = prefix.trim_end();
                if trimmed.ends_with("new") {
                    return Receiver::New(token.to_string());
                }
            }
        }
    }

    let mut start = end;
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }
    let token = &text[start..end];
    if token == "this" {
        return Receiver::This;
    }

    let prefix = &text[..start];
    let trimmed = prefix.trim_end();
    if trimmed.ends_with("new") {
        return Receiver::New(token.to_string());
    }

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

fn check_return_type_compatibility(
    text: &str,
    call_start: usize,
    new_return: &str,
) -> Option<(String, String)> {
    let line_start = text[..call_start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[call_start..]
        .find('\n')
        .map(|i| call_start + i)
        .unwrap_or(text.len());
    let line = &text[line_start..line_end];
    let rel_call_start = call_start - line_start;
    let before = &line[..rel_call_start];

    let assign_re = regex::Regex::new(
        r"(?x)^\s*(?P<ty>[A-Za-z_][A-Za-z0-9_<>,\[\]]*)\s+[A-Za-z_][A-Za-z0-9_]*\s*=\s*$",
    )
    .ok()?;
    let expected = assign_re
        .captures(before)
        .and_then(|c| c.name("ty"))
        .map(|m| m.as_str().to_string())?;

    if expected == new_return || expected == "Object" {
        return None;
    }

    Some((expected, new_return.to_string()))
}

fn parameter_rename_edits(
    index: &Index,
    method: &ParsedMethod,
    new_sig: &ParsedMethodSig,
    ops: &[ParameterOperation],
) -> Vec<(String, TextRange, String)> {
    let Some(body_range) = method.body_range else {
        return Vec::new();
    };
    let Some(text) = index.file_text(&method.file) else {
        return Vec::new();
    };
    let body = &text[body_range.start..body_range.end];

    let mut edits = Vec::new();
    for (new_pos, op) in ops.iter().enumerate() {
        let ParameterOperation::Existing {
            old_index,
            new_name,
            ..
        } = op
        else {
            continue;
        };
        if *old_index >= method.params.len() {
            continue;
        }
        let old_name = &method.params[*old_index].name;
        let new_name = new_name.as_deref().unwrap_or(&new_sig.params[new_pos].name);
        if old_name == new_name {
            continue;
        }

        let mut cursor = 0usize;
        while let Some(pos) = find_identifier(&body[cursor..], old_name) {
            let start = cursor + pos;
            edits.push((
                method.file.clone(),
                TextRange::new(
                    body_range.start + start,
                    body_range.start + start + old_name.len(),
                ),
                new_name.to_string(),
            ));
            cursor = start + old_name.len();
        }
    }

    edits
}

fn find_identifier(text: &str, ident: &str) -> Option<usize> {
    if ident.is_empty() {
        return None;
    }
    let bytes = text.as_bytes();
    let needle = ident.as_bytes();
    let mut i = 0usize;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !is_ident_continue(bytes[i - 1]);
            let after_ok =
                i + needle.len() == bytes.len() || !is_ident_continue(bytes[i + needle.len()]);
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn parse_method(
    index: &Index,
    sym: &nova_index::Symbol,
    method_id: MethodId,
) -> Result<ParsedMethod, ChangeSignatureConflict> {
    let text = index
        .file_text(&sym.file)
        .ok_or_else(|| ChangeSignatureConflict::ParseError {
            file: sym.file.clone(),
            context: "missing file text",
        })?;

    let Some(class) = sym.container.clone() else {
        return Err(ChangeSignatureConflict::ParseError {
            file: sym.file.clone(),
            context: "missing container",
        });
    };

    let name = sym.name.clone();
    let line_start = text[..sym.name_range.start]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);

    // Parse return type token as the final non-whitespace token before the name.
    let before_name = &text[line_start..sym.name_range.start];
    let trimmed = before_name.trim_end();
    let ret_start_rel = trimmed
        .rfind(|c: char| c.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(0);
    let prefix = before_name[..ret_start_rel].to_string();
    let return_type = trimmed[ret_start_rel..].to_string();
    if return_type.is_empty() {
        return Err(ChangeSignatureConflict::ParseError {
            file: sym.file.clone(),
            context: "missing return type",
        });
    }

    // Parse parameter list and detect `{`/`;`.
    let bytes = text.as_bytes();
    let mut open = sym.name_range.end;
    while open < bytes.len() && bytes[open].is_ascii_whitespace() {
        open += 1;
    }
    if bytes.get(open) != Some(&b'(') {
        return Err(ChangeSignatureConflict::ParseError {
            file: sym.file.clone(),
            context: "missing parameter list",
        });
    }
    let close =
        find_matching_paren(text, open).ok_or_else(|| ChangeSignatureConflict::ParseError {
            file: sym.file.clone(),
            context: "unmatched paren in signature",
        })?;

    let params_src = &text[open + 1..close - 1];
    let params = parse_params(params_src);

    let mut i = close;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut throws = Vec::new();
    if text[i..].starts_with("throws") {
        i += "throws".len();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let throws_start = i;
        while i < bytes.len() && bytes[i] != b'{' && bytes[i] != b';' {
            i += 1;
        }
        throws = text[throws_start..i]
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }

    let brace = match bytes.get(i) {
        Some(b'{') => '{',
        Some(b';') => ';',
        _ => {
            return Err(ChangeSignatureConflict::ParseError {
                file: sym.file.clone(),
                context: "missing `{` or `;` after signature",
            })
        }
    };
    let header_range = TextRange::new(line_start, i + 1);
    let body_range = if brace == '{' {
        Some(TextRange::new(i + 1, sym.decl_range.end.saturating_sub(1)))
    } else {
        None
    };

    Ok(ParsedMethod {
        file: sym.file.clone(),
        method_id,
        class,
        name,
        prefix,
        return_type,
        params,
        throws,
        header_range,
        body_range,
        brace,
    })
}

fn parse_params(params: &str) -> Vec<ParamDecl> {
    let mut out = Vec::new();
    let params = params.trim();
    if params.is_empty() {
        return out;
    }
    for part in split_top_level(params, ',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = p.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let name = tokens[tokens.len() - 1].to_string();
        let ty = tokens[..tokens.len() - 1].join(" ");
        out.push(ParamDecl { ty, name });
    }
    out
}

fn format_method_header(prefix: &str, sig: &ParsedMethodSig, brace: char) -> String {
    let params = sig
        .params
        .iter()
        .map(|p| format!("{} {}", p.ty, p.name))
        .collect::<Vec<_>>()
        .join(", ");
    let throws = if sig.throws.is_empty() {
        String::new()
    } else {
        format!(" throws {}", sig.throws.join(", "))
    };
    format!(
        "{prefix}{} {}({}){throws} {brace}",
        sig.return_type, sig.name, params
    )
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
                    return Some(i + 1);
                }
            }
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
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level(text: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match ch {
            '"' => in_string = true,
            '(' => depth_paren += 1,
            ')' => depth_paren -= 1,
            '[' => depth_brack += 1,
            ']' => depth_brack -= 1,
            '{' => depth_brace += 1,
            '}' => depth_brace -= 1,
            _ => {}
        }

        if ch == sep && depth_paren == 0 && depth_brack == 0 && depth_brace == 0 {
            out.push(text[start..i].to_string());
            start = i + 1;
        }
        i += 1;
    }
    out.push(text[start..].to_string());
    out
}

fn build_workspace_edit(
    edits: Vec<(String, TextRange, String)>,
) -> Result<WorkspaceEdit, ChangeSignatureConflict> {
    let mut by_file: BTreeMap<String, Vec<(TextRange, String)>> = BTreeMap::new();
    for (file, range, text) in edits {
        by_file.entry(file).or_default().push((range, text));
    }

    let mut out = Vec::new();
    for (file, mut file_edits) in by_file {
        file_edits.sort_by_key(|(r, _)| r.start);
        for w in file_edits.windows(2) {
            let a = &w[0].0;
            let b = &w[1].0;
            if a.end > b.start {
                return Err(ChangeSignatureConflict::OverlappingEdits {
                    file,
                    first: *a,
                    second: *b,
                });
            }
        }

        let file_id = FileId::new(file.clone());
        out.extend(
            file_edits
                .into_iter()
                .map(|(range, new_text)| WorkspaceTextEdit {
                    file: file_id.clone(),
                    range: WorkspaceTextRange::new(range.start, range.end),
                    replacement: new_text,
                }),
        );
    }

    let mut edit = WorkspaceEdit::new(out);
    edit.normalize().map_err(|err| match err {
        EditError::InvalidRange {
            file: FileId(file), ..
        } => ChangeSignatureConflict::ParseError {
            file,
            context: "invalid edit range",
        },
        EditError::OverlappingEdits {
            file: FileId(file),
            first,
            second,
        } => ChangeSignatureConflict::OverlappingEdits {
            file,
            first: TextRange::new(first.start, first.end),
            second: TextRange::new(second.start, second.end),
        },
        EditError::OutOfBounds {
            file: FileId(file), ..
        } => ChangeSignatureConflict::ParseError {
            file,
            context: "edit out of bounds",
        },
        EditError::UnknownFile(FileId(file)) => ChangeSignatureConflict::ParseError {
            file,
            context: "unknown file referenced by edit",
        },
        EditError::FileAlreadyExists(FileId(file)) => ChangeSignatureConflict::ParseError {
            file,
            context: "file already exists",
        },
        EditError::InvalidRename {
            from: FileId(file), ..
        } => ChangeSignatureConflict::ParseError {
            file,
            context: "invalid rename operation",
        },
        EditError::DuplicateCreate { file: FileId(file) } => ChangeSignatureConflict::ParseError {
            file,
            context: "duplicate create operation",
        },
        EditError::DuplicateRenameSource {
            from: FileId(file), ..
        } => ChangeSignatureConflict::ParseError {
            file,
            context: "duplicate rename source",
        },
        EditError::DuplicateRenameDestination {
            to: FileId(file), ..
        } => ChangeSignatureConflict::ParseError {
            file,
            context: "duplicate rename destination",
        },
        EditError::RenameCycle { file: FileId(file) } => ChangeSignatureConflict::ParseError {
            file,
            context: "rename cycle detected",
        },
        EditError::CreateDeleteConflict { file: FileId(file) } => {
            ChangeSignatureConflict::ParseError {
                file,
                context: "file is both created and deleted",
            }
        }
        EditError::FileOpCollision {
            file: FileId(file), ..
        } => ChangeSignatureConflict::ParseError {
            file,
            context: "conflicting file operation",
        },
        EditError::TextEditTargetsRenamedFile {
            file: FileId(file), ..
        } => ChangeSignatureConflict::ParseError {
            file,
            context: "text edit targets renamed file",
        },
        EditError::TextEditTargetsDeletedFile { file: FileId(file) } => {
            ChangeSignatureConflict::ParseError {
                file,
                context: "text edit targets deleted file",
            }
        }
    })?;
    Ok(edit)
}
