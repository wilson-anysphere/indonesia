use std::collections::{BTreeMap, HashMap, HashSet};

use crate::edit::{
    EditError, FileId, TextEdit as WorkspaceTextEdit, TextRange as WorkspaceTextRange,
    WorkspaceEdit,
};
use nova_index::{normalize_type_signature, Index, ReferenceKind, SymbolId, SymbolKind, TextRange};
use nova_syntax::ast::{self, AstNode};
use nova_syntax::parse_java;
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

#[derive(Debug, Clone)]
struct AnnotationValueRename {
    annotation_name: String,
    annotation_qualified_name: String,
    new_element_name: String,
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
    let call_updates = collect_call_site_updates(
        index,
        &target_parsed,
        &affected,
        &affected_ids,
        change,
        &mut conflicts,
    );

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

    let target_param_types: Vec<String> = index
        .method_signature(SymbolId(target.method_id.0))
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| {
            target
                .params
                .iter()
                .map(|p| normalize_type_signature(&p.ty))
                .collect()
        });
    let target_is_interface = index.is_interface(target_class);
    let target_id = SymbolId(target.method_id.0);

    if propagation.include_overridden() {
        if target_is_interface {
            // Interface -> superinterfaces.
            for super_iface in transitive_interface_supertypes(index, target_class) {
                out.extend(find_methods_by_signature(
                    index,
                    &super_iface,
                    &target.name,
                    &target_param_types,
                ));
            }
        } else {
            // Class -> superclasses via override chain.
            let mut cur = target_id;
            while let Some(next) = index.find_overridden(cur) {
                let Some(sym) = index.find_symbol(next) else {
                    break;
                };
                if let Ok(parsed) = parse_method(index, sym, MethodId(next.0)) {
                    out.push(parsed);
                }
                cur = next;
            }

            // Also include interface methods that the class implements (directly or via its
            // superclasses) so API refactors stay consistent.
            for iface in transitive_implemented_interfaces(index, target_class) {
                out.extend(find_methods_by_signature(
                    index,
                    &iface,
                    &target.name,
                    &target_param_types,
                ));
            }
        }
    }

    if propagation.include_overrides() {
        if target_is_interface {
            // 1) Subinterfaces that redeclare the method.
            for ty in index.symbols().iter().filter(|sym| {
                sym.kind == SymbolKind::Class
                    && sym.container.is_none()
                    && index.is_interface(&sym.name)
            }) {
                if ty.name == target_class {
                    continue;
                }
                if !is_subinterface_of(index, &ty.name, target_class) {
                    continue;
                }
                out.extend(find_methods_by_signature(
                    index,
                    &ty.name,
                    &target.name,
                    &target_param_types,
                ));
            }

            // 2) Classes that implement the interface (directly or transitively).
            for ty in index.symbols().iter().filter(|sym| {
                sym.kind == SymbolKind::Class
                    && sym.container.is_none()
                    && !index.is_interface(&sym.name)
            }) {
                if !class_implements_interface(index, &ty.name, target_class) {
                    continue;
                }

                // Best-effort: find the concrete/abstract class method declaration that would
                // satisfy the interface method for this class and rename it. This ensures cases
                // like `class C extends Base implements I {}` still rename `Base.m()` if it is the
                // inherited implementation.
                let Some(resolved) =
                    resolve_method_in_hierarchy(index, &ty.name, &target.name, &target_param_types)
                else {
                    continue;
                };

                // Avoid pulling in the interface method itself for abstract/default-only cases.
                let Some(sym) = index.find_symbol(SymbolId(resolved.0)) else {
                    continue;
                };
                let Some(container) = sym.container.as_deref() else {
                    continue;
                };
                if index.is_interface(container) {
                    continue;
                }

                let parsed = match parse_method(index, sym, resolved) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let parsed_types: Vec<String> = index
                    .method_signature(SymbolId(parsed.method_id.0))
                    .map(<[String]>::to_vec)
                    .unwrap_or_else(|| {
                        parsed
                            .params
                            .iter()
                            .map(|p| normalize_type_signature(&p.ty))
                            .collect()
                    });
                if parsed_types == target_param_types {
                    out.push(parsed);
                }
            }
        } else {
            // Class -> subclasses overriding the method.
            for id in index.find_overrides(target_id) {
                let Some(sym) = index.find_symbol(id) else {
                    continue;
                };
                if let Ok(parsed) = parse_method(index, sym, MethodId(id.0)) {
                    out.push(parsed);
                }
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

fn is_subinterface_of(index: &Index, sub: &str, sup: &str) -> bool {
    if sub == sup {
        return true;
    }
    let mut stack = vec![sub.to_string()];
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        if let Some(parents) = index.interface_extends(&cur) {
            for parent in parents {
                if parent == sup {
                    return true;
                }
                stack.push(parent.clone());
            }
        }
    }
    false
}

fn transitive_interface_supertypes(index: &Index, iface: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = index
        .interface_extends(iface)
        .map(|v| v.to_vec())
        .unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        out.push(cur.clone());
        if let Some(parents) = index.interface_extends(&cur) {
            for parent in parents {
                stack.push(parent.clone());
            }
        }
    }
    out
}

fn transitive_implemented_interfaces(index: &Index, class_name: &str) -> Vec<String> {
    let mut stack: Vec<String> = Vec::new();
    let mut cur = Some(class_name);
    while let Some(class) = cur {
        stack.extend(index.class_implements(class).iter().cloned());
        cur = index.class_extends(class);
    }

    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(iface) = stack.pop() {
        if !seen.insert(iface.clone()) {
            continue;
        }
        out.push(iface.clone());
        if let Some(parents) = index.interface_extends(&iface) {
            stack.extend(parents.iter().cloned());
        }
    }
    out
}

fn class_implements_interface(index: &Index, class_name: &str, iface: &str) -> bool {
    let mut cur = Some(class_name);
    while let Some(class) = cur {
        for implemented in index.class_implements(class) {
            if is_subinterface_of(index, implemented, iface) {
                return true;
            }
        }
        cur = index.class_extends(class);
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

    if let Some(sym_id) = index.method_symbol_id_by_signature(class, name, param_types) {
        let Some(sym) = index.find_symbol(sym_id) else {
            return out;
        };
        let id = MethodId(sym_id.0);
        if let Ok(parsed) = parse_method(index, sym, id) {
            out.push(parsed);
        }
        return out;
    }

    // Best-effort fallback: scan overloads in the class and compare parsed types. This keeps
    // behavior similar to the previous symbol-scan implementation when the sketch signature
    // extraction is incomplete.
    let expected: Vec<String> = param_types
        .iter()
        .map(|t| normalize_type_signature(t))
        .collect();
    for sym_id in index.method_symbol_ids(class, name) {
        let Some(sym) = index.find_symbol(sym_id) else {
            continue;
        };
        let id = MethodId(sym_id.0);
        let Ok(parsed) = parse_method(index, sym, id) else {
            continue;
        };
        let parsed_types: Vec<String> = parsed
            .params
            .iter()
            .map(|p| normalize_type_signature(&p.ty))
            .collect();
        if parsed_types == expected {
            out.push(parsed);
            break;
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

fn method_param_types_for_signature(index: &Index, method: &ParsedMethod) -> Vec<String> {
    index
        .method_signature(SymbolId(method.method_id.0))
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| {
            method
                .params
                .iter()
                .map(|p| normalize_type_signature(&p.ty))
                .collect()
        })
}

fn compute_new_param_types_for_signature(
    old_param_types: &[String],
    ops: &[ParameterOperation],
) -> Vec<String> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            ParameterOperation::Existing {
                old_index,
                new_type,
                ..
            } => {
                let ty = match new_type {
                    Some(ty) => normalize_type_signature(ty),
                    None => old_param_types.get(*old_index).cloned().unwrap_or_default(),
                };
                out.push(ty);
            }
            ParameterOperation::Add { ty, .. } => out.push(normalize_type_signature(ty)),
        }
    }
    out
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
    let new_name = change
        .new_name
        .clone()
        .unwrap_or_else(|| method.name.clone());
    let old_param_types = method_param_types_for_signature(index, method);
    let new_param_types =
        compute_new_param_types_for_signature(&old_param_types, &change.parameters);
    let Some(collides_with) =
        index.method_symbol_id_by_signature(&method.class, &new_name, &new_param_types)
    else {
        return;
    };
    let collides_with = MethodId(collides_with.0);
    if collides_with == method.method_id || affected.contains(&collides_with) {
        return;
    }
    conflicts.push(ChangeSignatureConflict::OverloadCollision {
        method: method.method_id,
        collides_with,
    });
}

fn collect_call_site_updates(
    index: &Index,
    target: &ParsedMethod,
    affected: &[ParsedMethod],
    affected_ids: &HashSet<MethodId>,
    change: &ChangeSignature,
    conflicts: &mut Vec<ChangeSignatureConflict>,
) -> Vec<(String, TextRange, String)> {
    let old_name = &target.name;
    let old_param_types = method_param_types_for_signature(index, target);
    let old_arity = old_param_types.len();

    let new_name = change.new_name.clone().unwrap_or_else(|| old_name.clone());
    let new_param_types =
        compute_new_param_types_for_signature(&old_param_types, &change.parameters);

    // Exclude method declaration name tokens. `find_name_candidates` is intentionally lexical
    // and reports method declarations as call candidates (identifier followed by `(`).
    let mut method_decl_name_ranges: HashMap<String, HashSet<TextRange>> = HashMap::new();
    for sym in index.symbols() {
        if sym.kind != SymbolKind::Method {
            continue;
        }
        method_decl_name_ranges
            .entry(sym.file.clone())
            .or_default()
            .insert(sym.name_range);
    }

    // Exclude occurrences that live inside any affected declaration header. The index's
    // candidate collection is intentionally lexical and will report method declarations
    // as call candidates (identifier followed by `(`).
    let mut header_spans_by_file: HashMap<String, Vec<TextRange>> = HashMap::new();
    for m in affected {
        header_spans_by_file
            .entry(m.file.clone())
            .or_default()
            .push(m.header_range);
    }

    let mut updates = Vec::new();
    for candidate in index.find_name_candidates(old_name) {
        if method_decl_name_ranges
            .get(&candidate.file)
            .is_some_and(|ranges| ranges.contains(&candidate.range))
        {
            continue;
        }
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

        // Best-effort overload disambiguation: if we can infer an argument's type and it
        // doesn't match the target signature, treat it as a call to a different overload.
        let inferred_arg_types = infer_call_arg_types(text, call_range.start, &args)
            .into_iter()
            .map(|ty| ty.map(|t| normalize_type_signature(&t)))
            .collect::<Vec<_>>();
        if inferred_arg_types.len() == old_param_types.len()
            && inferred_arg_types
                .iter()
                .zip(old_param_types.iter())
                .any(|(arg_ty, param_ty)| {
                    matches!(
                        arg_ty,
                        Some(t) if !types_equivalent_ignoring_whitespace(t, param_ty)
                    )
                })
        {
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
        let new_expected_types = rewrite_types_for_call(&inferred_arg_types, &change.parameters);
        let overloads = overload_candidates_after_change(
            index,
            &receiver_class,
            affected_ids,
            old_name,
            &new_name,
            &new_expected_types,
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

    // Special-case: Renaming the `value()` element of an annotation type breaks shorthand usages
    // like `@Anno(expr)`. Java desugars those to `@Anno(value = expr)`, so after a rename the
    // shorthand must be rewritten to an explicit element-value pair.
    if let Some(rename) = annotation_value_rename_context(index, target, change) {
        updates.extend(collect_annotation_value_rename_updates(
            index,
            &rename.annotation_name,
            &rename.annotation_qualified_name,
            &rename.new_element_name,
        ));
    }

    updates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start.cmp(&b.1.start)));
    updates
}

fn annotation_value_rename_context(
    index: &Index,
    target: &ParsedMethod,
    change: &ChangeSignature,
) -> Option<AnnotationValueRename> {
    let new_name = change.new_name.as_deref()?;
    if new_name == "value" {
        return None;
    }
    if target.name != "value" {
        return None;
    }
    if !target.params.is_empty() {
        return None;
    }

    let sym = index.find_symbol(SymbolId(target.method_id.0))?;
    let text = index.file_text(&sym.file)?;
    let parsed = parse_java(text);
    let root = parsed.syntax();

    // Locate the method declaration node corresponding to the target symbol (range match) and
    // check whether it is declared inside an `@interface` (annotation type).
    for method in root.descendants().filter_map(ast::MethodDeclaration::cast) {
        let Some(name_tok) = method.name_token() else {
            continue;
        };
        if syntax_token_range(&name_tok) != sym.name_range {
            continue;
        }

        let param_count = method
            .parameter_list()
            .map(|list| list.parameters().count())
            .unwrap_or(0);
        if param_count != 0 {
            return None;
        }

        let Some(annotation_ty) = method
            .syntax()
            .ancestors()
            .find_map(ast::AnnotationTypeDeclaration::cast)
        else {
            return None;
        };
        let annotation_name = annotation_ty.name_token()?.text().to_string();

        // Best-effort disambiguation: track the fully qualified annotation type name so we can
        // avoid rewriting unrelated qualified usages like `@other.A(...)`.
        //
        // This is still heuristic (the `Index` is not a full resolver), but it prevents common
        // false-positives when multiple packages contain the same simple annotation name.
        let mut type_parts: Vec<String> = Vec::new();
        for ancestor in annotation_ty.syntax().ancestors() {
            if let Some(ty) = ast::AnnotationTypeDeclaration::cast(ancestor.clone()) {
                if let Some(tok) = ty.name_token() {
                    type_parts.push(tok.text().to_string());
                }
                continue;
            }
            if let Some(ty) = ast::ClassDeclaration::cast(ancestor.clone()) {
                if let Some(tok) = ty.name_token() {
                    type_parts.push(tok.text().to_string());
                }
                continue;
            }
            if let Some(ty) = ast::InterfaceDeclaration::cast(ancestor.clone()) {
                if let Some(tok) = ty.name_token() {
                    type_parts.push(tok.text().to_string());
                }
                continue;
            }
            if let Some(ty) = ast::EnumDeclaration::cast(ancestor.clone()) {
                if let Some(tok) = ty.name_token() {
                    type_parts.push(tok.text().to_string());
                }
                continue;
            }
            if let Some(ty) = ast::RecordDeclaration::cast(ancestor) {
                if let Some(tok) = ty.name_token() {
                    type_parts.push(tok.text().to_string());
                }
            }
        }
        type_parts.reverse();
        let type_name = type_parts.join(".");

        let pkg = ast::CompilationUnit::cast(root.clone())
            .and_then(|unit| unit.package())
            .and_then(|pkg| pkg.name())
            .map(|name| name.text().trim().to_string())
            .filter(|pkg| !pkg.is_empty());
        let annotation_qualified_name = if let Some(pkg) = pkg {
            format!("{pkg}.{type_name}")
        } else {
            type_name
        };

        return Some(AnnotationValueRename {
            annotation_name,
            annotation_qualified_name,
            new_element_name: new_name.to_string(),
        });
    }

    None
}

fn collect_annotation_value_rename_updates(
    index: &Index,
    annotation_name: &str,
    annotation_qualified_name: &str,
    new_element_name: &str,
) -> Vec<(String, TextRange, String)> {
    let mut updates = Vec::new();

    for (file, text) in index.files() {
        let parsed = parse_java(text);
        let root = parsed.syntax();

        for ann in root.descendants().filter_map(ast::Annotation::cast) {
            let Some(name) = ann.name() else {
                continue;
            };
            let name_text = name.text();
            let matches_annotation = if name_text.contains('.') {
                name_text == annotation_qualified_name
                    || annotation_qualified_name.ends_with(&format!(".{name_text}"))
            } else {
                name_text == annotation_name
            };
            if !matches_annotation {
                continue;
            }

            let Some(args) = ann.arguments() else {
                continue;
            };

            let has_pairs = args.pairs().next().is_some();
            let value = args.value();

            // Conflicts: if the parse produced both a shorthand value and named pairs, skip.
            if value.is_some() && has_pairs {
                continue;
            }

            if let Some(value) = value {
                // Shorthand `@Anno(expr)` form.
                if has_pairs {
                    continue;
                }
                let Some(inner_range) = annotation_args_inner_range(text, &args) else {
                    continue;
                };
                let value_range = syntax_node_range(value.syntax());
                let value_text = text
                    .get(value_range.start..value_range.end)
                    .unwrap_or_default()
                    .trim();
                if value_text.is_empty() {
                    continue;
                }
                updates.push((
                    file.clone(),
                    inner_range,
                    format!("{new_element_name} = {value_text}"),
                ));
            } else if has_pairs {
                // Named pair `@Anno(value = expr)` form.
                for pair in args.pairs() {
                    let Some(name_tok) = pair.name_token() else {
                        continue;
                    };
                    if name_tok.text() != "value" {
                        continue;
                    }
                    updates.push((
                        file.clone(),
                        syntax_token_range(&name_tok),
                        new_element_name.to_string(),
                    ));
                }
            }
        }
    }

    updates
}

fn annotation_args_inner_range(
    source: &str,
    args: &ast::AnnotationElementValuePairList,
) -> Option<TextRange> {
    let range = syntax_node_range(args.syntax());
    if range.len() < 2 {
        return None;
    }

    let bytes = source.as_bytes();
    if bytes.get(range.start) != Some(&b'(') {
        return None;
    }
    if bytes.get(range.end.saturating_sub(1)) != Some(&b')') {
        return None;
    }

    Some(TextRange::new(range.start + 1, range.end - 1))
}

fn syntax_node_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn syntax_token_range(token: &nova_syntax::SyntaxToken) -> TextRange {
    let range = token.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn overload_candidates_after_change(
    index: &Index,
    receiver_class: &str,
    affected: &HashSet<MethodId>,
    old_name: &str,
    new_name: &str,
    expected_param_types: &[Option<String>],
    new_param_types: &[String],
) -> Vec<MethodId> {
    let mut by_sig: HashMap<Vec<String>, MethodId> = HashMap::new();
    let expected_param_count = expected_param_types.len();

    let mut cur = Some(receiver_class);
    while let Some(class) = cur {
        // Methods already named `new_name`.
        for sym_id in index.method_symbol_ids(class, new_name) {
            let id = MethodId(sym_id.0);
            let param_types = if affected.contains(&id) {
                new_param_types.to_vec()
            } else {
                index
                    .method_signature(sym_id)
                    .map(<[String]>::to_vec)
                    .unwrap_or_default()
            };
            if param_types.len() == expected_param_count
                && param_types_match_expected(&param_types, expected_param_types)
            {
                by_sig.entry(param_types).or_insert(id);
            }
        }

        // If we're renaming, include methods currently named `old_name` that will become
        // `new_name` (i.e. affected methods).
        if old_name != new_name {
            for sym_id in index.method_symbol_ids(class, old_name) {
                let id = MethodId(sym_id.0);
                if !affected.contains(&id) {
                    continue;
                }
                let param_types = new_param_types.to_vec();
                if param_types.len() == expected_param_count
                    && param_types_match_expected(&param_types, expected_param_types)
                {
                    by_sig.entry(param_types).or_insert(id);
                }
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

fn param_types_match_expected(param_types: &[String], expected: &[Option<String>]) -> bool {
    if param_types.len() != expected.len() {
        return false;
    }
    for (actual, exp) in param_types.iter().zip(expected.iter()) {
        if let Some(t) = exp {
            if !types_equivalent_ignoring_whitespace(actual, t) {
                return false;
            }
        }
    }
    true
}

fn types_equivalent_ignoring_whitespace(a: &str, b: &str) -> bool {
    fn erase_generic_args(ty: &str) -> String {
        let mut out = String::with_capacity(ty.len());
        let mut depth: i32 = 0;
        for ch in ty.chars() {
            match ch {
                '<' => {
                    depth += 1;
                    continue;
                }
                '>' => {
                    if depth > 0 {
                        depth -= 1;
                        continue;
                    }
                }
                _ => {}
            }
            if depth == 0 {
                out.push(ch);
            }
        }
        out
    }

    // For overload disambiguation we can ignore generic type arguments entirely: Java overloads
    // are resolved on the *erased* signature, so `<...>` never affects which overload is called.
    let a = erase_generic_args(a);
    let b = erase_generic_args(b);

    let mut ia = a.chars().filter(|c| !c.is_whitespace());
    let mut ib = b.chars().filter(|c| !c.is_whitespace());
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x == y => continue,
            _ => return false,
        }
    }
}

fn rewrite_types_for_call(
    old_types: &[Option<String>],
    ops: &[ParameterOperation],
) -> Vec<Option<String>> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            ParameterOperation::Existing { old_index, .. } => {
                out.push(old_types.get(*old_index).cloned().unwrap_or(None));
            }
            ParameterOperation::Add { ty, .. } => out.push(Some(normalize_type_signature(ty))),
        }
    }
    out
}

fn infer_call_arg_types(text: &str, call_start: usize, args: &[String]) -> Vec<Option<String>> {
    args.iter()
        .map(|arg| infer_expr_type(text, call_start, arg))
        .collect()
}

fn infer_expr_type(text: &str, offset: usize, expr: &str) -> Option<String> {
    let e = expr.trim();
    if e.is_empty() {
        return None;
    }
    if e.starts_with('"') {
        return Some("String".to_string());
    }
    if e.starts_with('\'') {
        return Some("char".to_string());
    }
    if e == "true" || e == "false" {
        return Some("boolean".to_string());
    }
    if looks_like_int_literal(e) {
        return Some("int".to_string());
    }
    if let Some(rest) = e.strip_prefix("new") {
        let rest = rest.trim_start();
        let bytes = rest.as_bytes();
        if bytes
            .first()
            .copied()
            .map(is_ident_continue)
            .unwrap_or(false)
        {
            let mut end = 0usize;
            while end < bytes.len()
                && (is_ident_continue(bytes[end]) || bytes[end] == b'.' || bytes[end] == b'$')
            {
                end += 1;
            }
            let ty = rest[..end].trim();
            if !ty.is_empty() {
                return Some(ty.to_string());
            }
        }
    }
    if is_simple_identifier(e) {
        return infer_var_type_in_scope_any(text, offset, e);
    }
    None
}

fn looks_like_int_literal(expr: &str) -> bool {
    let e = expr.trim();
    let e = e
        .strip_prefix('+')
        .or_else(|| e.strip_prefix('-'))
        .unwrap_or(e);
    if e.is_empty() {
        return false;
    }
    e.bytes().all(|b| b.is_ascii_digit() || b == b'_')
}

fn is_simple_identifier(expr: &str) -> bool {
    let bytes = expr.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    if !((first as char).is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return false;
    }
    bytes.iter().copied().all(is_ident_continue)
}

fn infer_var_type_in_scope_any(text: &str, offset: usize, var_name: &str) -> Option<String> {
    // Best-effort heuristic: search backwards in the same file for `<Type> <var_name>` before the
    // usage site. Unlike `infer_var_type_in_scope`, this also accepts primitive types.
    let before = &text[..offset.min(text.len())];
    let needle = format!(" {}", var_name);
    let mut search_pos = before.len();
    while let Some(pos) = before[..search_pos].rfind(&needle) {
        let prefix = before[..pos].trim_end();
        // Scan backwards for a plausible type token.
        //
        // This intentionally allows whitespace *inside* generic argument lists so we can recover
        // types like `Map<String, Integer>` without accidentally stopping at the space after the
        // comma.
        let bytes = prefix.as_bytes();
        let mut i = prefix.len();
        let mut depth_angle: i32 = 0;
        let mut depth_brack: i32 = 0;
        while i > 0 {
            let b = bytes[i - 1];
            match b {
                b'>' => {
                    depth_angle += 1;
                    i -= 1;
                    continue;
                }
                b'<' => {
                    if depth_angle > 0 {
                        depth_angle -= 1;
                        i -= 1;
                        continue;
                    }
                    break;
                }
                b']' => {
                    depth_brack += 1;
                    i -= 1;
                    continue;
                }
                b'[' => {
                    if depth_brack > 0 {
                        depth_brack -= 1;
                        i -= 1;
                        continue;
                    }
                    break;
                }
                _ => {}
            }

            if depth_angle > 0 {
                if is_type_token_char(b) || b.is_ascii_whitespace() {
                    i -= 1;
                    continue;
                }
                break;
            }

            if depth_brack > 0 {
                if is_type_token_char(b) || b.is_ascii_whitespace() {
                    i -= 1;
                    continue;
                }
                break;
            }
            if b.is_ascii_whitespace() {
                break;
            }
            if is_type_token_char(b) {
                i -= 1;
                continue;
            }
            break;
        }
        while i > 0 && !prefix.is_char_boundary(i) {
            i -= 1;
        }

        if depth_angle == 0 && depth_brack == 0 {
            let ty = prefix[i..].trim();
            let ty = ty.split_whitespace().collect::<Vec<_>>().join(" ");
            if is_plausible_type_token(&ty) {
                return Some(ty);
            }
        }
        search_pos = pos;
    }
    None
}

fn is_type_token_char(b: u8) -> bool {
    // Allow generic/array/package tokens in best-effort type extraction.
    (b as char).is_ascii_alphanumeric()
        || b.is_ascii_whitespace()
        || matches!(
            b,
            b'_' | b'$' | b'.' | b'<' | b'>' | b',' | b'[' | b']' | b'?' | b'&'
        )
}

fn extract_type_token_suffix(prefix: &str) -> Option<&str> {
    let bytes = prefix.as_bytes();
    let mut end = prefix.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }

    let mut start = end;
    let mut depth_angle: i32 = 0;
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_whitespace() {
            if depth_angle > 0 {
                start -= 1;
                continue;
            }
            break;
        }

        match b {
            b'>' => {
                depth_angle += 1;
                start -= 1;
                continue;
            }
            b'<' => {
                if depth_angle > 0 {
                    depth_angle -= 1;
                }
                start -= 1;
                continue;
            }
            _ => {}
        }

        if is_type_token_char(b) {
            start -= 1;
            continue;
        }
        break;
    }

    let ty = prefix[start..end].trim();
    (!ty.is_empty()).then_some(ty)
}

