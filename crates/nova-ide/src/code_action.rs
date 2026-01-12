use lsp_types::{
    CodeAction, CodeActionKind, Command, Diagnostic, NumberOrString, Position, Range, TextEdit,
    Uri, WorkspaceEdit,
};
use nova_core::{LineIndex, Name, PackageName, Position as CorePosition, TypeIndex};
use nova_jdk::JdkIndex;
use nova_refactor::extract_method::{
    ExtractMethod, ExtractMethodIssue, InsertionStrategy, Visibility,
};
use nova_refactor::TextRange;
use nova_types::Span;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractMethodCommandArgs {
    pub uri: Uri,
    pub range: Range,
    pub name: String,
    pub visibility: Visibility,
    pub insertion_strategy: InsertionStrategy,
}

/// Produces an Extract Method code action if the selected region is extractable.
///
/// The action is surfaced as a command because the client typically needs to
/// collect additional input (method name, visibility) before the edit can be
/// generated.
pub fn extract_method_code_action(source: &str, uri: Uri, lsp_range: Range) -> Option<CodeAction> {
    let index = LineIndex::new(source);
    let range = TextRange::new(
        index
            .offset_of_position(
                source,
                CorePosition::new(lsp_range.start.line, lsp_range.start.character),
            )?
            .into(),
        index
            .offset_of_position(
                source,
                CorePosition::new(lsp_range.end.line, lsp_range.end.character),
            )?
            .into(),
    );

    // Probe analysis to see if extraction is possible; use a placeholder name.
    let probe = ExtractMethod {
        file: uri.to_string(),
        selection: range,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let analysis = probe.analyze(source).ok()?;
    let extractable = analysis
        .issues
        .iter()
        .all(|issue| matches!(issue, ExtractMethodIssue::NameCollision { .. }));

    if extractable {
        let args = ExtractMethodCommandArgs {
            uri,
            range: lsp_range,
            name: probe.name,
            visibility: probe.visibility,
            insertion_strategy: probe.insertion_strategy,
        };

        Some(CodeAction {
            title: "Extract method…".to_string(),
            kind: Some(CodeActionKind::REFACTOR_EXTRACT),
            command: Some(Command {
                title: "Extract method".to_string(),
                command: "nova.extractMethod".to_string(),
                arguments: Some(vec![serde_json::to_value(args).ok()?]),
            }),
            ..Default::default()
        })
    } else {
        None
    }
}

/// Generate quick fixes for a code-action request based on the supplied diagnostics.
///
/// This is designed to be used by LSP layers that already have a list of diagnostics relevant to
/// the requested selection range (e.g. `CodeActionParams.context.diagnostics`).
///
/// Today this provides quick-fixes:
/// - `unresolved-type` → `Create class '<Name>'` / `Import <fqn>` / `Use fully qualified name '<fqn>'`
/// - `unresolved-name` →
///   - lowercase identifiers: `Create local variable '<name>'` / `Create field '<name>'`
///   - uppercase identifiers: `Import <fqn>` / `Use fully qualified name '<fqn>'`
/// - `unresolved-method` / `UNRESOLVED_REFERENCE` → `Create method '<name>'`
/// - `unresolved-field` → `Create field '<name>'`
/// - `FLOW_UNREACHABLE` → `Remove unreachable code`
/// - `FLOW_UNASSIGNED` → `Initialize '<name>'`
/// - `type-mismatch` → `Cast to <expected>` / `Convert to String`
/// - `return-mismatch` → `Remove returned value` / `Cast to <expected>`
/// - `unresolved-import` → `Remove unresolved import`
/// - `unused-import` → `Remove unused import`
/// - `duplicate-import` → `Remove duplicate import`
/// - `FLOW_NULL_DEREF` → `Wrap with Objects.requireNonNull`
pub fn diagnostic_quick_fixes(
    source: &str,
    uri: Option<Uri>,
    selection: Range,
    diagnostics: &[Diagnostic],
) -> Vec<CodeAction> {
    let Some(uri) = uri else {
        return Vec::new();
    };

    let mut actions = Vec::new();
    let mut seen_create_symbol_titles: HashSet<String> = HashSet::new();

    for diag in diagnostics {
        if let Some(action) = create_class_quick_fix(source, &uri, &selection, diag) {
            actions.push(action);
        }
        actions.extend(unresolved_type_import_quick_fixes(
            source, &uri, &selection, diag,
        ));

        for action in create_symbol_quick_fixes(source, &uri, &selection, diag) {
            if !seen_create_symbol_titles.insert(action.title.clone()) {
                continue;
            }
            actions.push(action);
        }
        for action in create_unresolved_name_quick_fixes(source, &uri, &selection, diag) {
            if !seen_create_symbol_titles.insert(action.title.clone()) {
                continue;
            }
            actions.push(action);
        }
        if let Some(action) = remove_unreachable_code_quick_fix(source, &uri, &selection, diag) {
            actions.push(action);
        }
        if let Some(action) = initialize_unassigned_local_quick_fix(source, &uri, &selection, diag)
        {
            actions.push(action);
        }
        if let Some(action) = remove_unused_import_quick_fix(source, &uri, &selection, diag) {
            actions.push(action);
        }
        if let Some(action) = remove_unresolved_import_quick_fix(source, &uri, &selection, diag) {
            actions.push(action);
        }
        if let Some(action) = remove_duplicate_import_quick_fix(source, &uri, &selection, diag) {
            actions.push(action);
        }
        actions.extend(type_mismatch_quick_fixes(source, &uri, &selection, diag));
        actions.extend(return_mismatch_quick_fixes(source, &uri, &selection, diag));
        if let Some(action) = flow_null_deref_quick_fix(source, &uri, &selection, diag) {
            actions.push(action);
        }
    }

    // Mirror the built-in `IdeExtensions::code_actions_lsp` quick-fix set: offer JDK
    // static-member fixes (qualify / add static import) when the diagnostic + selection indicate
    // an unresolved *unqualified* identifier.
    if let Some(selection_span) = lsp_range_to_span(source, &selection) {
        let converted_diagnostics: Vec<nova_types::Diagnostic> = diagnostics
            .iter()
            .filter_map(|diagnostic| {
                let code = diagnostic_code(diagnostic)?;
                let span = lsp_range_to_span(source, &diagnostic.range)?;
                Some(nova_types::Diagnostic {
                    severity: nova_types::Severity::Error,
                    code: Cow::Owned(code.to_string()),
                    message: diagnostic.message.clone(),
                    span: Some(span),
                })
            })
            .collect();

        actions.extend(crate::quick_fixes::unresolved_static_member_quick_fixes(
            source,
            &uri,
            selection_span,
            &converted_diagnostics,
        ));
    }

    actions
}

fn create_symbol_quick_fixes(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Vec<CodeAction> {
    let Some(code) = diagnostic_code(diagnostic) else {
        return Vec::new();
    };

    let kind = match code {
        "unresolved-method" | "UNRESOLVED_REFERENCE" => UnresolvedMemberKind::Method,
        "unresolved-field" => UnresolvedMemberKind::Field,
        _ => return Vec::new(),
    };

    if !ranges_intersect(selection, &diagnostic.range) {
        return Vec::new();
    }

    let span = match lsp_range_to_span(source, &diagnostic.range) {
        Some(span) => span,
        None => return Vec::new(),
    };

    let Some(name) = crate::quick_fixes::unresolved_member_name(&diagnostic.message, source, span)
    else {
        return Vec::new();
    };

    let snippet = source.get(span.start..span.end).unwrap_or_default();
    if !crate::quick_fixes::looks_like_enclosing_member_access(snippet) {
        return Vec::new();
    }

    let is_static = match kind {
        UnresolvedMemberKind::Method => {
            diagnostic.message.contains("static context")
                || crate::quick_fixes::looks_like_static_receiver(snippet)
                || crate::quick_fixes::is_within_static_block(source, span.start)
        }
        UnresolvedMemberKind::Field => crate::quick_fixes::looks_like_static_receiver(snippet),
    };

    let (insert_offset, indent) = crate::quick_fixes::insertion_point(source);
    let insert_pos = crate::text::offset_to_position(source, insert_offset);
    let insert_range = Range {
        start: insert_pos,
        end: insert_pos,
    };

    let (title, new_text) = match kind {
        UnresolvedMemberKind::Method => (
            format!("Create method '{name}'"),
            crate::quick_fixes::method_stub(&name, &indent, is_static),
        ),
        UnresolvedMemberKind::Field => (
            format!("Create field '{name}'"),
            crate::quick_fixes::field_stub(&name, &indent, is_static),
        ),
    };

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: insert_range,
            new_text,
        }],
    );

    vec![CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    }]
}

fn create_unresolved_name_quick_fixes(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Vec<CodeAction> {
    if diagnostic_code(diagnostic) != Some("unresolved-name") {
        return Vec::new();
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return Vec::new();
    }

    let Some(name) = extract_unresolved_name(&diagnostic.message, source, &diagnostic.range) else {
        return Vec::new();
    };

    // Lowercase identifiers are assumed to be missing values (locals/fields).
    if looks_like_value_identifier(&name) {
        let mut actions = Vec::new();

        // Create local variable: insert before the current line (line containing the unresolved name).
        if let Some(start_offset) = crate::text::position_to_offset(source, diagnostic.range.start)
        {
            if let Some((line_start, indent)) = line_start_and_indent(source, start_offset) {
                let line_ending = if source.contains("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                };
                let new_text = format!("{indent}Object {name} = null;{line_ending}");

                let insert_pos = crate::text::offset_to_position(source, line_start);
                let insert_range = Range {
                    start: insert_pos,
                    end: insert_pos,
                };

                let mut changes = HashMap::new();
                changes.insert(
                    uri.clone(),
                    vec![TextEdit {
                        range: insert_range,
                        new_text,
                    }],
                );

                actions.push(CodeAction {
                    title: format!("Create local variable '{name}'"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        document_changes: None,
                        change_annotations: None,
                    }),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    ..CodeAction::default()
                });
            }
        }

        // Create field: insert near end of file before final `}` with best-effort indentation.
        if source.rfind('}').is_some() {
            let (insert_offset, indent) = insertion_point_for_member(source);
            let insert_pos = crate::text::offset_to_position(source, insert_offset);
            let insert_range = Range {
                start: insert_pos,
                end: insert_pos,
            };
            let prefix = insertion_prefix(source, insert_offset);
            let new_text = format!("{prefix}{indent}private Object {name};\n");

            let mut changes = HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: insert_range,
                    new_text,
                }],
            );

            actions.push(CodeAction {
                title: format!("Create field '{name}'"),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }),
                diagnostics: Some(vec![diagnostic.clone()]),
                ..CodeAction::default()
            });
        }

        return actions;
    }

    // Uppercase identifiers are often missing types used as qualifiers in expression position
    // (e.g. `List.of(...)`).
    if looks_like_type_identifier(&name) {
        // Avoid offering import/FQN fixes if the span already contains a qualification
        // (e.g. `java.util.List`).
        if source_range_text(source, &diagnostic.range).is_some_and(|snippet| snippet.contains('.'))
        {
            return Vec::new();
        }

        let candidates = crate::quickfix::import_candidates(&name);
        let mut actions = Vec::new();

        for fqn in candidates {
            if let Some(import_edit) = crate::quickfix::java_import_text_edit(source, &fqn) {
                let mut changes = HashMap::new();
                changes.insert(uri.clone(), vec![import_edit]);
                actions.push(CodeAction {
                    title: format!("Import {fqn}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        document_changes: None,
                        change_annotations: None,
                    }),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    ..CodeAction::default()
                });
            }

            let (start, end) = normalize_range(&diagnostic.range);
            let mut changes = HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: Range { start, end },
                    new_text: fqn.clone(),
                }],
            );
            actions.push(CodeAction {
                title: format!("Use fully qualified name '{fqn}'"),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }),
                diagnostics: Some(vec![diagnostic.clone()]),
                ..CodeAction::default()
            });
        }

        return actions;
    }

    Vec::new()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnresolvedMemberKind {
    Method,
    Field,
}