fn normalize_type_whitespace(ty: &str) -> String {
    let mut out = String::new();
    for part in ty.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(part);
    }
    out
}

fn is_reference_type_token(ty: &str) -> bool {
    if ty.is_empty() {
        return false;
    }
    let mut base = ty.trim();

    // Strip generic arguments.
    if let Some((head, _)) = base.split_once('<') {
        base = head.trim_end();
    }

    // Strip array suffixes.
    while let Some(stripped) = base.strip_suffix("[]") {
        base = stripped.trim_end();
    }

    base.rsplit('.')
        .next()
        .and_then(|seg| seg.chars().next())
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
}

fn is_plausible_type_token(ty: &str) -> bool {
    if ty.is_empty() {
        return false;
    }
    let mut base = ty.trim();

    // Strip generic arguments.
    if let Some((head, _)) = base.split_once('<') {
        base = head.trim_end();
    }

    // Strip array suffixes.
    while let Some(stripped) = base.strip_suffix("[]") {
        base = stripped.trim_end();
    }

    matches!(
        base,
        "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char"
    ) || base
        .rsplit('.')
        .next()
        .and_then(|seg| seg.chars().next())
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
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
    // Interface receiver: search the interface itself and its superinterfaces.
    if index.is_interface(receiver_class) {
        let mut ifaces = vec![receiver_class.to_string()];
        ifaces.extend(transitive_interface_supertypes(index, receiver_class));
        return resolve_method_in_interfaces(index, ifaces, name, param_types);
    }

    // We intentionally use an owned string here because the starting class name
    // may come from a call-site receiver expression rather than the index's own
    // class table.
    let mut class = receiver_class.to_string();
    loop {
        if let Some(id) = index.method_symbol_id_by_signature(&class, name, param_types) {
            return Some(MethodId(id.0));
        }

        // Best-effort fallback when signature lookup fails: parse overload declarations in this
        // class and compare their parameter types.
        let expected: Vec<String> = param_types
            .iter()
            .map(|t| normalize_type_signature(t))
            .collect();
        for sym_id in index.method_symbol_ids(&class, name) {
            let Some(sym) = index.find_symbol(sym_id) else {
                continue;
            };
            let id = MethodId(sym_id.0);
            let Ok(parsed) = parse_method(index, sym, id) else {
                continue;
            };
            let parsed_types: Vec<String> = parsed
                .params
                .iter()
                .map(|p| normalize_type_signature(&p.ty))
                .collect();
            if parsed_types == expected {
                return Some(id);
            }
        }

        let Some(next) = index.class_extends(&class) else {
            break;
        };
        class = next.to_string();
    }

    // Best-effort: if the method isn't declared anywhere in the class hierarchy,
    // fall back to the class's implemented interfaces. This matters for cases like:
    //
    // `interface I { void m(); } abstract class C implements I { void f(){ m(); } }`
    //
    // where `m()` is inherited from the interface contract.
    resolve_method_in_interfaces(
        index,
        transitive_implemented_interfaces(index, receiver_class).into_iter(),
        name,
        param_types,
    )
}