fn lsp_range_to_span(source: &str, range: &Range) -> Option<Span> {
    let (start_pos, end_pos) = normalize_range(range);
    let start_offset = crate::text::position_to_offset(source, start_pos)?;
    let end_offset = crate::text::position_to_offset(source, end_pos)?;
    let (start_offset, end_offset) = (start_offset.min(end_offset), start_offset.max(end_offset));
    Some(Span::new(start_offset, end_offset))
}

fn create_class_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "unresolved-type" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let name = unresolved_type_name(&diagnostic.message)?;
    if !is_simple_type_identifier(name) {
        return None;
    }

    let insert_pos = crate::text::offset_to_position(source, source.len());
    let insert_range = Range {
        start: insert_pos,
        end: insert_pos,
    };

    let prefix = if source.ends_with('\n') { "\n" } else { "\n\n" };
    let new_text = format!("{prefix}class {name} {{\n}}\n");

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: insert_range,
            new_text,
        }],
    );

    Some(CodeAction {
        title: format!("Create class '{name}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn unresolved_type_import_quick_fixes(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Vec<CodeAction> {
    let Some(code) = diagnostic_code(diagnostic) else {
        return Vec::new();
    };
    if code != "unresolved-type" {
        return Vec::new();
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return Vec::new();
    }

    let name_from_source = source_range_text(source, &diagnostic.range)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let name_from_message = unresolved_type_name(&diagnostic.message).map(str::to_string);

    // Prefer the range text (it's typically the most precise), but fall back to the diagnostic
    // message if the range is empty or doesn't yield a simple type identifier.
    let name = name_from_source
        .filter(|name| is_simple_type_identifier(name))
        .or_else(|| name_from_message.filter(|name| is_simple_type_identifier(name)));
    let Some(name) = name else {
        return Vec::new();
    };

    let candidates = unresolved_type_import_candidates(&name);
    let mut actions = Vec::new();

    for fqn in candidates {
        if let Some(import_edit) = crate::quickfix::java_import_text_edit(source, &fqn) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![import_edit]);
            actions.push(CodeAction {
                title: format!("Import {fqn}"),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }),
                diagnostics: Some(vec![diagnostic.clone()]),
                ..CodeAction::default()
            });
        }

        let (start, end) = normalize_range(&diagnostic.range);
        let mut changes = HashMap::new();
        changes.insert(
            uri.clone(),
            vec![TextEdit {
                range: Range { start, end },
                new_text: fqn.clone(),
            }],
        );
        actions.push(CodeAction {
            title: format!("Use fully qualified name '{fqn}'"),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }),
            diagnostics: Some(vec![diagnostic.clone()]),
            ..CodeAction::default()
        });
    }

    actions
}

fn unresolved_type_import_candidates(unresolved_name: &str) -> Vec<String> {
    let needle = unresolved_name.trim();
    if needle.is_empty() {
        return Vec::new();
    }

    // Use the built-in (dependency-free) JDK index for deterministic and low-latency suggestions.
    // This avoids triggering expensive on-disk JDK discovery/indexing in the stdio-LSP path.
    static BUILTIN_JDK: Lazy<JdkIndex> = Lazy::new(JdkIndex::new);

    // NOTE: This list is intentionally small and ordered by rough "how likely is a missing import
    // from here?" heuristics. We still sort/dedupe the final output for deterministic results.
    //
    // Keep this bounded: quick-fix code actions run on latency-sensitive paths, and probing the
    // entire JDK index (e.g. by enumerating all class names) can be extremely expensive.
    const COMMON_PACKAGES: &[&str] = &[
        "java.util",
        "java.util.function",
        "java.io",
        "java.time",
        "java.nio",
        "java.nio.file",
        "java.net",
        "java.math",
        "java.util.regex",
        "java.util.concurrent",
        "java.util.stream",
        "java.lang",
    ];

    // Some very common nested types are referred to by their simple inner name (e.g. `Entry`)
    // and can be imported directly (`import java.util.Map.Entry;`). Those types are stored in
    // indices under their binary `$` names (`Map$Entry`), so we probe a small, fixed set of
    // common outers to retain nested type coverage without enumerating the entire JDK.
    const JAVA_UTIL_NESTED_OUTERS: &[&str] = &["Map"];
    const JAVA_LANG_NESTED_OUTERS: &[&str] = &["Thread"];

    let name = Name::from(needle);

    let mut out = Vec::new();
    for pkg_str in COMMON_PACKAGES {
        let pkg = PackageName::from_dotted(pkg_str);
        if let Some(ty) = BUILTIN_JDK.resolve_type_in_package(&pkg, &name) {
            // JDK indices use binary names for nested types (`Outer$Inner`). Java imports use source
            // names (`Outer.Inner`), so replace `$` with `.` as a best-effort.
            out.push(ty.as_str().replace('$', "."));
        }

        let nested_outers: &[&str] = match *pkg_str {
            "java.util" => JAVA_UTIL_NESTED_OUTERS,
            "java.lang" => JAVA_LANG_NESTED_OUTERS,
            _ => &[],
        };
        for outer in nested_outers {
            let nested = Name::from(format!("{outer}${needle}"));
            if let Some(ty) = BUILTIN_JDK.resolve_type_in_package(&pkg, &nested) {
                out.push(ty.as_str().replace('$', "."));
            }
        }
    }

    out.sort();
    out.dedup();
    out.truncate(5);
    out
}