fn resolve_method_in_interfaces(
    index: &Index,
    interfaces: impl IntoIterator<Item = impl AsRef<str>>,
    name: &str,
    param_types: &[String],
) -> Option<MethodId> {
    let expected: Vec<String> = param_types
        .iter()
        .map(|t| normalize_type_signature(t))
        .collect();
    let mut matches: HashSet<MethodId> = HashSet::new();

    for iface in interfaces {
        let iface = iface.as_ref();
        if let Some(sym_id) = index.method_symbol_id_by_signature(iface, name, &expected) {
            matches.insert(MethodId(sym_id.0));
            continue;
        }

        // Best-effort fallback: parse overload declarations in this interface and compare types.
        for sym_id in index.method_symbol_ids(iface, name) {
            let Some(sym) = index.find_symbol(sym_id) else {
                continue;
            };
            let id = MethodId(sym_id.0);
            let Ok(parsed) = parse_method(index, sym, id) else {
                continue;
            };
            let parsed_types: Vec<String> = parsed
                .params
                .iter()
                .map(|p| normalize_type_signature(&p.ty))
                .collect();
            if parsed_types == expected {
                matches.insert(id);
            }
        }
    }

    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

fn enclosing_class_at_offset(index: &Index, file: &str, offset: usize) -> Option<String> {
    index
        .symbol_at_offset(file, offset, Some(&[SymbolKind::Class]))
        .map(|sym| sym.name.clone())
}

fn infer_var_type_in_scope(text: &str, offset: usize, var_name: &str) -> Option<String> {
    let before = &text[..offset.min(text.len())];
    let needle = format!(" {}", var_name);
    let mut search_pos = before.len();
    while let Some(pos) = before[..search_pos].rfind(&needle) {
        let prefix = before[..pos].trim_end();
        let Some(ty) = extract_type_token_suffix(prefix) else {
            search_pos = pos;
            continue;
        };
        let ty = normalize_type_whitespace(ty);
        if is_reference_type_token(&ty) {
            return Some(ty);
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
    for part in split_top_level_types(params, ',') {
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

fn split_top_level_types(text: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_angle = 0i32;
    let mut start = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if in_string || in_char {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if in_string && ch == '"' {
                in_string = false;
            } else if in_char && ch == '\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        match ch {
            '"' => in_string = true,
            '\'' => in_char = true,
            '(' => depth_paren += 1,
            ')' => depth_paren -= 1,
            '[' => depth_brack += 1,
            ']' => depth_brack -= 1,
            '{' => depth_brace += 1,
            '}' => depth_brace -= 1,
            '<' => depth_angle += 1,
            '>' => depth_angle -= 1,
            _ => {}
        }

        if ch == sep && depth_paren == 0 && depth_brack == 0 && depth_brace == 0 && depth_angle == 0
        {
            out.push(text[start..i].to_string());
            start = i + 1;
        }
        i += 1;
    }
    out.push(text[start..].to_string());
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
    let brace_sep = if brace == ';' { "" } else { " " };
    format!(
        "{prefix}{} {}({}){throws}{brace_sep}{brace}",
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
    let mut depth_angle = 0i32;
    let mut start = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if in_string || in_char {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if in_string && ch == '"' {
                in_string = false;
            } else if in_char && ch == '\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        match ch {
            '"' => in_string = true,
            '\'' => in_char = true,
            '(' => depth_paren += 1,
            ')' => depth_paren -= 1,
            '[' => depth_brack += 1,
            ']' => depth_brack -= 1,
            '{' => depth_brace += 1,
            '}' => depth_brace -= 1,
            '<' => {
                // We only want to treat `<...>` as nested structure when it's likely to be a
                // generic type argument list (e.g. `new Map<String, Integer>()`). Treating all
                // `<` as nesting would break splitting call args like `foo(a < b, c)`.
                //
                // Best-effort heuristic: count `<`/`>` only when the `<` is directly attached to
                // the preceding token (no whitespace) and that token looks like either a type
                // (UpperCamelCase) or an explicit type-arg prefix (`obj.<T>m()`).
                let immediately_preceded_by_ws = i > 0
                    && bytes
                        .get(i - 1)
                        .copied()
                        .is_some_and(|b| b.is_ascii_whitespace());
                if depth_angle > 0 {
                    depth_angle += 1;
                } else if !immediately_preceded_by_ws {
                    // Look left for previous non-whitespace byte.
                    let mut j = i;
                    while j > 0 && bytes[j - 1].is_ascii_whitespace() {
                        j -= 1;
                    }
                    let prev = j.checked_sub(1).and_then(|k| bytes.get(k)).copied();
                    let looks_like_explicit_type_args = prev == Some(b'.');
                    let looks_like_type = (|| {
                        let Some(prev) = prev else {
                            return false;
                        };
                        if !is_ident_continue(prev) && prev != b'.' && prev != b'$' {
                            return false;
                        }
                        // Find the start of the `Foo.Bar` token immediately before `<`.
                        let mut start = j.saturating_sub(1);
                        while start > 0 {
                            let b = bytes[start - 1];
                            if is_ident_continue(b) || b == b'.' || b == b'$' {
                                start -= 1;
                            } else {
                                break;
                            }
                        }
                        let Ok(token) = std::str::from_utf8(&bytes[start..j]) else {
                            return false;
                        };
                        let last = token.rsplit('.').next().unwrap_or(token);
                        last.chars()
                            .next()
                            .map(|c| c.is_ascii_uppercase())
                            .unwrap_or(false)
                    })();

                    if looks_like_explicit_type_args || looks_like_type {
                        depth_angle += 1;
                    }
                }
            }
            '>' => {
                if depth_angle > 0 {
                    depth_angle -= 1;
                }
            }
            _ => {}
        }

        if ch == sep && depth_paren == 0 && depth_brack == 0 && depth_brace == 0 && depth_angle == 0
        {
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
        EditError::InvalidUtf8Boundary {
            file: FileId(file), ..
        } => ChangeSignatureConflict::ParseError {
            file,
            context: "edit range is not on UTF-8 character boundaries",
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