fn remove_unreachable_code_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "FLOW_UNREACHABLE" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let delete_range = full_line_range(source, &diagnostic.range)?;

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: delete_range,
            new_text: String::new(),
        }],
    );

    Some(CodeAction {
        title: "Remove unreachable code".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn initialize_unassigned_local_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "FLOW_UNASSIGNED" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let name = backticked_name(&diagnostic.message)?;

    let start_offset = crate::text::position_to_offset(source, diagnostic.range.start)?;
    let (line_start, indent) = line_start_and_indent(source, start_offset)?;

    let default_value = infer_default_value_for_local(source, name, start_offset);
    let line_ending = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let new_text = format!("{indent}{name} = {default_value};{line_ending}");

    let insert_pos = crate::text::offset_to_position(source, line_start);
    let insert_range = Range {
        start: insert_pos,
        end: insert_pos,
    };

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: insert_range,
            new_text,
        }],
    );

    Some(CodeAction {
        title: format!("Initialize '{name}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn remove_unused_import_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "unused-import" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let delete_range = full_line_range(source, &diagnostic.range)?;

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: delete_range,
            new_text: String::new(),
        }],
    );

    Some(CodeAction {
        title: "Remove unused import".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn remove_unresolved_import_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "unresolved-import" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let delete_range = full_line_range(source, &diagnostic.range)?;

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: delete_range,
            new_text: String::new(),
        }],
    );

    Some(CodeAction {
        title: "Remove unresolved import".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn remove_duplicate_import_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "duplicate-import" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let delete_range = full_line_range(source, &diagnostic.range)?;

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: delete_range,
            new_text: String::new(),
        }],
    );

    Some(CodeAction {
        title: "Remove duplicate import".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn type_mismatch_quick_fixes(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Vec<CodeAction> {
    fn cast_replacement(expected: &str, expr: &str) -> String {
        if is_simple_cast_expr(expr) {
            format!("({expected}) {expr}")
        } else {
            format!("({expected}) ({expr})")
        }
    }

    if diagnostic_code(diagnostic) != Some("type-mismatch") {
        return Vec::new();
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return Vec::new();
    }

    let Some((expected, _found)) = parse_type_mismatch(&diagnostic.message) else {
        return Vec::new();
    };

    let (start_pos, end_pos) = normalize_range(&diagnostic.range);
    let start_offset = crate::text::position_to_offset(source, start_pos);
    let end_offset = crate::text::position_to_offset(source, end_pos);
    let (Some(start_offset), Some(end_offset)) = (start_offset, end_offset) else {
        return Vec::new();
    };
    let (start_offset, end_offset) = (start_offset.min(end_offset), start_offset.max(end_offset));

    let expr = source
        .get(start_offset..end_offset)
        .unwrap_or_default()
        .trim();
    if expr.is_empty() {
        return Vec::new();
    }

    let range = Range {
        start: start_pos,
        end: end_pos,
    };

    fn single_replace_edit(uri: &Uri, range: Range, new_text: String) -> WorkspaceEdit {
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    let mut actions = Vec::new();

    if expected == "String" {
        actions.push(CodeAction {
            title: "Convert to String".to_string(),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(single_replace_edit(
                uri,
                range,
                format!("String.valueOf({expr})"),
            )),
            diagnostics: Some(vec![diagnostic.clone()]),
            is_preferred: Some(true),
            ..CodeAction::default()
        });
    }

    actions.push(CodeAction {
        title: format!("Cast to {expected}"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(single_replace_edit(
            uri,
            range,
            cast_replacement(&expected, expr),
        )),
        diagnostics: Some(vec![diagnostic.clone()]),
        is_preferred: Some(expected != "String"),
        ..CodeAction::default()
    });

    actions
}

fn return_mismatch_quick_fixes(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Vec<CodeAction> {
    if diagnostic_code(diagnostic) != Some("return-mismatch") {
        return Vec::new();
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return Vec::new();
    }

    let (start_pos, end_pos) = normalize_range(&diagnostic.range);
    let range = Range {
        start: start_pos,
        end: end_pos,
    };

    fn single_replace_edit(uri: &Uri, range: Range, new_text: String) -> WorkspaceEdit {
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    if diagnostic
        .message
        .contains("cannot return a value from a `void` method")
    {
        return vec![CodeAction {
            title: "Remove returned value".to_string(),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(single_replace_edit(uri, range, String::new())),
            diagnostics: Some(vec![diagnostic.clone()]),
            ..CodeAction::default()
        }];
    }

    let Some((expected, found)) = parse_return_mismatch(&diagnostic.message) else {
        return Vec::new();
    };
    if found == "void" {
        return Vec::new();
    }

    let start_offset = crate::text::position_to_offset(source, start_pos);
    let end_offset = crate::text::position_to_offset(source, end_pos);
    let (Some(start_offset), Some(end_offset)) = (start_offset, end_offset) else {
        return Vec::new();
    };
    let (start_offset, end_offset) = (start_offset.min(end_offset), start_offset.max(end_offset));

    let expr = source
        .get(start_offset..end_offset)
        .unwrap_or_default()
        .trim();
    if expr.is_empty() {
        return Vec::new();
    }

    let replacement = format!("({expected}) ({expr})");
    vec![CodeAction {
        title: format!("Cast to {expected}"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(single_replace_edit(uri, range, replacement)),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    }]
}

fn flow_null_deref_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "FLOW_NULL_DEREF" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let start = crate::text::position_to_offset(source, diagnostic.range.start)?;
    let end = crate::text::position_to_offset(source, diagnostic.range.end)?;
    if start >= end {
        return None;
    }

    let expr = source.get(start..end)?;
    let (receiver, rest) = split_member_access(expr)?;
    let receiver = receiver.trim();
    if receiver.is_empty() {
        return None;
    }

    let new_expr = format!("java.util.Objects.requireNonNull({receiver}){rest}");

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: diagnostic.range.clone(),
            new_text: new_expr,
        }],
    );

    Some(CodeAction {
        title: "Wrap with Objects.requireNonNull".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn diagnostic_code(diagnostic: &Diagnostic) -> Option<&str> {
    match diagnostic.code.as_ref()? {
        NumberOrString::String(code) => Some(code.as_str()),
        NumberOrString::Number(_) => None,
    }
}

fn unresolved_type_name(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("unresolved type `")?;
    rest.strip_suffix('`')
}

fn backticked_name(message: &str) -> Option<&str> {
    // Flow diagnostics for unassigned locals use the format:
    // `use of local `<name>` before definite assignment`
    let start = message.find('`')?;
    let rest = &message[start + 1..];
    let end = rest.find('`')?;
    Some(rest[..end].trim())
}

fn extract_unresolved_name(message: &str, source: &str, range: &Range) -> Option<String> {
    if let Some(name) = backticked_name(message) {
        return Some(name.to_string());
    }

    source_range_text(source, range).map(|s| s.to_string())
}

fn looks_like_value_identifier(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(|b| matches!(b, b'a'..=b'z'))
}

fn is_java_identifier(s: &str) -> bool {
    fn is_ident_start(c: char) -> bool {
        c == '_' || c == '$' || c.is_ascii_alphabetic()
    }

    fn is_ident_continue(c: char) -> bool {
        is_ident_start(c) || c.is_ascii_digit()
    }

    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ident_start(first) {
        return false;
    }
    chars.all(is_ident_continue)
}

fn looks_like_type_identifier(name: &str) -> bool {
    if !crate::quick_fixes::is_java_identifier(name) {
        return false;
    }

    name.as_bytes()
        .first()
        .is_some_and(|b| matches!(b, b'A'..=b'Z'))
}

fn parse_type_mismatch(message: &str) -> Option<(String, String)> {
    let message = message.strip_prefix("type mismatch: expected ")?;
    let (expected, found) = message.split_once(", found ")?;
    Some((expected.trim().to_string(), found.trim().to_string()))
}

fn parse_return_mismatch(message: &str) -> Option<(String, String)> {
    let message = message.strip_prefix("return type mismatch: expected ")?;
    let (expected, found) = message.split_once(", found ")?;
    Some((expected.trim().to_string(), found.trim().to_string()))
}

pub(crate) fn is_simple_cast_expr(expr: &str) -> bool {
    static IDENT_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[A-Za-z_$][A-Za-z0-9_$]*$").expect("valid regex"));
    // Best-effort Java numeric literal support (common int/float/hex/binary forms). This is used
    // only to decide whether the quick fix should emit extra parentheses.
    static NUMBER_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?x)
            ^
            (?:
                0[xX][0-9A-Fa-f][0-9A-Fa-f_]*[lL]?              # hex int
              | 0[bB][01][01_]*[lL]?                             # binary int
              | [0-9][0-9_]*[lL]?                                # decimal int (optional long suffix)
              | (?:                                              # decimal float with dot
                    [0-9][0-9_]*\.[0-9][0-9_]*
                  | [0-9][0-9_]*\.
                  | \.[0-9][0-9_]*
                )
                (?:[eE][+-]?[0-9][0-9_]*)?                       # optional exponent
                [fFdD]?                                          # optional float/double suffix
              | [0-9][0-9_]*[eE][+-]?[0-9][0-9_]*[fFdD]?         # decimal float with exponent
              | [0-9][0-9_]*[fFdD]                               # decimal float with suffix
            )
            $
            ",
        )
        .expect("valid regex")
    });
    // Minimal Java-like string literal: double-quoted, allowing escaped chars.
    static STRING_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"^"(?:\\.|[^"\\])*"$"#).expect("valid regex"));

    IDENT_RE.is_match(expr) || NUMBER_RE.is_match(expr) || STRING_RE.is_match(expr)
}

fn source_range_text<'a>(source: &'a str, range: &Range) -> Option<&'a str> {
    let start = crate::text::position_to_offset(source, range.start)?;
    let end = crate::text::position_to_offset(source, range.end)?;
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    source.get(start..end)
}

fn insertion_point_for_member(source: &str) -> (usize, String) {
    let Some(close_brace) = source.rfind('}') else {
        return (source.len(), "  ".to_string());
    };

    let line_start = source[..close_brace]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let before_brace_on_line = &source[line_start..close_brace];
    let close_indent: String = before_brace_on_line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();

    // If the last brace is on its own line, insert before the indentation so the stub appears
    // above the brace.
    let (insert_offset, indent) = if before_brace_on_line.trim().is_empty() {
        (line_start, format!("{close_indent}  "))
    } else {
        (close_brace, "  ".to_string())
    };

    (insert_offset, indent)
}

fn insertion_prefix(source: &str, insert_offset: usize) -> &'static str {
    if insert_offset > 0 && source.as_bytes().get(insert_offset - 1) == Some(&b'\n') {
        "\n"
    } else {
        "\n\n"
    }
}

fn is_simple_type_identifier(name: &str) -> bool {
    if name.is_empty() || name.contains('.') || name.contains('$') {
        return false;
    }

    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn ranges_intersect(a: &Range, b: &Range) -> bool {
    let (a_start, a_end) = normalize_range(a);
    let (b_start, b_end) = normalize_range(b);

    if a_start == a_end {
        return position_within_range(b_start, b_end, a_start);
    }
    if b_start == b_end {
        return position_within_range(a_start, a_end, b_start);
    }

    pos_lt(&a_start, &b_end) && pos_lt(&b_start, &a_end)
}

fn normalize_range(range: &Range) -> (Position, Position) {
    if pos_leq(&range.start, &range.end) {
        (range.start, range.end)
    } else {
        (range.end, range.start)
    }
}

fn full_line_range(source: &str, range: &Range) -> Option<Range> {
    let (start, end) = normalize_range(range);
    let start_offset = crate::text::position_to_offset(source, start)?;
    let end_offset = crate::text::position_to_offset(source, end)?;

    let start_offset = start_offset.min(source.len());
    let end_offset = end_offset.min(source.len());

    let line_start = source[..start_offset]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let line_end = source[end_offset..]
        .find('\n')
        .map(|idx| end_offset + idx + 1)
        .unwrap_or(source.len());

    Some(Range {
        start: crate::text::offset_to_position(source, line_start),
        end: crate::text::offset_to_position(source, line_end),
    })
}

fn line_start_and_indent(source: &str, offset: usize) -> Option<(usize, &str)> {
    let offset = offset.min(source.len());
    let line_start = source[..offset].rfind('\n').map(|idx| idx + 1).unwrap_or(0);

    let line = source.get(line_start..)?;
    let indent_len: usize = line
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .map(char::len_utf8)
        .sum();
    let indent = line.get(..indent_len)?;

    Some((line_start, indent))
}

fn infer_default_value_for_local(source: &str, name: &str, before_offset: usize) -> &'static str {
    let before_offset = before_offset.min(source.len());
    let prefix = &source[..before_offset];

    // Best-effort: detect primitive local declarations. For all non-primitives we default to
    // `null` anyway, so we can keep the type parsing narrow and avoid false positives.
    let pat = format!(
        r"^\s*(?:@\w+(?:\([^)]*\))?\s+)*(?:final\s+)?(?P<ty>byte|short|int|long|float|double|boolean|char)(?P<array1>(?:\[\])*)\s+{}\b",
        regex::escape(name)
    );
    let re = Regex::new(&pat).ok();

    if let Some(re) = re {
        for line in prefix.lines().rev() {
            let Some(caps) = re.captures(line) else {
                continue;
            };
            let ty = caps.name("ty").map(|m| m.as_str()).unwrap_or("");
            let array1 = caps.name("array1").map(|m| m.as_str()).unwrap_or("");

            // Handle the alternative Java array syntax: `int x[];` (brackets after the name).
            let array2 = line.contains(&format!("{name}[]"));

            if !array1.is_empty() || array2 {
                return "null";
            }

            return match ty {
                "boolean" => "false",
                "char" => "'\\0'",
                "byte" | "short" | "int" | "long" | "float" | "double" => "0",
                _ => "null",
            };
        }
    }

    "null"
}

fn position_within_range(start: Position, end: Position, pos: Position) -> bool {
    pos_leq(&start, &pos) && pos_leq(&pos, &end)
}

fn pos_lt(a: &Position, b: &Position) -> bool {
    (a.line, a.character) < (b.line, b.character)
}

fn pos_leq(a: &Position, b: &Position) -> bool {
    (a.line, a.character) <= (b.line, b.character)
}

/// Split `expr` into `(receiver, rest)` at the last top-level `.`.
///
/// `rest` includes the `.` and the member/call suffix.
fn split_member_access(expr: &str) -> Option<(&str, &str)> {
    let mut paren_depth = 0u32;
    let mut bracket_depth = 0u32;
    let mut brace_depth = 0u32;
    let mut last_dot: Option<usize> = None;

    for (idx, ch) in expr.char_indices() {
        match ch {
            '(' => paren_depth = paren_depth.saturating_add(1),
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth = bracket_depth.saturating_add(1),
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth = brace_depth.saturating_add(1),
            '}' => brace_depth = brace_depth.saturating_sub(1),
            '.' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                last_dot = Some(idx)
            }
            _ => {}
        }
    }

    let dot = last_dot?;
    let (receiver, rest) = expr.split_at(dot);
    if rest == "." {
        return None;
    }
    Some((receiver, rest))
}
