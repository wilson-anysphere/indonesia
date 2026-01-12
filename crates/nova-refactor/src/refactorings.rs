use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use nova_core::{file_uri_to_path, path_to_file_uri, AbsPathBuf};
use nova_format::NewlineStyle;
use nova_index::normalize_type_signature;
use nova_syntax::ast::{self, AstNode};
use nova_syntax::{parse_java, SyntaxKind};
use thiserror::Error;

use crate::edit::{apply_text_edits, FileId, TextEdit, TextRange, WorkspaceEdit};
use crate::java::{JavaSymbolKind, SymbolId};
use crate::materialize::{materialize, MaterializeError};
use crate::semantic::{Conflict, RefactorDatabase, ReferenceKind, SemanticChange};

#[derive(Debug, Error)]
pub enum RefactorError {
    #[error("refactoring has conflicts: {0:?}")]
    Conflicts(Vec<Conflict>),
    #[error("rename is not supported for this symbol (got {kind:?})")]
    RenameNotSupported { kind: Option<JavaSymbolKind> },
    #[error("extract variable is not supported inside assert statements")]
    ExtractNotSupportedInAssert,
    #[error(transparent)]
    Materialize(#[from] MaterializeError),
    #[error(transparent)]
    MoveJava(#[from] crate::move_java::RefactorError),
    #[error("invalid file id `{file:?}`: {reason}")]
    InvalidFileId { file: FileId, reason: String },
    #[error("unknown file {0:?}")]
    UnknownFile(FileId),
    #[error("invalid variable name `{name}`: {reason}")]
    InvalidIdentifier { name: String, reason: &'static str },
    #[error("expected a variable with initializer for inline")]
    InlineNotSupported,
    #[error("inline variable is not supported inside assert statements")]
    InlineNotSupportedInAssert,
    #[error("no variable usage at the given cursor/usage range")]
    InlineNoUsageAtCursor,
    #[error("variable initializer has side effects and cannot be inlined safely")]
    InlineSideEffects,
    #[error("inlining would change value: {reason}")]
    InlineWouldChangeValue { reason: String },
    #[error("cannot inline: `{name}` would resolve differently at the usage site")]
    InlineShadowedDependency { name: String },
    #[error("failed to parse Java source")]
    ParseError,
    #[error("selection does not resolve to a single expression")]
    InvalidSelection,
    #[error("extract variable is not supported in this context: {reason}")]
    ExtractNotSupported { reason: &'static str },
    #[error("expression has side effects and cannot be extracted safely")]
    ExtractSideEffects,
    #[error("could not infer type for extracted expression")]
    TypeInferenceFailed,
    #[error("cannot use `var` for this initializer; use an explicit type")]
    VarNotAllowedForInitializer,
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

pub struct RenameParams {
    pub symbol: SymbolId,
    pub new_name: String,
}

pub fn rename(
    db: &dyn RefactorDatabase,
    params: RenameParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let kind = db.symbol_kind(params.symbol);

    // Package rename is implemented as a Java package move (directory + package decl updates).
    if matches!(kind, Some(JavaSymbolKind::Package)) {
        let def = db
            .symbol_definition(params.symbol)
            .ok_or(RefactorError::RenameNotSupported { kind })?;

        let all_files = db.all_files();
        let file_ids_are_file_uris = !all_files.is_empty()
            && all_files
                .iter()
                .all(|file| file_uri_to_path(&file.0).is_ok());

        let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
        let mut path_to_file_id: HashMap<PathBuf, FileId> = HashMap::new();
        for file in all_files {
            let text = db
                .file_text(&file)
                .ok_or_else(|| RefactorError::UnknownFile(file.clone()))?;

            if file_ids_are_file_uris {
                let abs =
                    file_uri_to_path(&file.0).map_err(|err| RefactorError::InvalidFileId {
                        file: file.clone(),
                        reason: err.to_string(),
                    })?;
                let path = abs.into_path_buf();
                path_to_file_id.insert(path.clone(), file.clone());
                files.insert(path, text.to_string());
            } else {
                files.insert(PathBuf::from(&file.0), text.to_string());
            }
        }

        let mut edit = crate::move_java::move_package_workspace_edit(
            &files,
            crate::move_java::MovePackageParams {
                old_package: def.name,
                new_package: params.new_name,
            },
        )?;

        if file_ids_are_file_uris {
            fn remap_file_id(
                file: &FileId,
                path_to_file_id: &HashMap<PathBuf, FileId>,
            ) -> Result<FileId, RefactorError> {
                let path = PathBuf::from(&file.0);
                if let Some(existing) = path_to_file_id.get(&path) {
                    return Ok(existing.clone());
                }

                let abs = AbsPathBuf::new(path).map_err(|err| RefactorError::InvalidFileId {
                    file: file.clone(),
                    reason: err.to_string(),
                })?;
                let uri = path_to_file_uri(&abs).map_err(|err| RefactorError::InvalidFileId {
                    file: file.clone(),
                    reason: err.to_string(),
                })?;
                Ok(FileId::new(uri))
            }

            for op in &mut edit.file_ops {
                match op {
                    crate::edit::FileOp::Rename { from, to } => {
                        *from = remap_file_id(from, &path_to_file_id)?;
                        *to = remap_file_id(to, &path_to_file_id)?;
                    }
                    crate::edit::FileOp::Create { file, .. } => {
                        *file = remap_file_id(file, &path_to_file_id)?;
                    }
                    crate::edit::FileOp::Delete { file } => {
                        *file = remap_file_id(file, &path_to_file_id)?;
                    }
                }
            }
            for text_edit in &mut edit.text_edits {
                text_edit.file = remap_file_id(&text_edit.file, &path_to_file_id)?;
            }

            edit.normalize()?;
        }

        return Ok(edit);
    }

    match kind {
        Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter | JavaSymbolKind::TypeParameter) => {
            let conflicts = check_rename_conflicts(db, params.symbol, &params.new_name);
            if !conflicts.is_empty() {
                return Err(RefactorError::Conflicts(conflicts));
            }

            let changes = vec![SemanticChange::Rename {
                symbol: params.symbol,
                new_name: params.new_name,
            }];
            Ok(materialize(db, changes)?)
        }
        Some(JavaSymbolKind::Type) => {
            let conflicts = check_rename_conflicts(db, params.symbol, &params.new_name);
            if !conflicts.is_empty() {
                return Err(RefactorError::Conflicts(conflicts));
            }

            let new_name = params.new_name;
            let changes = vec![SemanticChange::Rename {
                symbol: params.symbol,
                new_name: new_name.clone(),
            }];
            let mut edit = materialize(db, changes)?;

            // Optional file rename for public top-level types:
            // `p/Foo.java` -> `p/Bar.java` when renaming `Foo` -> `Bar`.
            if let Some(info) = db.type_symbol_info(params.symbol) {
                if info.is_top_level && info.is_public {
                    if let Some(def) = db.symbol_definition(params.symbol) {
                        if let Some((from, to)) =
                            type_file_rename_candidate(&def.file, &def.name, &new_name)
                        {
                            // File existence conflicts are checked in `check_rename_conflicts`.
                            edit.file_ops.push(crate::edit::FileOp::Rename { from, to });
                            // Materialize emitted edits against the pre-rename file id. Remap them so
                            // `WorkspaceEdit::normalize()` accepts the file op.
                            edit.remap_text_edits_across_renames()?;
                            edit.normalize()?;
                        }
                    }
                }
            }

            Ok(edit)
        }
        Some(JavaSymbolKind::Field) => rename_field_with_accessors(db, params),
        Some(JavaSymbolKind::Method) => {
            let new_name = params.new_name;
            let mut symbols = db.method_override_chain(params.symbol);
            if symbols.is_empty() {
                symbols.push(params.symbol);
            }

            // Dedup defensively in case the database returns duplicates.
            let mut seen = HashSet::new();
            symbols.retain(|sym| seen.insert(*sym));

            // Conflict detection should consider every method that will be renamed. For example,
            // renaming an overridden method may collide with an existing method in an overriding
            // subtype.
            let mut conflicts = Vec::new();
            for symbol in &symbols {
                conflicts.extend(check_rename_conflicts(db, *symbol, &new_name));
            }
            if !conflicts.is_empty() {
                return Err(RefactorError::Conflicts(conflicts));
            }

            let mut changes = symbols
                .into_iter()
                .map(|symbol| SemanticChange::Rename {
                    symbol,
                    new_name: new_name.clone(),
                })
                .collect::<Vec<_>>();

            // Java annotation shorthand `@Anno(expr)` is desugared as `@Anno(value = expr)`. If the
            // annotation element method `value()` is renamed, shorthand usages must be rewritten to
            // an explicit element-value pair using the new name.
            changes.extend(annotation_value_shorthand_updates(
                db,
                params.symbol,
                &new_name,
            ));

            Ok(materialize(db, changes)?)
        }
        Some(JavaSymbolKind::Package) | None => Err(RefactorError::RenameNotSupported { kind }),
    }
}

fn annotation_value_shorthand_updates(
    db: &dyn RefactorDatabase,
    symbol: SymbolId,
    new_name: &str,
) -> Vec<SemanticChange> {
    if new_name == "value" {
        return Vec::new();
    }

    let Some(kind) = db.symbol_kind(symbol) else {
        return Vec::new();
    };
    if kind != JavaSymbolKind::Method {
        return Vec::new();
    }

    let Some(def) = db.symbol_definition(symbol) else {
        return Vec::new();
    };
    if def.name != "value" {
        return Vec::new();
    }

    let Some(text) = db.file_text(&def.file) else {
        return Vec::new();
    };

    let parsed = parse_java(text);
    let root = parsed.syntax();

    // Find the method declaration in the syntax tree and confirm it's a 0-arg `value()` inside an
    // `@interface`. This ensures we only apply the rewrite when renaming the special annotation
    // element, not arbitrary methods named `value`.
    let mut annotation_name = None;
    let mut annotation_type_symbol = None;
    for method in root.descendants().filter_map(ast::MethodDeclaration::cast) {
        let Some(name_tok) = method.name_token() else {
            continue;
        };
        if syntax_token_range(&name_tok) != def.name_range {
            continue;
        }

        let param_count = method
            .parameter_list()
            .map(|list| list.parameters().count())
            .unwrap_or(0);
        if param_count != 0 {
            return Vec::new();
        }

        let Some(annotation_ty) = method
            .syntax()
            .ancestors()
            .find_map(ast::AnnotationTypeDeclaration::cast)
        else {
            return Vec::new();
        };
        let Some(annotation_name_tok) = annotation_ty.name_token() else {
            return Vec::new();
        };
        annotation_name = Some(annotation_name_tok.text().to_string());

        // Best-effort: disambiguate `@A(...)` occurrences by confirming the annotation *type*
        // reference matches the `@interface` we're renaming. Without this, workspaces containing
        // multiple annotation types with the same simple name (e.g. `p.A` and `q.A`) could be
        // rewritten incorrectly.
        //
        // If the database can't resolve symbols-at-offset (e.g. a parser-only implementation), we
        // fall back to matching on the simple name only.
        let type_name_range = syntax_token_range(&annotation_name_tok);
        if type_name_range.start < type_name_range.end {
            let offset = type_name_range.start + (type_name_range.end - type_name_range.start) / 2;
            annotation_type_symbol = db.symbol_at(&def.file, offset);
        }
        break;
    }

    let Some(annotation_name) = annotation_name else {
        return Vec::new();
    };

    let existing_refs = db.find_references(symbol);
    let annotation_type_refs_by_file: Option<HashMap<FileId, Vec<TextRange>>> =
        annotation_type_symbol.map(|sym| {
            let mut out: HashMap<FileId, Vec<TextRange>> = HashMap::new();
            for r in db.find_references(sym) {
                out.entry(r.file).or_default().push(r.range);
            }
            out
        });

    fn annotation_args_inner_range(
        source: &str,
        args: &ast::AnnotationElementValuePairList,
    ) -> Option<TextRange> {
        let range = syntax_range(args.syntax());
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

    fn annotation_name_token_range(name: &ast::Name) -> Option<TextRange> {
        let mut non_trivia_tokens = name
            .syntax()
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|tok| !tok.kind().is_trivia());
        let first = non_trivia_tokens.next()?;
        let last = non_trivia_tokens.last().unwrap_or_else(|| first.clone());
        Some(syntax_token_range(&last))
    }

    let mut out = Vec::new();
    let mut seen: HashSet<(FileId, TextRange)> = HashSet::new();

    for file in db.all_files() {
        let Some(source) = db.file_text(&file) else {
            continue;
        };
        let parsed = parse_java(source);
        let root = parsed.syntax();

        for ann in root.descendants().filter_map(ast::Annotation::cast) {
            let Some(name) = ann.name() else {
                continue;
            };
            let name_text = name.text();
            let simple = name_text
                .rsplit('.')
                .next()
                .unwrap_or_else(|| name_text.as_str());
            if simple != annotation_name {
                continue;
            }
            if let Some(refs_by_file) = &annotation_type_refs_by_file {
                let Some(name_range) = annotation_name_token_range(&name) else {
                    continue;
                };
                let Some(ranges) = refs_by_file.get(&file) else {
                    continue;
                };
                if !ranges.iter().any(|r| ranges_overlap(*r, name_range)) {
                    continue;
                }
            }

            let Some(args) = ann.arguments() else {
                continue;
            };

            let has_pairs = args.pairs().next().is_some();
            let value = args.value();

            // If the parse produced both a shorthand value and named pairs, skip (shouldn't happen
            // in valid Java).
            if value.is_some() && has_pairs {
                continue;
            }

            if let Some(value) = value {
                // Shorthand `@Anno(expr)` form.
                if has_pairs {
                    continue;
                }

                let Some(inner_range) = annotation_args_inner_range(source, &args) else {
                    continue;
                };
                if !seen.insert((file.clone(), inner_range)) {
                    continue;
                }

                let value_range = syntax_range(value.syntax());
                let value_text = source
                    .get(value_range.start..value_range.end)
                    .unwrap_or_default()
                    .trim();
                if value_text.is_empty() {
                    continue;
                }

                out.push(SemanticChange::UpdateReferences {
                    file: file.clone(),
                    range: inner_range,
                    new_text: format!("{new_name} = {value_text}"),
                });
            } else if has_pairs {
                // Named pair `@Anno(value = expr)` form.
                for pair in args.pairs() {
                    let Some(name_tok) = pair.name_token() else {
                        continue;
                    };
                    if name_tok.text() != "value" {
                        continue;
                    }

                    let name_range = syntax_token_range(&name_tok);
                    // If the semantic DB already records this as a reference, rely on the normal
                    // rename path to avoid overlapping edits.
                    if existing_refs
                        .iter()
                        .any(|r| r.file == file && ranges_overlap(r.range, name_range))
                    {
                        continue;
                    }

                    if !seen.insert((file.clone(), name_range)) {
                        continue;
                    }

                    out.push(SemanticChange::UpdateReferences {
                        file: file.clone(),
                        range: name_range,
                        new_text: new_name.to_string(),
                    });
                }
            }
        }
    }

    out
}

fn check_rename_conflicts(
    db: &dyn RefactorDatabase,
    symbol: SymbolId,
    new_name: &str,
) -> Vec<Conflict> {
    let mut conflicts = Vec::new();

    let Some(def) = db.symbol_definition(symbol) else {
        return conflicts;
    };

    let kind = db.symbol_kind(symbol);
    let refs = db.find_references(symbol);

    match kind {
        Some(JavaSymbolKind::Method) => {
            // Overload-aware collision detection:
            // Renaming a method to an existing name is OK as long as it doesn't create any
            // duplicate signatures (same parameter type list) in the same owning type.
            //
            // Note: Nova models overloaded methods as a single "method group" symbol (one per
            // `(declaring type, method name)`). So renaming a method symbol renames *all* overloads
            // in the group, and we must check each overload signature for conflicts.
            if new_name != def.name {
                let Some(text) = db.file_text(&def.file) else {
                    return conflicts;
                };

                let parsed = parse_java(text);
                let root = parsed.syntax();

                let Some(rep_decl) = root
                    .descendants()
                    .filter_map(ast::MethodDeclaration::cast)
                    .find(|method| {
                        method.name_token().is_some_and(|tok| {
                            syntax_token_range(&tok) == def.name_range && tok.text() == def.name
                        })
                    })
                else {
                    return conflicts;
                };

                let Some(type_body) = find_enclosing_type_body(rep_decl.syntax()) else {
                    return conflicts;
                };

                let old_overloads = method_overload_signatures(text, &type_body, &def.name);
                let new_overloads = method_overload_signatures(text, &type_body, new_name);

                if method_overload_sets_overlap(&old_overloads, &new_overloads) {
                    let existing_symbol = db
                        .resolve_methods_in_scope(def.scope, new_name)
                        .into_iter()
                        .find(|sym| *sym != symbol)
                        // Best-effort: we should always be able to find the existing symbol in this
                        // scope when the signature overlap came from declarations in the same type,
                        // but fall back to a sentinel if not.
                        .unwrap_or_else(|| SymbolId::new(u32::MAX));
                    conflicts.push(Conflict::NameCollision {
                        file: def.file.clone(),
                        name: new_name.to_string(),
                        existing_symbol,
                    });
                }
            }
        }
        Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter) => {
            let Some(scope) = db.symbol_scope(symbol) else {
                return conflicts;
            };

            if let Some(existing) = db.resolve_name_in_scope(scope, new_name) {
                if existing != symbol {
                    conflicts.push(Conflict::NameCollision {
                        file: def.file.clone(),
                        name: new_name.to_string(),
                        existing_symbol: existing,
                    });
                }
            }

            if let Some(shadowed) = db.would_shadow(scope, new_name) {
                if shadowed != symbol {
                    conflicts.push(Conflict::Shadowing {
                        file: def.file.clone(),
                        name: new_name.to_string(),
                        shadowed_symbol: shadowed,
                    });
                }
            }

            if let Some(existing) = db.would_be_shadowed(symbol, new_name) {
                if existing != symbol
                    && !conflicts.iter().any(|c| {
                        matches!(
                            c,
                            Conflict::NameCollision {
                                name,
                                existing_symbol,
                                ..
                            } if name == new_name && *existing_symbol == existing
                        )
                    })
                {
                    conflicts.push(Conflict::NameCollision {
                        file: def.file.clone(),
                        name: new_name.to_string(),
                        existing_symbol: existing,
                    });
                }
            }
        }
        Some(JavaSymbolKind::Field) => {
            // Best-effort: avoid obvious declaration-scope collisions (another field with the same
            // name in the same type).
            if let Some(scope) = db.symbol_scope(symbol) {
                if let Some(existing) = db.resolve_name_in_scope(scope, new_name) {
                    if existing != symbol && db.symbol_kind(existing) == Some(JavaSymbolKind::Field)
                    {
                        conflicts.push(Conflict::NameCollision {
                            file: def.file.clone(),
                            name: new_name.to_string(),
                            existing_symbol: existing,
                        });
                    }
                }
            }

            // It's possible for a field rename to introduce name capture at *some* usage sites
            // without colliding in the declaration scope.
            //
            // Example:
            //   class C { int foo; void m() { int bar; foo; } }
            // Renaming `foo -> bar` would cause the (renamed) `bar` reference to resolve to the
            // local `bar`, changing semantics.
            for usage in &refs {
                let Some(usage_scope) = usage.scope else {
                    continue;
                };
                if usage.kind != ReferenceKind::Name {
                    // Qualified references like `this.foo` are not affected by local name capture.
                    continue;
                }

                // `resolve_name_in_scope` only checks the current scope; `would_shadow` checks
                // parents. Together, they approximate name resolution for locals/parameters at
                // this site.
                let existing = db
                    .resolve_name_in_scope(usage_scope, new_name)
                    .or_else(|| db.would_shadow(usage_scope, new_name));

                if let Some(existing_symbol) = existing {
                    if existing_symbol != symbol {
                        conflicts.push(Conflict::ReferenceWillChangeResolution {
                            file: usage.file.clone(),
                            usage_range: usage.range,
                            name: new_name.to_string(),
                            existing_symbol,
                        });
                    }
                }
            }
        }
        Some(JavaSymbolKind::Type) => {
            if let Some(info) = db.type_symbol_info(symbol) {
                if info.is_top_level {
                    if let Some(existing) =
                        db.find_top_level_type_in_package(info.package.as_deref(), new_name)
                    {
                        if existing != symbol {
                            conflicts.push(Conflict::NameCollision {
                                file: def.file.clone(),
                                name: new_name.to_string(),
                                existing_symbol: existing,
                            });
                        }
                    }

                    if info.is_public {
                        if let Some((_, to)) =
                            type_file_rename_candidate(&def.file, &def.name, new_name)
                        {
                            if db.file_exists(&to) {
                                conflicts.push(Conflict::FileAlreadyExists { file: to });
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }

    for usage in refs {
        if !db.is_visible_from(symbol, &usage.file, new_name) {
            conflicts.push(Conflict::VisibilityLoss {
                file: usage.file.clone(),
                usage_range: usage.range,
                name: new_name.to_string(),
            });
        }
    }

    conflicts
}

#[derive(Debug, Clone)]
enum EnclosingTypeBody {
    Class(ast::ClassBody),
    Interface(ast::InterfaceBody),
    Enum(ast::EnumBody),
    Record(ast::RecordBody),
    Annotation(ast::AnnotationBody),
}

impl EnclosingTypeBody {
    fn syntax(&self) -> &nova_syntax::SyntaxNode {
        match self {
            EnclosingTypeBody::Class(body) => body.syntax(),
            EnclosingTypeBody::Interface(body) => body.syntax(),
            EnclosingTypeBody::Enum(body) => body.syntax(),
            EnclosingTypeBody::Record(body) => body.syntax(),
            EnclosingTypeBody::Annotation(body) => body.syntax(),
        }
    }
}

fn find_enclosing_type_body(node: &nova_syntax::SyntaxNode) -> Option<EnclosingTypeBody> {
    node.ancestors().find_map(|ancestor| {
        if let Some(class_decl) = ast::ClassDeclaration::cast(ancestor.clone()) {
            return class_decl.body().map(EnclosingTypeBody::Class);
        }
        if let Some(intf_decl) = ast::InterfaceDeclaration::cast(ancestor.clone()) {
            return intf_decl.body().map(EnclosingTypeBody::Interface);
        }
        if let Some(enum_decl) = ast::EnumDeclaration::cast(ancestor.clone()) {
            return enum_decl.body().map(EnclosingTypeBody::Enum);
        }
        if let Some(record_decl) = ast::RecordDeclaration::cast(ancestor.clone()) {
            return record_decl.body().map(EnclosingTypeBody::Record);
        }
        if let Some(annot_decl) = ast::AnnotationTypeDeclaration::cast(ancestor.clone()) {
            return annot_decl.body().map(EnclosingTypeBody::Annotation);
        }
        None
    })
}

#[derive(Debug, Clone)]
struct OverloadSig {
    arity: usize,
    param_types: Option<Vec<String>>,
}

fn method_overload_signatures(
    source: &str,
    type_body: &EnclosingTypeBody,
    name: &str,
) -> Vec<OverloadSig> {
    let mut out = Vec::new();
    for member in type_body
        .syntax()
        .children()
        .filter_map(ast::ClassMember::cast)
    {
        let ast::ClassMember::MethodDeclaration(method) = member else {
            continue;
        };
        let Some(name_tok) = method.name_token() else {
            continue;
        };
        if name_tok.text() != name {
            continue;
        }

        let (arity, param_types) = method_parameter_types(source, &method);
        let param_types = param_types.map(|types| {
            types
                .into_iter()
                .map(|ty| normalize_type_signature(&ty))
                .collect()
        });
        out.push(OverloadSig { arity, param_types });
    }
    out
}

fn method_overload_sets_overlap(old: &[OverloadSig], new: &[OverloadSig]) -> bool {
    for old_sig in old {
        for new_sig in new {
            if old_sig.arity != new_sig.arity {
                continue;
            }

            match (&old_sig.param_types, &new_sig.param_types) {
                (Some(a), Some(b)) => {
                    if a == b {
                        return true;
                    }
                }
                // Conservative fallback: if we can't recover parameter types for either overload,
                // treat name+arity as a collision so we don't generate uncompilable code.
                _ => return true,
            }
        }
    }
    false
}

fn method_parameter_types(
    source: &str,
    method: &ast::MethodDeclaration,
) -> (usize, Option<Vec<String>>) {
    let Some(param_list) = method.parameter_list() else {
        return (0, Some(Vec::new()));
    };

    let mut arity = 0usize;
    let mut types = Vec::new();
    let mut unknown = false;
    for param in param_list.parameters() {
        arity += 1;
        if unknown {
            continue;
        }
        match parameter_type_text(source, &param) {
            Some(ty) => types.push(ty),
            None => unknown = true,
        }
    }

    if unknown {
        (arity, None)
    } else {
        (arity, Some(types))
    }
}

fn parameter_type_text(source: &str, param: &ast::Parameter) -> Option<String> {
    let ty = param.ty()?;
    let ty_range = syntax_range(ty.syntax());
    let mut text = source.get(ty_range.start..ty_range.end)?.trim().to_string();

    // Varargs: include the ellipsis token (`...`) as part of the type text so we can distinguish
    // `foo(String...)` from `foo(String)`.
    let ellipsis_after_type = param
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::Ellipsis)
        .is_some_and(|tok| (u32::from(tok.text_range().start()) as usize) >= ty_range.end);
    if ellipsis_after_type && !text.ends_with("...") {
        text.push_str("...");
    }

    Some(text)
}

fn type_file_rename_candidate(
    file: &FileId,
    old_name: &str,
    new_name: &str,
) -> Option<(FileId, FileId)> {
    if new_name == old_name {
        return None;
    }
    // Only rename `Foo.java` -> `Bar.java` when the file name matches the public top-level type.
    let path = file.0.as_str();
    let (dir, base) = path.rsplit_once('/').unwrap_or(("", path));
    let expected = format!("{old_name}.java");
    if base != expected {
        return None;
    }
    let new_base = format!("{new_name}.java");
    let new_path = if dir.is_empty() {
        new_base
    } else {
        format!("{dir}/{new_base}")
    };
    if new_path == file.0 {
        return None;
    }
    Some((file.clone(), FileId::new(new_path)))
}

fn rename_field_with_accessors(
    db: &dyn RefactorDatabase,
    params: RenameParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let Some(def) = db.symbol_definition(params.symbol) else {
        // Keep behavior consistent with other rename operations: materialize will report the
        // unknown symbol.
        return Ok(materialize(
            db,
            [SemanticChange::Rename {
                symbol: params.symbol,
                new_name: params.new_name,
            }],
        )?);
    };
    let Some(scope) = db.symbol_scope(params.symbol) else {
        return Ok(materialize(
            db,
            [SemanticChange::Rename {
                symbol: params.symbol,
                new_name: params.new_name,
            }],
        )?);
    };

    let mut conflicts = check_rename_conflicts(db, params.symbol, &params.new_name);

    if let Some(existing) = db.resolve_field_in_scope(scope, &params.new_name) {
        if existing != params.symbol {
            conflicts.push(Conflict::NameCollision {
                file: def.file.clone(),
                name: params.new_name.clone(),
                existing_symbol: existing,
            });
        }
    }

    let Some(old_suffix) = java_bean_suffix(&def.name) else {
        let changes = vec![SemanticChange::Rename {
            symbol: params.symbol,
            new_name: params.new_name,
        }];
        return Ok(materialize(db, changes)?);
    };
    let Some(new_suffix) = java_bean_suffix(&params.new_name) else {
        let changes = vec![SemanticChange::Rename {
            symbol: params.symbol,
            new_name: params.new_name,
        }];
        return Ok(materialize(db, changes)?);
    };

    let accessors = [("get", 0usize), ("is", 0usize), ("set", 1usize)];

    let mut accessor_changes = Vec::new();

    for (prefix, arity) in accessors {
        let old_name = format!("{prefix}{old_suffix}");
        let new_name = format!("{prefix}{new_suffix}");

        for method in db.resolve_methods_in_scope(scope, &old_name) {
            let Some(sig) = db.method_signature(method) else {
                continue;
            };
            if sig.arity() != arity {
                continue;
            }

            // Collision check: new accessor name + same signature must not already exist in the
            // declaring type.
            for existing in db.resolve_methods_in_scope(scope, &new_name) {
                if existing == method {
                    continue;
                }
                if db.method_signature(existing).as_ref() == Some(&sig) {
                    conflicts.push(Conflict::NameCollision {
                        file: def.file.clone(),
                        name: new_name.clone(),
                        existing_symbol: existing,
                    });
                }
            }

            accessor_changes.push(SemanticChange::Rename {
                symbol: method,
                new_name: new_name.clone(),
            });
        }
    }

    if !conflicts.is_empty() {
        return Err(RefactorError::Conflicts(conflicts));
    }

    let mut changes = Vec::with_capacity(1 + accessor_changes.len());
    changes.push(SemanticChange::Rename {
        symbol: params.symbol,
        new_name: params.new_name,
    });
    changes.extend(accessor_changes);
    Ok(materialize(db, changes)?)
}

fn java_bean_suffix(field_name: &str) -> Option<String> {
    let mut chars = field_name.chars();
    let first = chars.next()?;
    let mut out = String::new();
    for c in first.to_uppercase() {
        out.push(c);
    }
    out.push_str(chars.as_str());
    Some(out)
}

pub struct ExtractVariableParams {
    pub file: FileId,
    pub expr_range: TextRange,
    pub name: String,
    pub use_var: bool,
    /// When enabled, attempt to replace other equivalent expressions in the same statement-list
    /// scope as the extracted declaration.
    ///
    /// Safety note (switch statements): we intentionally restrict replacement to the current case
    /// group / rule body. Control flow may enter a `switch` at any label, so replacing across
    /// labels could introduce uses of the extracted local on paths that skip its declaration.
    pub replace_all: bool,
}

pub fn extract_variable(
    db: &dyn RefactorDatabase,
    params: ExtractVariableParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let name = crate::java::validate_java_identifier(&params.name).map_err(|err| {
        let trimmed = params.name.trim();
        let display_name = if trimmed.is_empty() {
            "<empty>".to_string()
        } else {
            trimmed.to_string()
        };
        RefactorError::InvalidIdentifier {
            name: display_name,
            reason: err.reason(),
        }
    })?;

    let text = db
        .file_text(&params.file)
        .ok_or_else(|| RefactorError::UnknownFile(params.file.clone()))?;

    if params.expr_range.start > params.expr_range.end {
        return Err(RefactorError::InvalidSelection);
    }

    if params.expr_range.end > text.len() {
        return Err(RefactorError::Edit(crate::edit::EditError::OutOfBounds {
            file: params.file.clone(),
            range: params.expr_range,
            len: text.len(),
        }));
    }

    let selection = trim_range(text, params.expr_range);
    if selection.len() == 0 {
        return Err(RefactorError::InvalidSelection);
    }
    let parsed = parse_java(text);
    if !parsed.errors.is_empty() {
        return Err(RefactorError::ParseError);
    }

    let root = parsed.syntax();
    let expr =
        find_expression(text, root.clone(), selection).ok_or(RefactorError::InvalidSelection)?;

    // Extracting expressions from `assert` statements is unsafe: Java assertions may be disabled
    // at runtime, and hoisting the expression into a preceding local variable would force it to be
    // evaluated unconditionally.
    if expr
        .syntax()
        .ancestors()
        .any(|node| ast::AssertStatement::cast(node).is_some())
    {
        return Err(RefactorError::ExtractNotSupportedInAssert);
    }

    // Java pattern matching for `instanceof` introduces pattern variables whose scope is tied to
    // the conditional expression. Extracting an `instanceof <pattern>` would remove the binding
    // and either break compilation (pattern variable no longer in scope) or change semantics.
    if let ast::Expression::InstanceofExpression(instanceof) = &expr {
        if instanceof.pattern().is_some() {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract `instanceof` pattern matching expression",
            });
        }
    }

    // Be conservative: reject extracting any expression subtree that contains patterns (nested
    // patterns, switch patterns, etc) so we never let pattern variables escape their scope.
    if expr
        .syntax()
        .descendants()
        .any(|node| node.kind() == SyntaxKind::Pattern)
    {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract expression containing pattern variables",
        });
    }

    // `extract_variable` inserts the new declaration before the enclosing statement. For
    // expression-bodied lambdas (`x -> x + 1`), there is no statement inside the lambda body,
    // so extraction would hoist the declaration outside the lambda (breaking compilation when
    // referencing parameters and/or changing evaluation timing). Reject this case.
    let in_expression_bodied_lambda = expr
        .syntax()
        .ancestors()
        .filter_map(ast::LambdaBody::cast)
        .any(|body| body.expression().is_some());
    if in_expression_bodied_lambda {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract from expression-bodied lambda body",
        });
    }

    // Expressions inside `try ( ... )` resource specifications have special semantics: the
    // AutoCloseable(s) created/used there are closed automatically at the end of the try block.
    // Naively extracting such expressions to a normal local variable declared before the `try`
    // can change resource lifetime/closing behavior. Until we implement a semantics-preserving
    // strategy (e.g. rewriting to `try (var tmp = <expr>)` where legal), refuse extraction here.
    if expr.syntax().ancestors().any(|node| {
        ast::ResourceSpecification::cast(node.clone()).is_some()
            || ast::Resource::cast(node).is_some()
    }) {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract from try-with-resources resource specification",
        });
    }

    // Reject extracting from switch labels/guards. Labels must remain constant expressions and
    // switch labels/guards cannot contain statement lists where we could insert a new declaration.
    if expr.syntax().ancestors().any(|node| {
        ast::SwitchLabel::cast(node.clone()).is_some()
            || ast::CaseLabelElement::cast(node.clone()).is_some()
            || ast::Guard::cast(node).is_some()
    }) {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract from switch labels",
        });
    }

    // Reject non-block switch-expression arrow rule bodies (`case ... -> <expr>` / `case ... -> <stmt>`).
    //
    // Extract Variable inserts a new local declaration statement before the enclosing statement.
    // For non-block switch *expression* rule bodies, doing so would either hoist evaluation out of
    // the selected case arm or require rewriting the rule into a `{ ... }` block (not implemented).
    if let Some(rule) = expr.syntax().ancestors().find_map(ast::SwitchRule::cast) {
        let Some(body) = rule.body() else {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract from malformed switch rule",
            });
        };
        if !matches!(body, ast::SwitchRuleBody::Block(_)) {
            // Only guard when the selection is inside the rule body (not the labels/guard).
            let body_range = syntax_range(body.syntax());
            if body_range.start <= selection.start && selection.end <= body_range.end {
                let container = rule.syntax().ancestors().skip(1).find_map(|node| {
                    if ast::SwitchExpression::cast(node.clone()).is_some() {
                        Some(true)
                    } else if ast::SwitchStatement::cast(node).is_some() {
                        Some(false)
                    } else {
                        None
                    }
                });
                if container == Some(true) {
                    return Err(RefactorError::ExtractNotSupported {
                        reason: "cannot extract from non-block switch rule body",
                    });
                }
            }
        }
    }

    if let Some(reason) = constant_expression_only_context_reason(&expr) {
        return Err(RefactorError::ExtractNotSupported { reason });
    }

    // Some expression node ranges in the AST can include leading/trailing trivia (whitespace and
    // comments). Treat the *token* span as the extracted expression text (so we don't move
    // trailing comments into the new declaration), but use a slightly wider range for replacement
    // so we keep legacy behavior of trimming stray whitespace when no trailing comment exists.
    let expr_token_range = trimmed_syntax_range(expr.syntax());
    let expr_range = expr_replacement_range(expr.syntax());
    let expr_text = text
        .get(expr_token_range.start..expr_token_range.end)
        .ok_or(RefactorError::InvalidSelection)?
        .to_string();

    if let Some(reason) = extract_variable_crosses_execution_boundary(&expr) {
        return Err(RefactorError::ExtractNotSupported { reason });
    }

    if params.use_var && var_initializer_requires_explicit_type(&expr) {
        return Err(RefactorError::VarNotAllowedForInitializer);
    }

    // Extracting a side-effectful expression into a new statement can change evaluation order or
    // conditionality (e.g. when the expression appears under `?:`, `&&`, etc).
    //
    // When extracting to `var`, be conservative and reject side-effectful initializers to avoid
    // producing surprising code (and to avoid `void`-typed method invocations that `var` cannot
    // represent).
    if params.use_var && has_side_effects(expr.syntax()) {
        return Err(RefactorError::ExtractSideEffects);
    }
    let stmt = expr
        .syntax()
        .ancestors()
        .find_map(ast::Statement::cast)
        .ok_or(RefactorError::InvalidSelection)?;

    // Java requires an explicit constructor invocation (`this(...)` / `super(...)`) to be the
    // first statement in a constructor body. Extracting a variable would insert a new statement
    // before it, producing uncompilable code.
    if matches!(stmt, ast::Statement::ExplicitConstructorInvocation(_)) {
        return Err(RefactorError::ExtractNotSupported {
            reason:
                "cannot extract from explicit constructor invocation (`this(...)` / `super(...)`)",
        });
    }
    reject_extract_variable_written_deps_in_same_statement(&expr, &stmt)?;
    reject_unsafe_extract_variable_context(&expr, &stmt)?;
    reject_extract_variable_eval_order_guard(text, selection, &expr, &stmt)?;

    // Be conservative: extracting from loop conditions changes evaluation frequency.
    match stmt {
        ast::Statement::WhileStatement(_) => {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract from while condition",
            })
        }
        ast::Statement::DoWhileStatement(_) => {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract from do-while condition",
            })
        }
        ast::Statement::ForStatement(_) => {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract from for statement header",
            })
        }
        _ => {}
    }

    let stmt_range = syntax_range(stmt.syntax());
    if let Some(parent) = stmt.syntax().parent() {
        // Reject labeled statements (`label:\n  stmt;`) where inserting at the start of the line
        // would "steal" the label and change control flow.
        if ast::LabeledStatement::cast(parent.clone()).is_some() {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract into a labeled statement body",
            });
        }

        // Reject switch arrow rules with a single statement body:
        // `case 1 -> stmt;`
        // Inserting a new statement would require rewriting the body to a `{ ... }` block.
        if ast::SwitchRule::cast(parent.clone()).is_some()
            && !matches!(stmt, ast::Statement::Block(_))
        {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract into a single-statement switch rule body without braces",
            });
        }

        // Reject inserting into single-statement control structure bodies without braces. In those
        // contexts we'd have to introduce a `{ ... }` block to preserve control flow.
        if !matches!(stmt, ast::Statement::Block(_))
            && (ast::IfStatement::cast(parent.clone()).is_some()
                || ast::WhileStatement::cast(parent.clone()).is_some()
                || ast::DoWhileStatement::cast(parent.clone()).is_some()
                || ast::ForStatement::cast(parent.clone()).is_some()
                || ast::SynchronizedStatement::cast(parent.clone()).is_some())
        {
            return Err(RefactorError::ExtractNotSupported {
                reason:
                    "cannot extract into a single-statement control structure body without braces",
            });
        }
    }

    let line_start = line_start(text, stmt_range.start);
    let prefix = &text[line_start..stmt_range.start];

    // Default insertion strategy inserts a new declaration at the start of the enclosing
    // statement's line. For statements that start mid-line (e.g. `case 1: foo();`), that would
    // be invalid or change semantics, so we reject those.
    //
    // Exception: if the statement begins after a `{` on the same line (e.g. `label: { foo(); }`),
    // we can safely insert the declaration in-line right before the statement.
    let (insert_pos, indent, newline) = if prefix.chars().all(|c| c.is_whitespace()) {
        (
            line_start,
            current_indent(text, line_start),
            NewlineStyle::detect(text).as_str(),
        )
    } else {
        let last_non_ws = prefix.chars().rev().find(|c| !c.is_whitespace());
        if last_non_ws != Some('{') {
            return Err(RefactorError::ExtractNotSupported {
                reason: "cannot extract when the enclosing statement starts mid-line",
            });
        }
        (stmt_range.start, String::new(), " ")
    };

    let mut replacement_ranges = if params.replace_all {
        find_replace_all_occurrences_same_execution_context(text, root.clone(), &stmt, &expr_text)
    } else {
        vec![expr_range]
    };
    if params.replace_all && !replacement_ranges.iter().any(|r| *r == expr_range) {
        replacement_ranges.push(expr_range);
        replacement_ranges.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));
        replacement_ranges.dedup();
    }

    check_extract_variable_name_conflicts(&params.file, &stmt, insert_pos, &name)?;
    check_extract_variable_field_shadowing(&stmt, &params.file, &name, &replacement_ranges)?;

    let ty = if params.use_var {
        "var".to_string()
    } else {
        let mut parser_ty = infer_expr_type(&expr);
        if parser_ty.contains("<>") {
            parser_ty = parser_ty.replace("<>", "");
        }

        let typeck_ty = best_type_at_range_display(db, &params.file, text, expr_token_range);

        // For explicit-typed extraction we must be confident about the type. If we don't have
        // type-checker type information and our parser-only inference fell back to the generic
        // "Object" type, we may be unable to distinguish between a value-returning expression and a
        // `void`-typed method invocation, or determine a required target type for lambdas/method
        // references. In those cases, reject the refactoring rather than guessing.
        //
        // Note: if the type-checker *does* report `Object`, treat it as a real inferred type and
        // allow it.
        if typeck_ty.is_none() && parser_ty == "Object" {
            fn expr_might_be_void(expr: &ast::Expression) -> bool {
                match expr {
                    ast::Expression::MethodCallExpression(_) => true,
                    ast::Expression::ParenthesizedExpression(p) => p
                        .expression()
                        .is_some_and(|e| expr_might_be_void(&e)),
                    _ => false,
                }
            }

            fn requires_target_type(expr: &ast::Expression) -> bool {
                match expr {
                    ast::Expression::ParenthesizedExpression(p) => p
                        .expression()
                        .is_some_and(|e| requires_target_type(&e)),
                    ast::Expression::LambdaExpression(_)
                    | ast::Expression::MethodReferenceExpression(_)
                    | ast::Expression::ConstructorReferenceExpression(_)
                    | ast::Expression::ArrayInitializer(_) => true,
                    _ => false,
                }
            }

            if expr_might_be_void(&expr) || requires_target_type(&expr) {
                return Err(RefactorError::TypeInferenceFailed);
            }
        }

        // When emitting an explicit type (instead of `var`), prefer parser-inferred names when
        // they are already meaningful and avoid redundant *package* qualification (`Foo` instead
        // of `pkg.Foo`). Keep enclosing-type qualifiers like `Outer.Foo` since dropping them can
        // change meaning.
        match typeck_ty {
            Some(typeck_ty) if typeck_ty != "null" => {
                if parser_ty == "Object" {
                    if typeck_ty != "Object" {
                        typeck_ty
                    } else {
                        parser_ty
                    }
                } else if typeck_ty == "Object" {
                    parser_ty
                } else {
                    let typeck_has_type_args = typeck_ty.contains('<');
                    let parser_has_type_args = parser_ty.contains('<');
                    let typeck_has_arrays = typeck_ty.contains("[]");
                    let parser_has_arrays = parser_ty.contains("[]");

                    // Prefer typeck when it adds generics or array dimensions that parser didn't
                    // capture (e.g. diamond inference).
                    if (typeck_has_type_args && !parser_has_type_args)
                        || (typeck_has_arrays && !parser_has_arrays)
                    {
                        typeck_ty
                    } else if typeck_ty_is_qualified_version_of(&parser_ty, &typeck_ty) {
                        // Avoid redundant package qualification like `java.util.List` when `List`
                        // is already a useful type name in this context.
                        parser_ty
                    } else {
                        typeck_ty
                    }
                }
            }
            _ => parser_ty,
        }
    };

    // Special-case: when extracting the whole expression of an expression statement, the usual
    // strategy (insert declaration before the statement + replace the selected expression with the
    // variable name) would leave a bare identifier statement (`name;`), which is not valid Java.
    //
    // In this case, replace the entire expression statement with a local variable declaration.
    if let ast::Statement::ExpressionStatement(expr_stmt) = &stmt {
        if let Some(stmt_expr) = expr_stmt.expression() {
            let stmt_expr_range = trimmed_syntax_range(stmt_expr.syntax());
            let stmt_expr_range_ws = trim_range(text, syntax_range(stmt_expr.syntax()));
            if (stmt_expr_range.start == selection.start && stmt_expr_range.end == selection.end)
                || (stmt_expr_range_ws.start == selection.start
                    && stmt_expr_range_ws.end == selection.end)
            {
                let stmt_range = syntax_range(expr_stmt.syntax());
                let prefix = &text[stmt_range.start..expr_token_range.start];
                let suffix = &text[expr_range.end..stmt_range.end];
                let replacement = format!("{prefix}{ty} {name} = {expr_text}{suffix}");

                let mut edit = WorkspaceEdit::new(vec![TextEdit::replace(
                    params.file.clone(),
                    stmt_range,
                    replacement,
                )]);
                edit.normalize()?;
                return Ok(edit);
            }
        }
    }

    // Side-effectful expressions are tricky:
    // - Evaluating them once and replacing multiple occurrences is never safe.
    // - Even evaluating them once can be unsafe when extracting to `var` because target typing for
    //   method calls / diamond inference can change without an explicit type.
    let expr_has_side_effects = has_side_effects(expr.syntax());
    if expr_has_side_effects {
        if params.use_var {
            return Err(RefactorError::ExtractSideEffects);
        }
        if replacement_ranges.iter().any(|r| *r != expr_range) {
            return Err(RefactorError::ExtractSideEffects);
        }
    }

    let file_newline = NewlineStyle::detect(text).as_str();

    // Special-case: extracting inside a multi-declarator local variable declaration needs to
    // preserve scoping and initializer evaluation order. Naively inserting the extracted binding
    // before the whole statement can be invalid (later declarators can reference earlier ones) and
    // can also reorder side effects relative to earlier declarators.
    //
    // Example:
    //   int a = 1, b = a + 2;
    //
    // Desired:
    //   int a = 1;
    //   var tmp = a + 2;
    //   int b = tmp;
    if let ast::Statement::LocalVariableDeclarationStatement(local) = &stmt {
        if let Some(replacement) = rewrite_multi_declarator_local_variable_declaration(
            text,
            local,
            stmt_range,
            expr_range,
            &expr_text,
            &name,
            &ty,
            &indent,
            file_newline,
        ) {
            let mut edit = WorkspaceEdit::new(vec![TextEdit::replace(
                params.file.clone(),
                stmt_range,
                replacement,
            )]);
            edit.normalize()?;
            return Ok(edit);
        }
    }
    let decl = format!("{indent}{ty} {} = {expr_text};{newline}", &name);

    let mut edits = Vec::with_capacity(1 + replacement_ranges.len());
    edits.push(TextEdit::insert(params.file.clone(), insert_pos, decl));
    for range in replacement_ranges {
        edits.push(TextEdit::replace(params.file.clone(), range, name.clone()));
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

const EXTRACT_VARIABLE_WRITES_BEFORE_SELECTION_REASON: &str =
    "cannot extract expression that depends on a variable written earlier in the same statement";

fn reject_extract_variable_written_deps_in_same_statement(
    expr: &ast::Expression,
    stmt: &ast::Statement,
) -> Result<(), RefactorError> {
    let deps = collect_simple_name_dependencies(expr);
    if deps.is_empty() {
        return Ok(());
    }

    let expr_range = syntax_range(expr.syntax());
    let selection_start = expr_range.start;

    for node in stmt.syntax().descendants() {
        if let Some(assignment) = ast::AssignmentExpression::cast(node.clone()) {
            let Some(lhs) = assignment.lhs() else {
                continue;
            };
            let Some(lhs_name) = simple_name_from_expression(&lhs) else {
                continue;
            };

            // Only consider assignments that syntactically occur before the selection.
            let lhs_range = syntax_range(lhs.syntax());
            if lhs_range.end > selection_start {
                continue;
            }

            // Special-case: `x = ...` writes to `x` *after* evaluating the RHS. When the selection
            // is inside the RHS, treat this as a write-after-selection (safe w.r.t. this guard).
            if let Some(rhs) = assignment.rhs() {
                let rhs_range = syntax_range(rhs.syntax());
                if contains_range(rhs_range, expr_range) {
                    continue;
                }
            }

            if deps.contains(&lhs_name) {
                return Err(RefactorError::ExtractNotSupported {
                    reason: EXTRACT_VARIABLE_WRITES_BEFORE_SELECTION_REASON,
                });
            }
        }

        if let Some(unary) = ast::UnaryExpression::cast(node) {
            if !unary_is_inc_or_dec(&unary) {
                continue;
            }

            let Some(operand) = unary.operand() else {
                continue;
            };
            let Some(name) = simple_name_from_expression(&operand) else {
                continue;
            };

            let unary_range = syntax_range(unary.syntax());
            if unary_range.end > selection_start {
                continue;
            }

            if deps.contains(&name) {
                return Err(RefactorError::ExtractNotSupported {
                    reason: EXTRACT_VARIABLE_WRITES_BEFORE_SELECTION_REASON,
                });
            }
        }
    }

    Ok(())
}

fn collect_simple_name_dependencies(expr: &ast::Expression) -> HashSet<String> {
    let mut out = HashSet::new();

    for node in expr.syntax().descendants() {
        let Some(name_expr) = ast::NameExpression::cast(node.clone()) else {
            continue;
        };

        // Avoid obvious false positives: for `foo(x)` the callee `foo` is a method name, not a
        // variable read.
        //
        // However, `obj.foo(x)` *does* read `obj` before evaluating arguments, so when the callee
        // is qualified (`obj.foo`) we treat the leftmost segment as a dependency.
        if let Some(parent) = node.parent() {
            if let Some(call) = ast::MethodCallExpression::cast(parent.clone()) {
                if call.callee().is_some_and(|callee| callee.syntax() == &node) {
                    if !name_expression_has_dot(&name_expr) {
                        continue;
                    }
                }
            }
        }

        if let Some(name) = leftmost_segment_from_name_expression(&name_expr) {
            out.insert(name);
        }
    }

    // `this.x` / `super.x` are modeled as `FieldAccessExpression` nodes rather than `NameExpression`
    // nodes, but they still read `x` (a field) at runtime. Include the field name segment as a
    // dependency so we can detect `x = 1, this.x` style hazards.
    for node in expr.syntax().descendants() {
        let Some(field_access) = ast::FieldAccessExpression::cast(node) else {
            continue;
        };

        // If this field access is being used as a method callee (`this.foo(...)` / `super.foo(...)`),
        // its identifier is a method name, not a variable/field read.
        if let Some(parent) = field_access.syntax().parent() {
            if let Some(call) = ast::MethodCallExpression::cast(parent) {
                if call
                    .callee()
                    .is_some_and(|callee| callee.syntax() == field_access.syntax())
                {
                    continue;
                }
            }
        }

        let Some(receiver) = field_access.expression() else {
            continue;
        };

        if !matches!(
            receiver,
            ast::Expression::ThisExpression(_) | ast::Expression::SuperExpression(_)
        ) {
            continue;
        }

        let Some(name_tok) = field_access.name_token() else {
            continue;
        };

        out.insert(name_tok.text().to_string());
    }

    out
}

fn simple_name_from_expression(expr: &ast::Expression) -> Option<String> {
    match expr {
        ast::Expression::NameExpression(name_expr) => simple_name_from_name_expression(name_expr),
        _ => None,
    }
}

fn leftmost_segment_from_name_expression(expr: &ast::NameExpression) -> Option<String> {
    expr.syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| !tok.kind().is_trivia())
        .find(|tok| tok.kind().is_identifier_like())
        .map(|tok| tok.text().to_string())
}

fn name_expression_has_dot(expr: &ast::NameExpression) -> bool {
    expr.syntax()
        .descendants_with_tokens()
        .any(|el| el.kind() == SyntaxKind::Dot)
}

fn simple_name_from_name_expression(expr: &ast::NameExpression) -> Option<String> {
    let mut name: Option<String> = None;
    let mut has_dot = false;

    for tok in expr
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| !tok.kind().is_trivia())
    {
        if tok.kind() == SyntaxKind::Dot {
            has_dot = true;
        }

        if tok.kind().is_identifier_like() {
            if name.is_some() {
                return None;
            }
            name = Some(tok.text().to_string());
        }
    }

    if has_dot {
        return None;
    }

    name
}
pub struct InlineVariableParams {
    pub symbol: SymbolId,
    pub inline_all: bool,
    /// When `inline_all` is false, identifies which usage should be inlined.
    ///
    /// This must match the byte range of a reference returned by `find_references(symbol)`.
    pub usage_range: Option<TextRange>,
}

fn contains_unknown_name_expression(
    db: &dyn RefactorDatabase,
    file: &FileId,
    source: &str,
    name: &str,
    symbol: SymbolId,
    known_refs: &[crate::semantic::Reference],
) -> bool {
    // Some RefactorDatabase implementations may be able to return semantic references for a symbol
    // even when `symbol_at` span tracking is incomplete. Treat any same-name occurrence whose
    // identifier token range exactly matches a known reference range as "known" without requiring
    // an additional `symbol_at` resolution step.
    let known_ranges: HashSet<TextRange> = known_refs
        .iter()
        .filter(|r| &r.file == file)
        .map(|r| r.range)
        .collect();

    let parsed = parse_java(source);
    let root = parsed.syntax();

    for name_expr in root.descendants().filter_map(ast::NameExpression::cast) {
        // Avoid false positives for unqualified method calls like `foo(x)`: the callee `foo` is a
        // method name, not a variable reference.
        //
        // This matters in unindexed contexts (for example anonymous class bodies) where
        // `symbol_at` may return `None` for unrelated identifiers.
        //
        // NOTE: Do *not* skip qualified callees like `a.b(x)` because the leftmost segment `a`
        // can still be a variable reference.
        if let Some(parent) = name_expr.syntax().parent() {
            if let Some(call) = ast::MethodCallExpression::cast(parent.clone()) {
                if call
                    .callee()
                    .is_some_and(|callee| callee.syntax() == name_expr.syntax())
                    && simple_name_from_name_expression(&name_expr).is_some()
                {
                    continue;
                }
            }
        }

        // `NameExpression` nodes cover both simple names (`a`) and qualified names (`a.b.c`).
        //
        // For local variables, any reference must start with the variable name as the leftmost
        // identifier segment.
        let Some(first_ident) = name_expr
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| !tok.kind().is_trivia())
            .find(|tok| tok.kind().is_identifier_like())
        else {
            continue;
        };

        if first_ident.text() != name {
            continue;
        }

        let ident_start = u32::from(first_ident.text_range().start()) as usize;
        let ident_end = u32::from(first_ident.text_range().end()) as usize;
        let ident_range = TextRange::new(ident_start, ident_end);

        if known_ranges.contains(&ident_range) {
            continue;
        }

        match db.symbol_at(file, ident_start) {
            Some(resolved) if resolved == symbol => return true,
            Some(_) => {}
            None => return true,
        }
    }

    false
}

pub fn inline_variable(
    db: &dyn RefactorDatabase,
    params: InlineVariableParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let def = db
        .symbol_definition(params.symbol)
        .ok_or(RefactorError::InlineNotSupported)?;

    if inline_variable_has_writes(db, params.symbol, &def)? {
        return Err(RefactorError::InlineNotSupported);
    }

    let text = db
        .file_text(&def.file)
        .ok_or_else(|| RefactorError::UnknownFile(def.file.clone()))?;

    let parsed = parse_java(text);
    // `parse_java` may produce recoverable errors even for source we can still refactor correctly
    // (e.g. some switch/case layouts). Inline Variable only relies on a small subset of the syntax
    // tree (the declaration statement and usage tokens), so avoid failing fast on parse errors;
    // downstream lookups will return `InlineNotSupported` if the required structure cannot be
    // recovered.
    let root = parsed.syntax();

    // Variables declared in `for` headers or as try-with-resources bindings have special lifetime
    // semantics (loop-init evaluation / resource closing). Conservatively refuse to inline them.
    if let Some(declarator) = root
        .descendants()
        .filter_map(ast::VariableDeclarator::cast)
        .find(|decl| {
            let Some(tok) = decl.name_token() else {
                return false;
            };
            let range = syntax_token_range(&tok);
            range == def.name_range
                || (range.start <= def.name_range.start && def.name_range.end <= range.end)
                || db
                    .symbol_at(&def.file, range.start)
                    .is_some_and(|sym| sym == params.symbol)
        })
    {
        if declarator
            .syntax()
            .ancestors()
            .any(|n| matches!(n.kind(), SyntaxKind::ForHeader | SyntaxKind::ForInit))
        {
            return Err(RefactorError::InlineNotSupported);
        }

        if declarator.syntax().ancestors().any(|n| {
            matches!(
                n.kind(),
                SyntaxKind::ResourceSpecification | SyntaxKind::Resource
            )
        }) {
            return Err(RefactorError::InlineNotSupported);
        }
    }

    let decl = find_local_variable_declaration(db, &def.file, params.symbol, &root, def.name_range)
        .ok_or(RefactorError::InlineNotSupported)?;

    let decl_stmt = decl.statement.clone();
    let init_expr = decl.initializer;
    // Array initializers (`int[] xs = {1,2};`) are not expressions in Java; they cannot be inlined
    // at arbitrary use sites.
    if matches!(init_expr, ast::Expression::ArrayInitializer(_)) {
        return Err(RefactorError::InlineNotSupported);
    }
    let init_range = syntax_range(init_expr.syntax());
    let init_text = text
        .get(init_range.start..init_range.end)
        .unwrap_or_default()
        .trim();
    if init_text.is_empty() {
        return Err(RefactorError::InlineNotSupported);
    }

    let init_has_side_effects = has_side_effects(init_expr.syntax());
    let init_is_order_sensitive = initializer_is_order_sensitive(init_expr.syntax());
    let init_replacement = parenthesize_initializer(init_text, &init_expr);

    let mut all_refs = db.find_references(params.symbol);
    if all_refs.is_empty() && params.inline_all {
        // Best-effort fallback: if semantic reference collection failed (e.g. due to incomplete
        // scoping for certain constructs like switch case groups), try to find identifier tokens
        // by name in the enclosing block.
        //
        // This keeps `inline_variable` usable in common cases even when the semantic model is
        // incomplete. We intentionally scope the search to the nearest enclosing `{ ... }` block
        // and only consider occurrences after the declaration statement to avoid accidentally
        // capturing unrelated identifiers earlier in the method.
        let search_root = decl_stmt
            .syntax()
            .ancestors()
            .find_map(ast::Block::cast)
            .map(|b| b.syntax().clone())
            .unwrap_or_else(|| root.clone());

        let decl_end = decl.statement_range.end;
        for tok in search_root
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
        {
            if tok.kind() != SyntaxKind::Identifier {
                continue;
            }
            if tok.text() != def.name.as_str() {
                continue;
            }
            let range = syntax_token_range(&tok);
            if range == def.name_range {
                continue;
            }
            if range.start < decl_end {
                continue;
            }
            all_refs.push(crate::semantic::Reference {
                file: def.file.clone(),
                range,
                scope: None,
                kind: ReferenceKind::Name,
            });
        }
    }

    let targets = if params.inline_all {
        all_refs.clone()
    } else {
        let Some(usage_range) = params.usage_range else {
            return Err(RefactorError::InlineNoUsageAtCursor);
        };
        let Some(reference) = all_refs
            .iter()
            .find(|r| r.range.start == usage_range.start && r.range.end == usage_range.end)
            .cloned()
        else {
            return Err(RefactorError::InlineNoUsageAtCursor);
        };
        vec![reference]
    };

    if targets.is_empty() {
        return Err(RefactorError::InlineNotSupported);
    }

    ensure_inline_variable_dependencies_not_shadowed(
        db, &parsed, &def.file, text, init_range, &init_expr, &targets,
    )?;

    // Reject inlining inside `assert` statements: Java assertions may be disabled at runtime.
    //
    // Inlining a local into an `assert` expression can change semantics by making the initializer:
    // - conditional on assertions being enabled (when the declaration is removed), and/or
    // - evaluated multiple times (when the declaration remains).
    {
        fn usage_is_within_assert_statement(
            db: &dyn RefactorDatabase,
            cache: &mut HashMap<FileId, nova_syntax::SyntaxNode>,
            file: &FileId,
            token_range: TextRange,
        ) -> Result<bool, RefactorError> {
            let root = if let Some(root) = cache.get(file) {
                root.clone()
            } else {
                let text = db
                    .file_text(file)
                    .ok_or_else(|| RefactorError::UnknownFile(file.clone()))?;
                let parsed = parse_java(text);
                let root = parsed.syntax();
                cache.insert(file.clone(), root.clone());
                root
            };

            let Some(tok) = root
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|tok| !tok.kind().is_trivia())
                .find(|tok| {
                    let range = syntax_token_range(tok);
                    range.start <= token_range.start && token_range.start < range.end
                })
            else {
                return Err(RefactorError::InlineNotSupported);
            };

            let Some(parent) = tok.parent() else {
                return Err(RefactorError::InlineNotSupported);
            };

            Ok(parent
                .ancestors()
                .any(|node| ast::AssertStatement::cast(node).is_some()))
        }

        let mut cache: HashMap<FileId, nova_syntax::SyntaxNode> = HashMap::new();
        cache.insert(def.file.clone(), root.clone());
        for usage in &targets {
            if usage_is_within_assert_statement(db, &mut cache, &usage.file, usage.range)? {
                return Err(RefactorError::InlineNotSupportedInAssert);
            }
        }
    }

    // Reject inlining across lambda execution-context boundaries. Inlining a local into a lambda
    // body (or out of it) can change evaluation timing and captured-variable semantics.
    //
    // Example:
    // ```
    // int x = 1;
    // int a = x;
    // Runnable r = () -> System.out.println(a);
    // x = 2;
    // r.run(); // prints 1
    // ```
    //
    // Inlining `a` inside the lambda would become `() -> println(x)`, printing 2 instead.
    {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct LambdaContext {
            file: FileId,
            range: TextRange,
        }

        fn lambda_context_at(
            db: &dyn RefactorDatabase,
            cache: &mut HashMap<FileId, nova_syntax::SyntaxNode>,
            file: &FileId,
            token_range: TextRange,
        ) -> Result<Option<LambdaContext>, RefactorError> {
            let root = if let Some(root) = cache.get(file) {
                root.clone()
            } else {
                let text = db
                    .file_text(file)
                    .ok_or_else(|| RefactorError::UnknownFile(file.clone()))?;
                let parsed = parse_java(text);
                let root = parsed.syntax();
                cache.insert(file.clone(), root.clone());
                root
            };

            // Some syntax/HIR layers may give us a range that is slightly larger than the raw
            // identifier token. Be tolerant and accept any token that overlaps the provided range.
            let Some(tok) = root
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|tok| !tok.kind().is_trivia())
                .find(|tok| ranges_overlap(syntax_token_range(tok), token_range))
            else {
                return Err(RefactorError::InlineNotSupported);
            };

            let Some(parent) = tok.parent() else {
                return Err(RefactorError::InlineNotSupported);
            };

            // `try (r) { ... }` resource specifications only allow effectively-final local variables;
            // they are not general expression positions. Inlining a variable here would produce
            // invalid Java like `try (new Foo()) { ... }` and/or change resource lifetime semantics.
            if parent.ancestors().any(|node| {
                ast::ResourceSpecification::cast(node.clone()).is_some()
                    || ast::Resource::cast(node).is_some()
            }) {
                return Err(RefactorError::InlineNotSupported);
            }

            let Some(lambda) = parent.ancestors().find_map(ast::LambdaExpression::cast) else {
                return Ok(None);
            };

            Ok(Some(LambdaContext {
                file: file.clone(),
                range: syntax_range(lambda.syntax()),
            }))
        }

        let mut cache: HashMap<FileId, nova_syntax::SyntaxNode> = HashMap::new();
        cache.insert(def.file.clone(), root.clone());

        let decl_ctx = lambda_context_at(db, &mut cache, &def.file, def.name_range)?;
        for usage in &targets {
            let usage_ctx = lambda_context_at(db, &mut cache, &usage.file, usage.range)?;
            if decl_ctx != usage_ctx {
                return Err(if init_has_side_effects {
                    RefactorError::InlineSideEffects
                } else {
                    RefactorError::InlineNotSupported
                });
            }
        }
    }

    // Reject inlining across nested type boundaries (anonymous/local/inner classes). Inlining a
    // local into a nested type body (or out of it) can change evaluation timing and captured-value
    // semantics, even when the initializer has no side effects.
    //
    // Example:
    // ```
    // class C {
    //   int foo = 1;
    //   void m() {
    //     int a = foo;
    //     Runnable r = new Runnable() { public void run() { System.out.println(a); } };
    //     foo = 2;
    //     r.run(); // prints 1
    //   }
    // }
    // ```
    //
    // Inlining `a` inside the anonymous class would become `... println(foo)`, printing 2 instead.
    {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct TypeContext {
            file: FileId,
            range: TextRange,
        }

        fn type_context_at(
            db: &dyn RefactorDatabase,
            cache: &mut HashMap<FileId, nova_syntax::SyntaxNode>,
            file: &FileId,
            token_range: TextRange,
        ) -> Result<Option<TypeContext>, RefactorError> {
            let root = if let Some(root) = cache.get(file) {
                root.clone()
            } else {
                let text = db
                    .file_text(file)
                    .ok_or_else(|| RefactorError::UnknownFile(file.clone()))?;
                let parsed = parse_java(text);
                let root = parsed.syntax();
                cache.insert(file.clone(), root.clone());
                root
            };

            let Some(tok) = root
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|tok| !tok.kind().is_trivia())
                .find(|tok| {
                    let range = syntax_token_range(tok);
                    range.start <= token_range.start && token_range.start < range.end
                })
            else {
                return Err(RefactorError::InlineNotSupported);
            };

            let Some(parent) = tok.parent() else {
                return Err(RefactorError::InlineNotSupported);
            };

            for anc in parent.ancestors() {
                if let Some(decl) = ast::ClassDeclaration::cast(anc.clone()) {
                    return Ok(Some(TypeContext {
                        file: file.clone(),
                        range: syntax_range(decl.syntax()),
                    }));
                }
                if let Some(decl) = ast::InterfaceDeclaration::cast(anc.clone()) {
                    return Ok(Some(TypeContext {
                        file: file.clone(),
                        range: syntax_range(decl.syntax()),
                    }));
                }
                if let Some(decl) = ast::EnumDeclaration::cast(anc.clone()) {
                    return Ok(Some(TypeContext {
                        file: file.clone(),
                        range: syntax_range(decl.syntax()),
                    }));
                }
                if let Some(decl) = ast::RecordDeclaration::cast(anc.clone()) {
                    return Ok(Some(TypeContext {
                        file: file.clone(),
                        range: syntax_range(decl.syntax()),
                    }));
                }
                if let Some(decl) = ast::AnnotationTypeDeclaration::cast(anc.clone()) {
                    return Ok(Some(TypeContext {
                        file: file.clone(),
                        range: syntax_range(decl.syntax()),
                    }));
                }

                // Anonymous classes have a `ClassBody` without a `ClassDeclaration` wrapper.
                if let Some(body) = ast::ClassBody::cast(anc) {
                    let is_named_class_body = body
                        .syntax()
                        .parent()
                        .is_some_and(|p| ast::ClassDeclaration::can_cast(p.kind()));
                    if !is_named_class_body {
                        return Ok(Some(TypeContext {
                            file: file.clone(),
                            range: syntax_range(body.syntax()),
                        }));
                    }
                }
            }

            Ok(None)
        }

        let mut cache: HashMap<FileId, nova_syntax::SyntaxNode> = HashMap::new();
        cache.insert(def.file.clone(), root.clone());

        let decl_ctx = type_context_at(db, &mut cache, &def.file, def.name_range)?;
        for usage in &targets {
            let usage_ctx = type_context_at(db, &mut cache, &usage.file, usage.range)?;
            if decl_ctx != usage_ctx {
                return Err(RefactorError::InlineNotSupported);
            }
        }
    }

    ensure_inline_variable_value_stable(
        db,
        &parsed,
        &def.file,
        decl.statement_range.end,
        &init_expr,
        &targets,
    )?;

    let mut remove_decl = params.inline_all || all_refs.len() == 1;
    if remove_decl
        && contains_unknown_name_expression(
            db,
            &def.file,
            text,
            &def.name,
            params.symbol,
            &all_refs,
        )
    {
        // If we cannot prove that our semantic reference index covers every textual occurrence of
        // the variable name, deleting the declaration can produce uncompilable code.
        //
        // When the user explicitly requested "inline all" we reject the refactoring. Otherwise we
        // fall back to keeping the declaration (still safe) even if the indexed reference set
        // makes it look like there is only one usage.
        if params.inline_all {
            return Err(RefactorError::InlineNotSupported);
        }
        remove_decl = false;
    }

    // If we are going to delete the declaration, we are moving evaluation of the initializer from
    // the declaration site (unconditionally, once) to the usage site. When the initializer is
    // order-sensitive, reject inlining when this would move evaluation into conditionally- or
    // repeatedly-evaluated contexts such as `&&`/`||` RHS, `?:` branches, loop conditions, or
    // `assert`.
    if remove_decl && init_is_order_sensitive {
        if let Err(err) = inline_variable_validate_safe_deletion_contexts(db, &targets) {
            // If the initializer has side effects, surface the more specific error so callers can
            // distinguish "not supported" from "would change side effects".
            return Err(
                if init_has_side_effects
                    && !params.inline_all
                    && matches!(err, RefactorError::InlineNotSupported)
                {
                    RefactorError::InlineSideEffects
                } else {
                    err
                },
            );
        }
    }

    if init_has_side_effects {
        // Side-effectful initializer inlining is only safe when we can guarantee it is evaluated
        // exactly once and that we preserve statement-level evaluation order.
        //
        // - If we would need to duplicate the initializer (multiple targets) or keep the
        //   declaration (inline-one with multiple usages), reject to avoid changing evaluation
        //   count.
        // - If the usage is conditional/repeated (e.g. under `if`/loops) or not adjacent to the
        //   declaration statement, reject to avoid changing conditionality/order.
        if !(all_refs.len() == 1 && remove_decl && targets.len() == 1) {
            return Err(RefactorError::InlineSideEffects);
        }

        // When deleting only a single declarator from a multi-declarator statement (`int a = f(),
        // b = g();`), the initializer moves relative to the remaining initializers, which can
        // reorder side effects. Be conservative and reject if any other initializer in the same
        // statement has side effects.
        if decl.declarator_delete_range.is_some() && decl.other_initializers_have_side_effects {
            return Err(RefactorError::InlineSideEffects);
        }

        // Even if the declaration is adjacent to the usage statement, deleting it can reorder the
        // initializer relative to other side-effectful expressions in the usage statement (e.g.
        // `int a = foo(); bar() + a` -> `bar() + foo()` changes evaluation order).
        //
        // The conservative ordering check is enforced later once we've located the usage
        // statement and can reason about evaluation order within that statement.
        let usage = targets
            .first()
            .expect("targets.len() == 1 checked above")
            .clone();

        // Side-effectful initializer inlining is only safe when we can guarantee:
        // - the usage executes exactly once (no conditional/loop execution boundaries)
        // - there are no intervening statements between declaration and usage
        // - we preserve statement-level evaluation order by requiring adjacency
        //
        // Be conservative and reject anything we can't confidently classify.
        if usage.file != def.file {
            return Err(RefactorError::InlineSideEffects);
        }

        let Some(usage_tok) = root
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| !tok.kind().is_trivia())
            .find(|tok| ranges_overlap(syntax_token_range(tok), usage.range))
        else {
            return Err(RefactorError::InlineNotSupported);
        };

        let Some(usage_stmt) = usage_tok
            .parent()
            .and_then(|n| n.ancestors().find_map(ast::Statement::cast))
        else {
            return Err(RefactorError::InlineNotSupported);
        };

        let decl_stmt = ast::Statement::cast(decl_stmt.syntax().clone())
            .ok_or(RefactorError::InlineNotSupported)?;

        // 1) Ensure the usage statement is the immediate next statement after the declaration,
        // within the same enclosing block/switch block.
        let Some(decl_parent) = decl_stmt.syntax().parent() else {
            return Err(RefactorError::InlineSideEffects);
        };
        let Some(usage_parent) = usage_stmt.syntax().parent() else {
            return Err(RefactorError::InlineSideEffects);
        };
        if decl_parent != usage_parent {
            return Err(RefactorError::InlineSideEffects);
        }

        let container_stmts: Vec<ast::Statement> =
            if let Some(block) = ast::Block::cast(decl_parent.clone()) {
                block.statements().collect()
            } else if let Some(block) = ast::SwitchBlock::cast(decl_parent.clone()) {
                block.statements().collect()
            } else if let Some(group) = ast::SwitchGroup::cast(decl_parent.clone()) {
                group.statements().collect()
            } else {
                return Err(RefactorError::InlineSideEffects);
            };

        let mut decl_idx: Option<usize> = None;
        let mut usage_idx: Option<usize> = None;
        let usage_stmt_range = syntax_range(usage_stmt.syntax());
        for (idx, stmt) in container_stmts.iter().enumerate() {
            let stmt_range = syntax_range(stmt.syntax());
            if stmt_range == decl.statement_range {
                decl_idx = Some(idx);
            }
            if stmt_range == usage_stmt_range {
                usage_idx = Some(idx);
            }
        }

        let (Some(decl_idx), Some(usage_idx)) = (decl_idx, usage_idx) else {
            return Err(RefactorError::InlineSideEffects);
        };
        if usage_idx != decl_idx + 1 {
            return Err(RefactorError::InlineSideEffects);
        }

        // 2) Reject when the usage is nested under an execution boundary relative to the
        // declaration (conservative).
        let decl_stmt_range = decl.statement_range;
        let usage_tok_parent = usage_tok
            .parent()
            .ok_or(RefactorError::InlineNotSupported)?;
        for ancestor in usage_tok_parent.ancestors() {
            let boundary_range = if ast::IfStatement::cast(ancestor.clone()).is_some()
                || ast::WhileStatement::cast(ancestor.clone()).is_some()
                || ast::DoWhileStatement::cast(ancestor.clone()).is_some()
                || ast::ForStatement::cast(ancestor.clone()).is_some()
                || matches!(
                    ancestor.kind(),
                    SyntaxKind::BasicForStatement | SyntaxKind::EnhancedForStatement
                )
                || ast::TryStatement::cast(ancestor.clone()).is_some()
                || ast::CatchClause::cast(ancestor.clone()).is_some()
                || ast::FinallyClause::cast(ancestor.clone()).is_some()
                || ast::LambdaExpression::cast(ancestor.clone()).is_some()
                || ast::SwitchGroup::cast(ancestor.clone()).is_some()
                || ast::SwitchRule::cast(ancestor.clone()).is_some()
                || ast::AssertStatement::cast(ancestor.clone()).is_some()
            {
                Some(syntax_range(&ancestor))
            } else {
                None
            };

            if let Some(boundary_range) = boundary_range {
                if !contains_range(boundary_range, decl_stmt_range) {
                    return Err(RefactorError::InlineSideEffects);
                }
            }
        }

        // 3) Reject inlining into short-circuit/conditional expression segments where the variable
        // usage may be evaluated conditionally.
        for ancestor in usage_tok_parent.ancestors() {
            if let Some(binary) = ast::BinaryExpression::cast(ancestor.clone()) {
                if let Some(op) = binary_short_circuit_operator_kind(&binary) {
                    if matches!(op, SyntaxKind::AmpAmp | SyntaxKind::PipePipe) {
                        if let Some(rhs) = binary.rhs() {
                            if contains_range(syntax_range(rhs.syntax()), usage.range) {
                                return Err(RefactorError::InlineSideEffects);
                            }
                        }
                    }
                }
            }

            if let Some(cond_expr) = ast::ConditionalExpression::cast(ancestor.clone()) {
                if let Some(then_branch) = cond_expr.then_branch() {
                    if contains_range(syntax_range(then_branch.syntax()), usage.range) {
                        return Err(RefactorError::InlineSideEffects);
                    }
                }
                if let Some(else_branch) = cond_expr.else_branch() {
                    if contains_range(syntax_range(else_branch.syntax()), usage.range) {
                        return Err(RefactorError::InlineSideEffects);
                    }
                }
            }
        }

        // 4) Prevent side-effect reordering within the usage statement itself.
        //
        // Example:
        //   int a = foo();
        //   System.out.println(bar() + a);
        //
        // Inlining `a` would produce `println(bar() + foo())`, which evaluates `bar()` before
        // `foo()`, changing the original side-effect ordering (`foo()` ran before the entire
        // `println(...)` statement).
        let usage_start = usage.range.start;
        if usage_stmt.syntax().descendants().any(|node| {
            if !has_side_effects(&node) {
                return false;
            }
            let range = syntax_range(&node);
            range.end <= usage_start
        }) {
            return Err(RefactorError::InlineSideEffects);
        }
    }

    // Even "pure" initializers can be order-sensitive because they may throw (NPE/AIOOBE/ClassCast)
    // at a different time if moved across intervening statements. When we delete the declaration,
    // require the earliest inlined usage statement to be the immediately following statement in
    // the same block statement list.
    if remove_decl && init_is_order_sensitive && !init_has_side_effects {
        check_order_sensitive_inline_order(&root, &decl_stmt, &targets, &def.file)?;
    }

    let mut edits: Vec<TextEdit> = Vec::with_capacity(targets.len());
    for usage in targets {
        // Safety check: ensure the byte range is still a name expression that resolves to the
        // target symbol. This avoids accidentally rewriting shadowed identifiers in code like:
        //
        //   int a = 1;
        //   { int a = 2; System.out.println(a); }
        //
        // where a naive, text-based scan could touch the inner `a`.
        match db.resolve_name_expr(&usage.file, usage.range) {
            Some(resolved) if resolved == params.symbol => {}
            _ => return Err(RefactorError::InlineNotSupported),
        }

        edits.push(TextEdit::replace(
            usage.file,
            usage.range,
            init_replacement.clone(),
        ));
    }

    if remove_decl {
        if let Some(delete_range) = decl.declarator_delete_range {
            edits.push(TextEdit::delete(def.file.clone(), delete_range));
        } else {
            // Delete the declaration statement. Be careful not to delete tokens that precede the
            // statement on the same line (e.g. `case 1: int a = ...;`).
            let stmt_range = decl.statement_range;
            let stmt_start = stmt_range.start;
            let stmt_end = stmt_range.end;
            let line_start = line_start(text, stmt_start);

            let decl_range = if text[line_start..stmt_start]
                .chars()
                .all(|c| c.is_whitespace())
            {
                // Statement begins at line start (only indentation precedes it). Delete indentation and
                // one trailing newline when present.
                let end = statement_end_including_trailing_newline(text, stmt_end);
                TextRange::new(line_start, end)
            } else {
                // Statement begins mid-line. Delete only the statement token range, but avoid leaving
                // awkward whitespace behind.
                let mut end = stmt_end;

                // If a newline immediately follows the statement, consume it too (preserve CRLF as a
                // unit).
                let tail = text.get(end..).unwrap_or_default();
                if tail.starts_with("\r\n") {
                    end += 2;
                } else if tail.starts_with('\n') || tail.starts_with('\r') {
                    end += 1;
                } else if let Some(comment_end) =
                    statement_end_including_trailing_inline_comment(text, end)
                {
                    // When deleting a mid-line statement (e.g. after `case 1:`), also delete any trailing
                    // inline comments (`// ...` or `/* ... */`) that occur before the line break. This
                    // avoids leaving a dangling comment behind after removing the statement.
                    end = comment_end;
                } else if matches!(text.as_bytes().get(end), Some(b' ')) {
                    // If there is a single space after the statement and another token follows on the
                    // same line, delete that one space (e.g. `; System.out...`).
                    let after_space = end + 1;
                    if after_space < text.len() {
                        let next = text.as_bytes()[after_space];
                        if next != b'\n' && next != b'\r' && next != b' ' && next != b'\t' {
                            end = after_space;
                        }
                    }
                }

                TextRange::new(stmt_start, end)
            };

            edits.push(TextEdit::delete(def.file.clone(), decl_range));
        }
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

fn ensure_inline_variable_dependencies_not_shadowed(
    db: &dyn RefactorDatabase,
    parsed: &nova_syntax::JavaParseResult,
    file: &FileId,
    text: &str,
    initializer_range: TextRange,
    initializer: &ast::Expression,
    targets: &[crate::semantic::Reference],
) -> Result<(), RefactorError> {
    let mut deps: HashMap<String, SymbolId> = HashMap::new();

    // Collect any *unqualified* locals/params/fields referenced by the initializer so we can
    // prevent cases where inlining would change what those identifiers resolve to at the usage
    // site (shadowing).
    //
    // Only consider the leftmost identifier segment of each `NameExpression` (e.g. `b` in `b`,
    // `b` in `b.x`, but not `x` in `b.x`), since qualified segments like `obj.field` cannot be
    // shadowed by locals.
    for name_expr in initializer
        .syntax()
        .descendants()
        .filter_map(ast::NameExpression::cast)
    {
        let Some(first_ident) = name_expr
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| !tok.kind().is_trivia())
            .find(|tok| tok.kind().is_identifier_like())
        else {
            continue;
        };

        let range = syntax_token_range(&first_ident);
        let Some(sym) = db.symbol_at(file, range.start) else {
            continue;
        };

        let kind = db.symbol_kind(sym);
        if !matches!(
            kind,
            Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter | JavaSymbolKind::Field)
        ) {
            continue;
        }

        // Ignore locals/params/fields declared inside the initializer expression itself (for
        // example lambda parameters or fields declared inside an anonymous class expression).
        // Inlining doesn't change their binding.
        if let Some(def) = db.symbol_definition(sym) {
            if def.file == *file
                && initializer_range.start <= def.name_range.start
                && def.name_range.end <= initializer_range.end
            {
                continue;
            }
        }

        let name = first_ident.text().to_string();
        if let Some(existing) = deps.insert(name.clone(), sym) {
            if existing != sym {
                // Same spelling resolves to different locals/params/fields inside the initializer
                // (e.g. nested scopes). This best-effort resolver doesn't support that.
                return Err(RefactorError::InlineNotSupported);
            }
        }
    }

    if deps.is_empty() {
        return Ok(());
    }

    for usage in targets {
        if usage.file != *file {
            // Locals should not have cross-file references.
            return Err(RefactorError::InlineNotSupported);
        }

        for (name, init_sym) in &deps {
            let init_kind = db.symbol_kind(*init_sym);

            // This lexical resolver only finds locals/params (and other local-like bindings such as
            // catch params / for-init decls / patterns). When the initializer dependency is a
            // field, `None` means "no local binding shadows it" (so the inlined name should still
            // resolve to the field).
            let decl_offset =
                lexical_resolve_local_or_param_decl_offset(parsed, text, usage.range.start, name);

            match init_kind {
                Some(JavaSymbolKind::Field) => {
                    if let Some(decl_offset) = decl_offset {
                        let Some(resolved_sym) = db.symbol_at(file, decl_offset) else {
                            return Err(RefactorError::InlineShadowedDependency {
                                name: name.clone(),
                            });
                        };

                        if resolved_sym != *init_sym {
                            return Err(RefactorError::InlineShadowedDependency {
                                name: name.clone(),
                            });
                        }
                    }
                }
                Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter) => {
                    let Some(decl_offset) = decl_offset else {
                        return Err(RefactorError::InlineShadowedDependency { name: name.clone() });
                    };

                    let Some(resolved_sym) = db.symbol_at(file, decl_offset) else {
                        return Err(RefactorError::InlineShadowedDependency { name: name.clone() });
                    };

                    if resolved_sym != *init_sym {
                        return Err(RefactorError::InlineShadowedDependency { name: name.clone() });
                    }
                }
                _ => return Err(RefactorError::InlineNotSupported),
            }
        }
    }

    Ok(())
}

fn ensure_inline_variable_value_stable(
    db: &dyn RefactorDatabase,
    parsed: &nova_syntax::JavaParseResult,
    file: &FileId,
    decl_stmt_end: usize,
    initializer: &ast::Expression,
    targets: &[crate::semantic::Reference],
) -> Result<(), RefactorError> {
    let mut deps: Vec<(SymbolId, String)> = Vec::new();
    let mut seen: HashSet<SymbolId> = HashSet::new();

    // Collect locals/params referenced by the initializer.
    for tok in initializer
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
    {
        if tok.kind() != SyntaxKind::Identifier {
            continue;
        }

        let range = syntax_token_range(&tok);
        let Some(sym) = db.symbol_at(file, range.start) else {
            continue;
        };

        // Treat any symbol whose *value* can change over time as a dependency that must remain
        // stable between the declaration and the inlined usage(s).
        //
        // Note: This is intentionally conservative for locals/params and also covers fields, since
        // re-evaluating `this.x` / `x` later can change behavior just like with locals.
        if !matches!(
            db.symbol_kind(sym),
            Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter | JavaSymbolKind::Field)
        ) {
            continue;
        }

        if seen.insert(sym) {
            deps.push((sym, tok.text().to_string()));
        }
    }

    if deps.is_empty() {
        return Ok(());
    }

    for usage in targets {
        let usage_start = usage.range.start;
        if usage_start <= decl_stmt_end {
            continue;
        }

        for (sym, name) in &deps {
            if has_write_to_symbol_between(db, parsed, file, *sym, decl_stmt_end, usage_start)? {
                return Err(RefactorError::InlineWouldChangeValue {
                    reason: format!(
                        "`{name}` is written between the variable declaration and the inlined usage"
                    ),
                });
            }
        }
    }

    Ok(())
}

/// Best-effort lexical name resolution for locals/parameters at `offset`.
///
/// This is intentionally syntax-based and only understands:
/// - local variables declared via `LocalVariableDeclarationStatement` in blocks,
/// - locals declared in `for (...)` headers,
/// - locals declared in `try ( ... )` resource specifications,
/// - catch clause parameters,
/// - pattern variables (`instanceof` / record patterns / switch patterns),
/// - method/constructor parameters, and
/// - lambda parameters.
///
/// It returns the *declaration identifier token start offset* of the resolved binding, if found.
fn lexical_resolve_local_or_param_decl_offset(
    parsed: &nova_syntax::JavaParseResult,
    text: &str,
    offset: usize,
    name: &str,
) -> Option<usize> {
    if offset >= text.len() {
        return None;
    }

    let element = parsed.covering_element(nova_syntax::TextRange {
        start: u32::try_from(offset).ok()?,
        end: u32::try_from(offset + 1).ok()?,
    });

    let node = match element {
        nova_syntax::SyntaxElement::Node(node) => node,
        nova_syntax::SyntaxElement::Token(token) => token.parent()?,
    };

    for anc in node.ancestors() {
        if let Some(block) = ast::Block::cast(anc.clone()) {
            if let Some(decl_offset) = lexical_resolve_in_block(&block, offset, name) {
                return Some(decl_offset);
            }
        }

        // Lambdas introduce their own parameter scope which shadows outer locals/params.
        if let Some(lambda) = ast::LambdaExpression::cast(anc.clone()) {
            if let Some(decl_offset) = lexical_resolve_in_lambda_params(&lambda, name) {
                return Some(decl_offset);
            }
        }

        if let Some(catch_clause) = ast::CatchClause::cast(anc.clone()) {
            if let Some(decl_offset) = lexical_resolve_in_catch_param(&catch_clause, name) {
                return Some(decl_offset);
            }
        }

        if let Some(try_stmt) = ast::TryStatement::cast(anc.clone()) {
            if let Some(decl_offset) = lexical_resolve_in_try_resources(&try_stmt, offset, name) {
                return Some(decl_offset);
            }
        }

        if let Some(for_stmt) = ast::ForStatement::cast(anc.clone()) {
            if let Some(decl_offset) = lexical_resolve_in_for_header(&for_stmt, offset, name) {
                return Some(decl_offset);
            }
        }

        if let Some(stmt) = ast::Statement::cast(anc.clone()) {
            if let Some(decl_offset) = lexical_resolve_in_type_patterns(&stmt, offset, name) {
                return Some(decl_offset);
            }
        }
    }

    // If no locals were found, fall back to method/constructor parameters.
    if let Some(method) = node.ancestors().find_map(ast::MethodDeclaration::cast) {
        if let Some(decl_offset) = lexical_resolve_in_parameter_list(method.parameter_list(), name)
        {
            return Some(decl_offset);
        }
    }

    if let Some(ctor) = node.ancestors().find_map(ast::ConstructorDeclaration::cast) {
        if let Some(decl_offset) = lexical_resolve_in_parameter_list(ctor.parameter_list(), name) {
            return Some(decl_offset);
        }
    }

    None
}

fn lexical_resolve_in_block(block: &ast::Block, offset: usize, name: &str) -> Option<usize> {
    let mut found = None;
    for stmt in block.statements() {
        let stmt_range = syntax_range(stmt.syntax());
        if stmt_range.start > offset {
            break;
        }

        let ast::Statement::LocalVariableDeclarationStatement(local) = stmt else {
            continue;
        };

        let Some(list) = local.declarator_list() else {
            continue;
        };

        for declarator in list.declarators() {
            let Some(name_tok) = declarator.name_token() else {
                continue;
            };
            if name_tok.text() != name {
                continue;
            }
            let start = u32::from(name_tok.text_range().start()) as usize;
            if start < offset {
                found = Some(start);
            }
        }
    }
    found
}

fn lexical_resolve_in_lambda_params(lambda: &ast::LambdaExpression, name: &str) -> Option<usize> {
    let params = lambda.parameters()?;
    for param in params.parameters() {
        let Some(name_tok) = param.name_token() else {
            continue;
        };
        if name_tok.text() == name {
            return Some(u32::from(name_tok.text_range().start()) as usize);
        }
    }
    None
}

fn lexical_resolve_in_type_patterns(
    stmt: &ast::Statement,
    offset: usize,
    name: &str,
) -> Option<usize> {
    let mut found = None;
    for pat in stmt
        .syntax()
        .descendants()
        .filter_map(ast::TypePattern::cast)
    {
        let Some(name_tok) = pat.name_token() else {
            continue;
        };
        if name_tok.text() != name {
            continue;
        }
        let start = u32::from(name_tok.text_range().start()) as usize;
        if start < offset {
            found = Some(start);
        }
    }
    found
}

fn lexical_resolve_in_for_header(
    for_stmt: &ast::ForStatement,
    offset: usize,
    name: &str,
) -> Option<usize> {
    let header = for_stmt.header()?;
    for declarator in header
        .syntax()
        .descendants()
        .filter_map(ast::VariableDeclarator::cast)
    {
        let Some(name_tok) = declarator.name_token() else {
            continue;
        };
        if name_tok.text() != name {
            continue;
        }
        let start = u32::from(name_tok.text_range().start()) as usize;
        if start < offset {
            return Some(start);
        }
    }
    None
}

fn lexical_resolve_in_try_resources(
    try_stmt: &ast::TryStatement,
    offset: usize,
    name: &str,
) -> Option<usize> {
    let resources = try_stmt.resources()?;
    let resources_range = syntax_range(resources.syntax());
    let in_resources = resources_range.start <= offset && offset < resources_range.end;

    let in_body = try_stmt.body().is_some_and(|body| {
        let body_range = syntax_range(body.syntax());
        body_range.start <= offset && offset < body_range.end
    });

    if !in_resources && !in_body {
        // Resource variables are not in scope in `catch`/`finally` clauses (javac rejects them),
        // so only resolve them when the usage is inside the resource specification itself or the
        // try body.
        return None;
    }

    for resource in resources.resources() {
        for declarator in resource
            .syntax()
            .descendants()
            .filter_map(ast::VariableDeclarator::cast)
        {
            let Some(name_tok) = declarator.name_token() else {
                continue;
            };
            if name_tok.text() != name {
                continue;
            }
            let start = u32::from(name_tok.text_range().start()) as usize;
            if start < offset {
                return Some(start);
            }
        }
    }
    None
}

fn lexical_resolve_in_catch_param(catch_clause: &ast::CatchClause, name: &str) -> Option<usize> {
    // There is no typed AST wrapper for the catch parameter yet, so parse it token-wise.
    // The parameter name is the last identifier before the closing `)` of `catch (...)`.
    let mut last_ident: Option<(usize, bool)> = None;
    for el in catch_clause.syntax().children_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        if tok.kind().is_trivia() {
            continue;
        }
        if tok.kind() == SyntaxKind::Identifier {
            let start = u32::from(tok.text_range().start()) as usize;
            last_ident = Some((start, tok.text() == name));
        }
        if tok.kind() == SyntaxKind::RParen {
            break;
        }
    }

    match last_ident {
        Some((start, true)) => Some(start),
        _ => None,
    }
}

fn lexical_resolve_in_parameter_list(
    params: Option<ast::ParameterList>,
    name: &str,
) -> Option<usize> {
    let params = params?;
    for param in params.parameters() {
        let Some(name_tok) = param.name_token() else {
            continue;
        };
        if name_tok.text() == name {
            return Some(u32::from(name_tok.text_range().start()) as usize);
        }
    }
    None
}

fn has_write_to_symbol_between(
    db: &dyn RefactorDatabase,
    parsed: &nova_syntax::JavaParseResult,
    file: &FileId,
    symbol: SymbolId,
    start: usize,
    end: usize,
) -> Result<bool, RefactorError> {
    if start >= end {
        return Ok(false);
    }
    for reference in db.find_references(symbol) {
        if reference.file != *file {
            continue;
        }
        if reference.range.start < start || reference.range.start >= end {
            continue;
        }
        if reference_is_write(parsed, reference.range)? {
            return Ok(true);
        }
    }

    Ok(false)
}

fn inline_variable_validate_safe_deletion_contexts(
    db: &dyn RefactorDatabase,
    targets: &[crate::semantic::Reference],
) -> Result<(), RefactorError> {
    let mut parses: HashMap<FileId, nova_syntax::JavaParseResult> = HashMap::new();

    for usage in targets {
        let parsed = match parses.get(&usage.file) {
            Some(parsed) => parsed,
            None => {
                let text = db
                    .file_text(&usage.file)
                    .ok_or_else(|| RefactorError::UnknownFile(usage.file.clone()))?;
                let parsed = parse_java(text);
                parses.insert(usage.file.clone(), parsed);
                parses.get(&usage.file).expect("just inserted parse result")
            }
        };

        let name_expr = inline_variable_find_name_expr(parsed, usage.range)?;
        if inline_variable_usage_is_conditionally_or_repeatedly_evaluated(&name_expr) {
            return Err(RefactorError::InlineNotSupported);
        }
    }

    Ok(())
}

fn inline_variable_find_name_expr(
    parsed: &nova_syntax::JavaParseResult,
    range: TextRange,
) -> Result<ast::NameExpression, RefactorError> {
    let syntax_range = to_syntax_range(range).ok_or(RefactorError::InlineNotSupported)?;
    let element = parsed.covering_element(syntax_range);

    let node = match element {
        nova_syntax::SyntaxElement::Node(node) => node,
        nova_syntax::SyntaxElement::Token(token) => {
            token.parent().ok_or(RefactorError::InlineNotSupported)?
        }
    };

    node.ancestors()
        .find_map(ast::NameExpression::cast)
        .ok_or(RefactorError::InlineNotSupported)
}

fn inline_variable_usage_is_conditionally_or_repeatedly_evaluated(
    name_expr: &ast::NameExpression,
) -> bool {
    let expr_range = syntax_range(name_expr.syntax());

    for ancestor in name_expr.syntax().ancestors() {
        if let Some(binary) = ast::BinaryExpression::cast(ancestor.clone()) {
            if let Some(op) = binary_short_circuit_operator_kind(&binary) {
                if matches!(op, SyntaxKind::AmpAmp | SyntaxKind::PipePipe) {
                    if let Some(rhs) = binary.rhs() {
                        if contains_range(syntax_range(rhs.syntax()), expr_range) {
                            return true;
                        }
                    }
                }
            }
        }

        if let Some(cond_expr) = ast::ConditionalExpression::cast(ancestor.clone()) {
            if let Some(then_branch) = cond_expr.then_branch() {
                if contains_range(syntax_range(then_branch.syntax()), expr_range) {
                    return true;
                }
            }
            if let Some(else_branch) = cond_expr.else_branch() {
                if contains_range(syntax_range(else_branch.syntax()), expr_range) {
                    return true;
                }
            }
        }

        if let Some(while_stmt) = ast::WhileStatement::cast(ancestor.clone()) {
            if let Some(cond) = while_stmt.condition() {
                if contains_range(syntax_range(cond.syntax()), expr_range) {
                    return true;
                }
            }
        }

        if let Some(do_while_stmt) = ast::DoWhileStatement::cast(ancestor.clone()) {
            if let Some(cond) = do_while_stmt.condition() {
                if contains_range(syntax_range(cond.syntax()), expr_range) {
                    return true;
                }
            }
        }

        if let Some(for_stmt) = ast::ForStatement::cast(ancestor.clone()) {
            if let Some(header) = for_stmt.header() {
                if for_header_has_unsafe_eval_context(&header, expr_range) {
                    return true;
                }
            }
        }

        if let Some(assert_stmt) = ast::AssertStatement::cast(ancestor.clone()) {
            if let Some(cond) = assert_stmt.condition() {
                if contains_range(syntax_range(cond.syntax()), expr_range) {
                    return true;
                }
            }
            if let Some(message) = assert_stmt.message() {
                if contains_range(syntax_range(message.syntax()), expr_range) {
                    return true;
                }
            }
        }

        // Optional: switch *expression* rule bodies that are expressions (not blocks). These are
        // conditionally evaluated based on the selector, so moving an order-sensitive initializer
        // into them changes evaluation timing.
        if let Some(rule) = ast::SwitchRule::cast(ancestor.clone()) {
            let Some(body) = rule.body() else {
                continue;
            };
            if !matches!(body, ast::SwitchRuleBody::Expression(_)) {
                continue;
            }

            // Only guard when the name expression is inside the rule body (not in the labels/guard).
            let body_range = syntax_range(body.syntax());
            if !contains_range(body_range, expr_range) {
                continue;
            }

            // Distinguish switch expression vs switch statement.
            let container = rule.syntax().ancestors().skip(1).find_map(|node| {
                if ast::SwitchExpression::cast(node.clone()).is_some() {
                    Some(true)
                } else if ast::SwitchStatement::cast(node).is_some() {
                    Some(false)
                } else {
                    None
                }
            });
            if container == Some(true) {
                return true;
            }
        }
    }

    false
}
fn inline_variable_has_writes(
    db: &dyn RefactorDatabase,
    symbol: SymbolId,
    def: &crate::semantic::SymbolDefinition,
) -> Result<bool, RefactorError> {
    let mut parses: HashMap<FileId, nova_syntax::JavaParseResult> = HashMap::new();

    for reference in db.find_references(symbol) {
        // Some RefactorDatabase implementations may include the definition span in the reference
        // list; ignore it so we only reject on writes after the initializer.
        if reference.file == def.file && reference.range == def.name_range {
            continue;
        }

        let parsed = match parses.get(&reference.file) {
            Some(parsed) => parsed,
            None => {
                let text = db
                    .file_text(&reference.file)
                    .ok_or_else(|| RefactorError::UnknownFile(reference.file.clone()))?;
                let parsed = parse_java(text);
                parses.insert(reference.file.clone(), parsed);
                parses
                    .get(&reference.file)
                    .expect("just inserted parse result")
            }
        };

        match reference_is_write(parsed, reference.range) {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(err) => return Err(err),
        }
    }

    Ok(false)
}

fn reference_is_write(
    parsed: &nova_syntax::JavaParseResult,
    range: TextRange,
) -> Result<bool, RefactorError> {
    let syntax_range = to_syntax_range(range).ok_or(RefactorError::InlineNotSupported)?;
    let element = parsed.covering_element(syntax_range);

    let node = match element {
        nova_syntax::SyntaxElement::Node(node) => node,
        nova_syntax::SyntaxElement::Token(token) => {
            token.parent().ok_or(RefactorError::InlineNotSupported)?
        }
    };

    for ancestor in node.ancestors() {
        if let Some(assign) = ast::AssignmentExpression::cast(ancestor.clone()) {
            // Only treat this reference as a write if the assignment target is the variable
            // itself (`a = 1`, `a += 1`), not when the reference appears somewhere inside a more
            // complex lvalue (`arr[idx] = 1`, `obj.field = 1`).
            if let Some(ast::Expression::NameExpression(name)) = assign.lhs() {
                let name_tok = name
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|el| el.into_token())
                    .filter(|tok| tok.kind().is_identifier_like())
                    .last();
                if let Some(tok) = name_tok {
                    if syntax_token_range(&tok) == range {
                        return Ok(true);
                    }
                }
            }
        }

        if let Some(unary) = ast::UnaryExpression::cast(ancestor) {
            // Only treat this reference as a write if the inc/dec operand is the variable
            // itself (`a++`, `++a`), not when the reference appears somewhere inside a more complex
            // lvalue (`arr[idx]++`, `obj.field++`).
            if unary_is_inc_or_dec(&unary) {
                if let Some(ast::Expression::NameExpression(name)) = unary.operand() {
                    let name_tok = name
                        .syntax()
                        .descendants_with_tokens()
                        .filter_map(|el| el.into_token())
                        .filter(|tok| tok.kind().is_identifier_like())
                        .last();
                    if let Some(tok) = name_tok {
                        if syntax_token_range(&tok) == range {
                            return Ok(true);
                        }
                    }
                }
            }
        }
    }

    Ok(false)
}

fn unary_is_inc_or_dec(expr: &ast::UnaryExpression) -> bool {
    let mut first: Option<SyntaxKind> = None;
    let mut last: Option<SyntaxKind> = None;

    for tok in expr
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)
    {
        let kind = tok.kind();
        if first.is_none() {
            first = Some(kind);
        }
        last = Some(kind);
    }

    matches!(first, Some(SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
        || matches!(last, Some(SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
}

fn to_syntax_range(range: TextRange) -> Option<nova_syntax::TextRange> {
    Some(nova_syntax::TextRange {
        start: u32::try_from(range.start).ok()?,
        end: u32::try_from(range.end).ok()?,
    })
}

pub struct OrganizeImportsParams {
    pub file: FileId,
}

pub fn organize_imports(
    db: &dyn RefactorDatabase,
    params: OrganizeImportsParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let text = db
        .file_text(&params.file)
        .ok_or_else(|| RefactorError::UnknownFile(params.file.clone()))?;

    let import_block = parse_import_block(text);
    if import_block.imports.is_empty() {
        return Ok(WorkspaceEdit::default());
    }

    let body_start = import_block.range.end;
    let usage = collect_identifier_usage(&text[body_start..]);
    let declared_types = collect_declared_type_names(&text[body_start..]);

    let mut normal = Vec::new();
    let mut static_imports = Vec::new();
    let mut explicit_non_static: HashSet<String> = HashSet::new();

    // First pass: filter explicit (non-wildcard) imports based on unqualified identifier usage.
    // We use unqualified identifiers so that references like `foo.Bar` or `Foo.BAR` do not keep
    // otherwise-unused imports.
    let mut wildcard_candidates = Vec::new();
    for import in &import_block.imports {
        if import.is_wildcard() {
            wildcard_candidates.push(import.clone());
            continue;
        }

        let Some(simple) = import.simple_name() else {
            continue;
        };
        if usage.unqualified.contains(simple) {
            if import.is_static {
                static_imports.push(import.render());
            } else {
                explicit_non_static.insert(simple.to_string());
                normal.push(import.render());
            }
        }
    }

    // Second pass: keep wildcard imports conservatively.
    //
    // We only drop a non-static wildcard import (`foo.bar.*`) when:
    // - there is at least one kept explicit import from the same package, and
    // - all *type-like* identifiers in the file appear to be already covered by explicit imports,
    //   declared types, or common `java.lang` names (heuristic).
    //
    // This avoids deleting `.*` imports in files that likely rely on them.
    let uncovered_type_idents = collect_uncovered_type_identifiers(
        &usage.unqualified,
        &explicit_non_static,
        &declared_types,
    );

    // Precompute whether each package has any explicit imports that survived filtering.
    let mut explicit_by_package: HashMap<String, usize> = HashMap::new();
    for import in &import_block.imports {
        if import.is_static || import.is_wildcard() {
            continue;
        }
        if let Some((pkg, _)) = import.split_package_and_name() {
            if usage
                .unqualified
                .contains(import.simple_name().unwrap_or_default())
            {
                *explicit_by_package.entry(pkg.to_string()).or_default() += 1;
            }
        }
    }

    for import in wildcard_candidates {
        if import.is_static {
            // Static wildcard imports are hard to validate heuristically because they introduce
            // unqualified method and constant names. Keep them.
            static_imports.push(import.render());
            continue;
        }

        let Some(pkg) = import.wildcard_package() else {
            normal.push(import.render());
            continue;
        };

        let has_explicit_cover = explicit_by_package.get(pkg).copied().unwrap_or(0) > 0;
        let can_remove = has_explicit_cover && uncovered_type_idents.is_empty();
        if !can_remove {
            normal.push(import.render());
        }
    }

    normal.sort();
    normal.dedup();
    static_imports.sort();
    static_imports.dedup();

    let mut out = String::new();
    for import in &normal {
        out.push_str(import);
        out.push('\n');
    }

    if !normal.is_empty() && !static_imports.is_empty() {
        out.push('\n');
    }

    for import in &static_imports {
        out.push_str(import);
        out.push('\n');
    }

    // Ensure exactly one blank line after imports when there is any body.
    // If all imports were removed, keep the original header spacing untouched.
    if body_start < text.len() && !(normal.is_empty() && static_imports.is_empty()) {
        out.push('\n');
    }

    // If the computed block is identical, return an empty edit to reduce churn.
    let original_block = &text[import_block.range.start..import_block.range.end];
    if original_block == out {
        return Ok(WorkspaceEdit::default());
    }

    let mut edits = Vec::new();
    edits.push(TextEdit::replace(
        params.file.clone(),
        import_block.range,
        out,
    ));

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

fn trim_range(text: &str, mut range: TextRange) -> TextRange {
    if range.start >= range.end || range.end > text.len() {
        return range;
    }
    let bytes = text.as_bytes();
    while range.start < range.end && bytes[range.start].is_ascii_whitespace() {
        range.start += 1;
    }
    while range.start < range.end && bytes[range.end - 1].is_ascii_whitespace() {
        range.end -= 1;
    }
    range
}

fn syntax_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn check_extract_variable_field_shadowing(
    stmt: &ast::Statement,
    file: &FileId,
    name: &str,
    replacement_ranges: &[TextRange],
) -> Result<(), RefactorError> {
    if !enclosing_type_declares_field_named(stmt.syntax(), name) {
        return Ok(());
    }

    let stmt_start = syntax_range(stmt.syntax()).start;
    // The new local is inserted immediately before `stmt`. Use the closest enclosing statement-list
    // container to approximate the scope where the local will shadow an existing field:
    // - a `{ ... }` block (local variable scope is the block)
    // - otherwise the surrounding switch block (locals declared in a `case:` group are in scope
    //   until the end of the switch block)
    let scope = stmt.syntax().ancestors().find_map(|node| {
        if let Some(block) = ast::Block::cast(node.clone()) {
            Some(block.syntax().clone())
        } else {
            ast::SwitchBlock::cast(node).map(|b| b.syntax().clone())
        }
    });

    let Some(scope) = scope else {
        // If we can't determine the scope, fall back to allowing extraction. Other validation
        // (including name conflicts) will reject unsupported contexts.
        return Ok(());
    };

    for name_expr in scope.descendants().filter_map(ast::NameExpression::cast) {
        let range = syntax_range(name_expr.syntax());
        if range.start < stmt_start {
            continue;
        }
        if replacement_ranges
            .iter()
            .any(|replacement_range| ranges_overlap(range, *replacement_range))
        {
            continue;
        }
        // Only flag *unqualified* same-name identifier references. Qualified names like `C.value`
        // or `obj.value` are unaffected by introducing a same-named local.
        if name_expression_has_dot(&name_expr) {
            continue;
        }
        // Avoid false positives: `foo` in `foo(...)` is a method name, not a field access.
        if let Some(parent) = name_expr.syntax().parent() {
            if let Some(call) = ast::MethodCallExpression::cast(parent) {
                if call
                    .callee()
                    .is_some_and(|callee| callee.syntax() == name_expr.syntax())
                {
                    continue;
                }
            }
        }
        let Some(tok) = ast::support::ident_token(name_expr.syntax()) else {
            continue;
        };
        if tok.text() != name {
            continue;
        }

        return Err(RefactorError::Conflicts(vec![Conflict::FieldShadowing {
            file: file.clone(),
            name: name.to_string(),
            usage_range: range,
        }]));
    }

    Ok(())
}

fn enclosing_type_declares_field_named(node: &nova_syntax::SyntaxNode, name: &str) -> bool {
    for anc in node.ancestors() {
        if let Some(body) = ast::ClassBody::cast(anc.clone()) {
            return members_declare_field_named(body.members(), name);
        }
        if let Some(body) = ast::InterfaceBody::cast(anc.clone()) {
            return members_declare_field_named(body.members(), name);
        }
        if let Some(body) = ast::EnumBody::cast(anc.clone()) {
            return members_declare_field_named(body.members(), name);
        }
        if let Some(body) = ast::RecordBody::cast(anc.clone()) {
            return members_declare_field_named(body.members(), name);
        }
        if let Some(body) = ast::AnnotationBody::cast(anc.clone()) {
            return members_declare_field_named(body.members(), name);
        }
    }
    false
}

fn members_declare_field_named(
    members: impl Iterator<Item = ast::ClassMember>,
    name: &str,
) -> bool {
    for member in members {
        let ast::ClassMember::FieldDeclaration(field) = member else {
            continue;
        };
        let Some(list) = field.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            let Some(tok) = decl.name_token() else {
                continue;
            };
            if tok.text() == name {
                return true;
            }
        }
    }
    false
}
fn check_extract_variable_name_conflicts(
    file: &FileId,
    stmt: &ast::Statement,
    insert_pos: usize,
    name: &str,
) -> Result<(), RefactorError> {
    let name_collision = || {
        RefactorError::Conflicts(vec![Conflict::NameCollision {
            file: file.clone(),
            name: name.to_string(),
            // Extract Variable can run on a `TextDatabase` without semantic IDs for declarations.
            existing_symbol: SymbolId::new(u32::MAX),
        }])
    };

    // The extracted variable's declaration is inserted at `insert_pos` before `stmt`. We need a
    // conservative scope range for name collision checks.
    //
    // Prefer the *nearest* statement-list container:
    // - a `{ ... }` block (local variable scope is the block)
    // - otherwise the surrounding switch block (traditional `case:` groups share switch scope)
    let Some(insertion_scope) = stmt.syntax().ancestors().find_map(|node| {
        if let Some(block) = ast::Block::cast(node.clone()) {
            Some(syntax_range(block.syntax()))
        } else {
            ast::SwitchBlock::cast(node).map(|b| syntax_range(b.syntax()))
        }
    }) else {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot determine scope for extracted variable",
        });
    };

    let new_scope = TextRange::new(insert_pos, insertion_scope.end);

    let Some(enclosing) = find_enclosing_body_owner(stmt) else {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot determine enclosing method/initializer body for extraction",
        });
    };

    // Method/constructor parameters.
    if enclosing.has_parameter_named(name) {
        return Err(name_collision());
    }

    // Local variable declarators.
    for decl in enclosing
        .body()
        .syntax()
        .descendants()
        .filter_map(ast::VariableDeclarator::cast)
    {
        if is_within_nested_type(decl.syntax(), enclosing.body().syntax()) {
            continue;
        }
        // Loop variables declared in `for (...)` headers are scoped to the `for` statement,
        // not the surrounding block. Handle them separately below so we use the correct scope.
        if decl
            .syntax()
            .ancestors()
            .any(|node| node.kind() == SyntaxKind::ForHeader)
        {
            continue;
        }
        let Some(tok) = decl.name_token() else {
            continue;
        };
        if tok.text() != name {
            continue;
        }
        let Some(scope) = local_binding_scope_range(&decl) else {
            continue;
        };
        if ranges_overlap(new_scope, scope) {
            return Err(name_collision());
        }
    }

    // Pattern variables (`instanceof` / record patterns / switch patterns).
    //
    // Pattern matching introduces bindings whose scope is not purely block-based (it depends on
    // control flow and the surrounding construct). We use a conservative approximation of scope
    // based on the enclosing control structure / statement list, and reject extraction when the new
    // binding's scope would overlap and collide.
    for pat in enclosing
        .body()
        .syntax()
        .descendants()
        .filter_map(ast::TypePattern::cast)
    {
        if is_within_nested_type(pat.syntax(), enclosing.body().syntax()) {
            continue;
        }
        let Some(tok) = pat.name_token() else {
            continue;
        };
        if tok.text() != name {
            continue;
        }
        let Some(scopes) = pattern_binding_scope_ranges(&pat) else {
            continue;
        };
        if scopes.iter().any(|scope| ranges_overlap(new_scope, *scope)) {
            return Err(name_collision());
        }
    }

    // `for (...)` / enhanced-for loop header variables.
    for for_stmt in enclosing
        .body()
        .syntax()
        .descendants()
        .filter_map(ast::ForStatement::cast)
    {
        if is_within_nested_type(for_stmt.syntax(), enclosing.body().syntax()) {
            continue;
        }

        let Some(header) = for_stmt
            .syntax()
            .children()
            .find(|n| n.kind() == SyntaxKind::ForHeader)
        else {
            continue;
        };

        let for_end = syntax_range(for_stmt.syntax()).end;
        for decl in header
            .descendants()
            .filter_map(ast::VariableDeclarator::cast)
        {
            let Some(tok) = decl.name_token() else {
                continue;
            };
            if tok.text() != name {
                continue;
            }

            let start = u32::from(tok.text_range().start()) as usize;
            let scope = TextRange::new(start, for_end);
            if ranges_overlap(new_scope, scope) {
                return Err(name_collision());
            }
        }
    }

    // Catch parameters.
    for catch_clause in enclosing
        .body()
        .syntax()
        .descendants()
        .filter_map(ast::CatchClause::cast)
    {
        if is_within_nested_type(catch_clause.syntax(), enclosing.body().syntax()) {
            continue;
        }
        let Some(body) = catch_clause.body() else {
            continue;
        };
        let Some(param_name) = catch_parameter_name(&catch_clause) else {
            continue;
        };
        if param_name != name {
            continue;
        }
        let scope = syntax_range(body.syntax());
        if ranges_overlap(new_scope, scope) {
            return Err(name_collision());
        }
    }

    // Lambda parameters.
    for lambda in enclosing
        .body()
        .syntax()
        .descendants()
        .filter_map(ast::LambdaExpression::cast)
    {
        if is_within_nested_type(lambda.syntax(), enclosing.body().syntax()) {
            continue;
        }

        let Some(body) = lambda.body() else {
            continue;
        };
        let lambda_scope = if let Some(block) = body.block() {
            syntax_range(block.syntax())
        } else if let Some(expr) = body.expression() {
            syntax_range(expr.syntax())
        } else {
            continue;
        };
        if !ranges_overlap(new_scope, lambda_scope) {
            continue;
        }

        let Some(params) = lambda.parameters() else {
            continue;
        };

        let mut has_conflict = false;
        if let Some(list) = params.parameter_list() {
            has_conflict = list
                .parameters()
                .filter_map(|p| p.name_token().map(|t| t.text().to_string()))
                .any(|n| n == name);
        } else if let Some(param) = params.parameter() {
            has_conflict = param.name_token().is_some_and(|t| t.text() == name);
        }

        if has_conflict {
            return Err(name_collision());
        }
    }

    Ok(())
}

fn ranges_overlap(a: TextRange, b: TextRange) -> bool {
    a.start < b.end && b.start < a.end
}

fn pattern_binding_scope_ranges(pat: &ast::TypePattern) -> Option<Vec<TextRange>> {
    // Switch label patterns (`case Type name -> ...` / `case Type name:` / record patterns).
    //
    // Pattern variables introduced in a switch label are only in scope for the corresponding case
    // group/rule body (not across the entire switch).
    if pat.syntax().ancestors().any(|n| {
        ast::SwitchLabel::cast(n.clone()).is_some() || ast::CaseLabelElement::cast(n).is_some()
    }) {
        if let Some(rule) = pat.syntax().ancestors().find_map(ast::SwitchRule::cast) {
            let scope = rule
                .body()
                .map(|body| syntax_range(body.syntax()))
                .unwrap_or_else(|| syntax_range(rule.syntax()));
            return Some(vec![scope]);
        }
        if let Some(group) = pat.syntax().ancestors().find_map(ast::SwitchGroup::cast) {
            return Some(vec![syntax_range(group.syntax())]);
        }
    }

    // Patterns introduced in conditional expressions (`if` / `while` / `for` / `do-while`).
    //
    // For simple `if`/`while` conditions that are just an `instanceof <pattern>` (possibly wrapped
    // in parentheses and `!` negations), we can approximate the binding scope as:
    // - always: the condition expression itself (the declaration site)
    // - plus: the body branch where the pattern is definitely matched (then/else/loop body)
    //
    // For more complex boolean expressions, fall back to a conservative statement-level scope.
    if let Some(inst) = pat
        .syntax()
        .ancestors()
        .find_map(ast::InstanceofExpression::cast)
    {
        let inst_range = syntax_range(inst.syntax());

        // `if (cond) ...`
        if let Some(if_stmt) = pat
            .syntax()
            .ancestors()
            .filter_map(ast::IfStatement::cast)
            .find(|if_stmt| {
                if_stmt
                    .condition()
                    .is_some_and(|cond| contains_range(syntax_range(cond.syntax()), inst_range))
            })
        {
            let stmt_range = syntax_range(if_stmt.syntax());
            let Some(condition) = if_stmt.condition() else {
                return Some(vec![stmt_range]);
            };

            if let Some(matches_when_true) = pattern_matches_when_condition_true(pat, &condition) {
                let mut scopes = vec![syntax_range(condition.syntax())];
                if matches_when_true {
                    if let Some(then_branch) = if_stmt.then_branch() {
                        scopes.push(syntax_range(then_branch.syntax()));
                    }
                } else if let Some(else_branch) = if_stmt.else_branch() {
                    scopes.push(syntax_range(else_branch.syntax()));
                }

                // Guard-style flow scoping: the pattern can be in scope *after* the if-statement
                // when all paths that reach the following statements imply a successful match.
                if pattern_in_scope_after_if_statement(pat, &if_stmt) {
                    if let Some(container) = nearest_flow_scope_container_range(if_stmt.syntax()) {
                        let after_if = TextRange::new(stmt_range.end, container.end);
                        if after_if.start < after_if.end {
                            scopes.push(after_if);
                        }
                    }
                }

                scopes.sort_by_key(|r| (r.start, r.end));
                scopes.dedup();
                return Some(scopes);
            }

            // Complex condition: conservatively treat as statement-scoped.
            return Some(vec![stmt_range]);
        }

        // `while (cond) ...`
        if let Some(while_stmt) = pat
            .syntax()
            .ancestors()
            .filter_map(ast::WhileStatement::cast)
            .find(|while_stmt| {
                while_stmt
                    .condition()
                    .is_some_and(|cond| contains_range(syntax_range(cond.syntax()), inst_range))
            })
        {
            let stmt_range = syntax_range(while_stmt.syntax());
            let Some(condition) = while_stmt.condition() else {
                return Some(vec![stmt_range]);
            };

            if let Some(matches_when_true) = pattern_matches_when_condition_true(pat, &condition) {
                let mut scopes = vec![syntax_range(condition.syntax())];
                if matches_when_true {
                    if let Some(body) = while_stmt.body() {
                        scopes.push(syntax_range(body.syntax()));
                    }
                }
                scopes.sort_by_key(|r| (r.start, r.end));
                scopes.dedup();
                return Some(scopes);
            }

            return Some(vec![stmt_range]);
        }

        // Classic `for (...; cond; ...) ...`
        if let Some(for_stmt) = pat
            .syntax()
            .ancestors()
            .filter_map(ast::ForStatement::cast)
            .find(|for_stmt| {
                for_stmt.header().is_some_and(|header| {
                    for_header_condition_segment_range(&header)
                        .is_some_and(|segment| contains_range(segment, inst_range))
                })
            })
        {
            let stmt_range = syntax_range(for_stmt.syntax());
            let Some(header) = for_stmt.header() else {
                return Some(vec![stmt_range]);
            };
            let Some(segment) = for_header_condition_segment_range(&header) else {
                return Some(vec![stmt_range]);
            };

            // Try to recover the condition expression node so we can detect simple negation.
            let mut condition_expr: Option<ast::Expression> = None;
            let mut best_len: usize = 0;
            for expr in header
                .syntax()
                .descendants()
                .filter_map(ast::Expression::cast)
            {
                let expr_range = syntax_range(expr.syntax());
                if !contains_range(segment, expr_range) {
                    continue;
                }
                if !contains_range(expr_range, inst_range) {
                    continue;
                }
                let len = expr_range.len();
                if len >= best_len {
                    best_len = len;
                    condition_expr = Some(expr);
                }
            }

            let Some(condition) = condition_expr else {
                return Some(vec![stmt_range]);
            };

            if let Some(matches_when_true) = pattern_matches_when_condition_true(pat, &condition) {
                let mut scopes = vec![syntax_range(condition.syntax())];
                if matches_when_true {
                    if let Some(body) = for_stmt.body() {
                        scopes.push(syntax_range(body.syntax()));
                    }
                }
                scopes.sort_by_key(|r| (r.start, r.end));
                scopes.dedup();
                return Some(scopes);
            }

            return Some(vec![stmt_range]);
        }

        // `do { ... } while (cond);`
        //
        // Pattern variables in the do-while condition are *not* in scope in the body (the body runs
        // before the condition is evaluated).
        if let Some(do_stmt) = pat
            .syntax()
            .ancestors()
            .filter_map(ast::DoWhileStatement::cast)
            .find(|do_stmt| {
                do_stmt
                    .condition()
                    .is_some_and(|cond| contains_range(syntax_range(cond.syntax()), inst_range))
            })
        {
            let stmt_range = syntax_range(do_stmt.syntax());
            let Some(condition) = do_stmt.condition() else {
                return Some(vec![stmt_range]);
            };
            return Some(vec![syntax_range(condition.syntax())]);
        }
    }

    // Default: treat as statement-scoped.
    let stmt = pat.syntax().ancestors().find_map(ast::Statement::cast)?;
    Some(vec![syntax_range(stmt.syntax())])
}

fn pattern_in_scope_after_if_statement(pat: &ast::TypePattern, if_stmt: &ast::IfStatement) -> bool {
    let Some(condition) = if_stmt.condition() else {
        return false;
    };

    let Some(pattern_matches_when_condition_true) =
        pattern_matches_when_condition_true(pat, &condition)
    else {
        return false;
    };

    let Some(then_branch) = if_stmt.then_branch() else {
        return false;
    };

    let then_completes = !statement_always_exits(&then_branch);
    let else_completes = if let Some(else_branch) = if_stmt.else_branch() {
        !statement_always_exits(&else_branch)
    } else {
        // No else branch means the implicit else is an empty statement, which completes normally.
        true
    };

    // If control can reach after the `if` through the then-branch, then the condition was `true`.
    // If control can reach after the `if` through the else-branch, then the condition was `false`.
    //
    // Pattern variables are in scope after the statement only if every path that reaches after the
    // `if` implies a successful match for the pattern.
    if then_completes && !pattern_matches_when_condition_true {
        return false;
    }
    if else_completes && pattern_matches_when_condition_true {
        return false;
    }

    // If no path reaches after the if-statement, there is no meaningful "after" scope.
    then_completes || else_completes
}

fn pattern_matches_when_condition_true(
    pat: &ast::TypePattern,
    condition: &ast::Expression,
) -> Option<bool> {
    // Find the `instanceof` expression that introduces this pattern binding.
    let inst = pat
        .syntax()
        .ancestors()
        .find_map(ast::InstanceofExpression::cast)?;

    let cond_range = syntax_range(condition.syntax());
    let inst_range = syntax_range(inst.syntax());
    if !(cond_range.start <= inst_range.start && inst_range.end <= cond_range.end) {
        return None;
    }

    // Walk up the tree from the instanceof expression to the condition, tracking whether the
    // condition is negated by an odd number of `!` operators.
    let mut negated = false;
    let mut node = inst.syntax().clone();
    let cond_syntax = condition.syntax();

    while &node != cond_syntax {
        let parent = node.parent()?;

        if ast::ParenthesizedExpression::cast(parent.clone()).is_some() {
            node = parent;
            continue;
        }

        if let Some(unary) = ast::UnaryExpression::cast(parent.clone()) {
            let is_bang = unary
                .syntax()
                .first_token()
                .is_some_and(|tok| tok.kind() == SyntaxKind::Bang);
            if !is_bang {
                return None;
            }
            negated = !negated;
            node = parent;
            continue;
        }

        // We only handle simple conditions that are a (possibly parenthesized) instanceof pattern,
        // optionally wrapped in `!` negations. For more complex boolean expressions, fall back to a
        // conservative statement-local scope approximation.
        return None;
    }

    Some(!negated)
}

fn statement_always_exits(stmt: &ast::Statement) -> bool {
    match stmt {
        ast::Statement::ReturnStatement(_)
        | ast::Statement::ThrowStatement(_)
        | ast::Statement::BreakStatement(_)
        | ast::Statement::ContinueStatement(_) => true,
        ast::Statement::Block(block) => {
            for stmt in block.statements() {
                if statement_always_exits(&stmt) {
                    return true;
                }
            }
            false
        }
        ast::Statement::IfStatement(if_stmt) => {
            let Some(then_branch) = if_stmt.then_branch() else {
                return false;
            };
            let Some(else_branch) = if_stmt.else_branch() else {
                return false;
            };
            statement_always_exits(&then_branch) && statement_always_exits(&else_branch)
        }
        _ => false,
    }
}

fn nearest_flow_scope_container_range(node: &nova_syntax::SyntaxNode) -> Option<TextRange> {
    node.ancestors().find_map(|node| {
        if let Some(block) = ast::Block::cast(node.clone()) {
            Some(syntax_range(block.syntax()))
        } else if let Some(group) = ast::SwitchGroup::cast(node.clone()) {
            Some(syntax_range(group.syntax()))
        } else if let Some(rule) = ast::SwitchRule::cast(node.clone()) {
            Some(
                rule.body()
                    .map(|body| syntax_range(body.syntax()))
                    .unwrap_or_else(|| syntax_range(rule.syntax())),
            )
        } else {
            ast::SwitchBlock::cast(node).map(|b| syntax_range(b.syntax()))
        }
    })
}

fn for_header_condition_segment_range(header: &ast::ForHeader) -> Option<TextRange> {
    // Classic for-loop header shape:
    // `for (<init> ; <condition> ; <update>)`
    let mut semicolons = Vec::new();
    let mut r_paren = None;

    for el in header.syntax().children_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        match tok.kind() {
            SyntaxKind::Semicolon => semicolons.push(tok),
            SyntaxKind::RParen => r_paren = Some(tok),
            _ => {}
        }
    }

    if semicolons.len() < 2 {
        return None;
    }
    let Some(r_paren) = r_paren else {
        return None;
    };

    let first_semi = syntax_token_range(&semicolons[0]);
    let second_semi = syntax_token_range(&semicolons[1]);
    let r_paren = syntax_token_range(&r_paren);

    let condition_segment = TextRange::new(first_semi.end, second_semi.start);
    let condition_segment = TextRange::new(
        condition_segment.start,
        condition_segment.end.min(r_paren.start),
    );
    Some(condition_segment)
}

#[derive(Clone, Debug)]
enum EnclosingBodyOwner {
    Method(ast::MethodDeclaration, ast::Block),
    Constructor(ast::ConstructorDeclaration, ast::Block),
    CompactConstructor(ast::Block),
    Initializer(ast::Block),
}

impl EnclosingBodyOwner {
    fn body(&self) -> &ast::Block {
        match self {
            EnclosingBodyOwner::Method(_, body) | EnclosingBodyOwner::Constructor(_, body) => body,
            EnclosingBodyOwner::CompactConstructor(body)
            | EnclosingBodyOwner::Initializer(body) => body,
        }
    }

    fn has_parameter_named(&self, name: &str) -> bool {
        match self {
            EnclosingBodyOwner::Method(method, _) => method.parameter_list().is_some_and(|list| {
                list.parameters()
                    .any(|p| p.name_token().is_some_and(|t| t.text() == name))
            }),
            EnclosingBodyOwner::Constructor(ctor, _) => ctor.parameter_list().is_some_and(|list| {
                list.parameters()
                    .any(|p| p.name_token().is_some_and(|t| t.text() == name))
            }),
            EnclosingBodyOwner::CompactConstructor(_) | EnclosingBodyOwner::Initializer(_) => false,
        }
    }
}

fn find_enclosing_body_owner(stmt: &ast::Statement) -> Option<EnclosingBodyOwner> {
    for node in stmt.syntax().ancestors() {
        if let Some(method) = ast::MethodDeclaration::cast(node.clone()) {
            let body = method.body()?;
            return Some(EnclosingBodyOwner::Method(method, body));
        }
        if let Some(ctor) = ast::ConstructorDeclaration::cast(node.clone()) {
            let body = ctor.body()?;
            return Some(EnclosingBodyOwner::Constructor(ctor, body));
        }
        if let Some(ctor) = ast::CompactConstructorDeclaration::cast(node.clone()) {
            let body = ctor.body()?;
            return Some(EnclosingBodyOwner::CompactConstructor(body));
        }
        if let Some(init) = ast::InitializerBlock::cast(node) {
            let body = init.body()?;
            return Some(EnclosingBodyOwner::Initializer(body));
        }
    }
    None
}

fn is_within_nested_type(
    node: &nova_syntax::SyntaxNode,
    stop_at: &nova_syntax::SyntaxNode,
) -> bool {
    for anc in node.ancestors() {
        if &anc == stop_at {
            break;
        }
        if ast::ClassDeclaration::can_cast(anc.kind())
            || ast::InterfaceDeclaration::can_cast(anc.kind())
            || ast::EnumDeclaration::can_cast(anc.kind())
            || ast::RecordDeclaration::can_cast(anc.kind())
            || ast::AnnotationTypeDeclaration::can_cast(anc.kind())
            // Anonymous classes have a `ClassBody` without a `ClassDeclaration` wrapper.
            || ast::ClassBody::can_cast(anc.kind())
            || ast::InterfaceBody::can_cast(anc.kind())
            || ast::EnumBody::can_cast(anc.kind())
            || ast::RecordBody::can_cast(anc.kind())
        {
            return true;
        }
    }
    false
}

fn catch_parameter_name(catch_clause: &ast::CatchClause) -> Option<String> {
    let mut last_ident: Option<String> = None;
    for el in catch_clause.syntax().descendants_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        if tok.kind() == SyntaxKind::RParen {
            break;
        }
        if tok.kind().is_identifier_like() {
            last_ident = Some(tok.text().to_string());
        }
    }
    last_ident
}

fn local_binding_scope_range(decl: &ast::VariableDeclarator) -> Option<TextRange> {
    let decl_range = syntax_range(decl.syntax());

    for for_stmt in decl
        .syntax()
        .ancestors()
        .filter_map(ast::ForStatement::cast)
    {
        let header = for_stmt.header()?;
        let header_range = syntax_range(header.syntax());
        if contains_range(header_range, decl_range) {
            return Some(syntax_range(for_stmt.syntax()));
        }
    }

    for try_stmt in decl
        .syntax()
        .ancestors()
        .filter_map(ast::TryStatement::cast)
    {
        let resources = try_stmt.resources()?;
        let resources_range = syntax_range(resources.syntax());
        if contains_range(resources_range, decl_range) {
            // Try-with-resources locals are only in scope within the `try { ... }` body (they are
            // *not* visible inside `catch` / `finally` clauses).
            //
            // Using the whole `try` statement range here would lead to false-positive name
            // collisions when extracting/introducing a new local inside `catch` / `finally`.
            if let Some(body) = try_stmt.body() {
                return Some(syntax_range(body.syntax()));
            }
            return Some(syntax_range(try_stmt.syntax()));
        }
    }

    // Default: nearest enclosing block-like scope.
    if let Some(block) = decl.syntax().ancestors().find_map(ast::Block::cast) {
        return Some(syntax_range(block.syntax()));
    }
    if let Some(block) = decl.syntax().ancestors().find_map(ast::SwitchBlock::cast) {
        return Some(syntax_range(block.syntax()));
    }

    None
}

fn find_expression(
    source: &str,
    root: nova_syntax::SyntaxNode,
    selection: TextRange,
) -> Option<ast::Expression> {
    for expr in root.descendants().filter_map(ast::Expression::cast) {
        // The Java AST can include trivia (whitespace/comments) in node ranges. Prefer comparing
        // against a trivia-trimmed range so selections still match when the user does *not*
        // include trailing inline comments (e.g. `new Foo() /* comment */;`).
        //
        // Fall back to a simple whitespace-trimmed range for compatibility with selections that
        // *do* include comments but not surrounding whitespace.
        let range = trimmed_syntax_range(expr.syntax());
        if range.start == selection.start && range.end == selection.end {
            return Some(expr);
        }

        let range = trim_range(source, syntax_range(expr.syntax()));
        if range.start == selection.start && range.end == selection.end {
            return Some(expr);
        }
    }
    None
}

fn rewrite_multi_declarator_local_variable_declaration(
    source: &str,
    stmt: &ast::LocalVariableDeclarationStatement,
    stmt_range: TextRange,
    expr_range: TextRange,
    expr_text: &str,
    extracted_name: &str,
    extracted_ty: &str,
    indent: &str,
    newline: &str,
) -> Option<String> {
    let list = stmt.declarator_list()?;
    let decls: Vec<_> = list.declarators().collect();
    if decls.len() <= 1 {
        return None;
    }

    let mut target_idx: Option<usize> = None;
    for (idx, decl) in decls.iter().enumerate() {
        let Some(init) = decl.initializer() else {
            continue;
        };
        let init_range = syntax_range(init.syntax());
        if init_range.start <= expr_range.start && expr_range.end <= init_range.end {
            target_idx = Some(idx);
            break;
        }
    }
    let target_idx = target_idx?;
    if target_idx == 0 {
        return None;
    }

    let first_decl = decls.first()?;
    let prev_decl = decls.get(target_idx - 1)?;
    let target_decl = decls.get(target_idx)?;
    let last_decl = decls.last()?;

    let first_decl_range = syntax_range(first_decl.syntax());
    let prev_decl_range = syntax_range(prev_decl.syntax());
    let target_decl_range = syntax_range(target_decl.syntax());
    let last_decl_range = syntax_range(last_decl.syntax());

    // The declarator node range can include separator whitespace/comments after the comma. Use the
    // identifier/pattern token range as the true start of the declarator so we never produce
    // invalid output like:
    //   int // comment
    //       b = tmp;
    let target_start = target_decl
        .name_token()
        .map(|tok| syntax_token_range(&tok).start)
        .or_else(|| {
            target_decl
                .unnamed_pattern()
                .map(|pat| syntax_range(pat.syntax()).start)
        })
        .unwrap_or(target_decl_range.start);

    if expr_range.start < target_start || expr_range.end > last_decl_range.end {
        return None;
    }

    let prefix_text = source
        .get(stmt_range.start..first_decl_range.start)?
        .to_string();
    let before_text = source
        .get(first_decl_range.start..prev_decl_range.end)?
        .to_string();
    let between_text = source.get(prev_decl_range.end..target_start)?.to_string();
    let between_trivia = between_text
        .split_once(',')
        .map(|(_, after)| after)
        .unwrap_or(&between_text)
        .trim()
        .to_string();

    let after_text = source.get(target_start..last_decl_range.end)?.to_string();
    let stmt_suffix = source.get(last_decl_range.end..stmt_range.end)?.to_string();

    let rel_start = expr_range.start - target_start;
    let rel_end = expr_range.end - target_start;
    let after_replaced = format!(
        "{}{}{}",
        &after_text[..rel_start],
        extracted_name,
        &after_text[rel_end..]
    );

    let mut replacement = String::new();
    replacement.push_str(&prefix_text);
    replacement.push_str(&before_text);
    replacement.push(';');
    if !between_trivia.is_empty() {
        replacement.push(' ');
        replacement.push_str(&between_trivia);
    }
    replacement.push_str(newline);
    replacement.push_str(indent);
    replacement.push_str(extracted_ty);
    replacement.push(' ');
    replacement.push_str(extracted_name);
    replacement.push_str(" = ");
    replacement.push_str(expr_text);
    replacement.push(';');
    replacement.push_str(newline);
    replacement.push_str(indent);
    replacement.push_str(&prefix_text);
    replacement.push_str(&after_replaced);
    replacement.push_str(&stmt_suffix);

    Some(replacement)
}

fn normalize_expr_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn find_replace_all_occurrences_same_execution_context(
    source: &str,
    root: nova_syntax::SyntaxNode,
    insertion_stmt: &ast::Statement,
    selected_text: &str,
) -> Vec<TextRange> {
    let selected_norm = normalize_expr_text(selected_text);
    if selected_norm.is_empty() {
        return Vec::new();
    }

    // Execution context owner: the nearest enclosing lambda (if any), plus the nearest class body.
    //
    // This prevents `replace_all=true` from replacing occurrences across lambda boundaries and
    // across nested/anonymous class bodies. Replacing into those contexts can change evaluation
    // timing and frequency, since the extracted local is computed at the insertion point.
    let insertion_lambda = insertion_stmt
        .syntax()
        .ancestors()
        .find_map(ast::LambdaExpression::cast);
    let insertion_class_body = insertion_stmt
        .syntax()
        .ancestors()
        .find_map(ast::ClassBody::cast);

    // Restrict to the closest statement-list-like scope to avoid replacing occurrences where the
    // extracted local would not be visible *or* would not have been evaluated yet.
    //
    // `switch` case groups (`case 1: ...`) are special: a local declared in one case group is in
    // scope until the end of the switch block, but execution can jump directly to a later case
    // label without running the earlier case group's statements. Replacing occurrences in other
    // case groups would reference a local that was never initialized.
    let search_root = insertion_stmt
        .syntax()
        .ancestors()
        .find_map(|node| {
            if let Some(block) = ast::Block::cast(node.clone()) {
                return Some(block.syntax().clone());
            }
            if let Some(group) = ast::SwitchGroup::cast(node.clone()) {
                return Some(group.syntax().clone());
            }
            if let Some(rule) = ast::SwitchRule::cast(node.clone()) {
                return rule.body().map(|body| body.syntax().clone());
            }
            None
        })
        .unwrap_or(root);

    let min_offset = syntax_range(insertion_stmt.syntax()).start;

    let mut ranges = Vec::new();
    for expr in search_root.descendants().filter_map(ast::Expression::cast) {
        // `SyntaxNode::text_range()` for expressions can include trivia. Use a trivia-trimmed range
        // so `replace_all` still matches selections like `new Foo()` against occurrences like
        // `new Foo() /*comment*/`, and so replacing preserves trailing comments.
        let token_range = trimmed_syntax_range(expr.syntax());

        // The extracted local is declared immediately before `insertion_stmt`, so we only replace
        // occurrences within that statement and after it.
        if token_range.start < min_offset {
            continue;
        }

        let Some(text) = source.get(token_range.start..token_range.end) else {
            continue;
        };
        if normalize_expr_text(text) != selected_norm {
            continue;
        }

        let expr_lambda = expr
            .syntax()
            .ancestors()
            .find_map(ast::LambdaExpression::cast);
        if expr_lambda != insertion_lambda {
            continue;
        }

        let expr_class_body = expr.syntax().ancestors().find_map(ast::ClassBody::cast);
        if expr_class_body != insertion_class_body {
            continue;
        }

        ranges.push(expr_replacement_range(expr.syntax()));
    }

    ranges.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));
    ranges.dedup();
    ranges
}

fn constant_expression_only_context_reason(expr: &ast::Expression) -> Option<&'static str> {
    for node in expr.syntax().ancestors() {
        if ast::AnnotationElementValue::cast(node.clone()).is_some() {
            return Some(
                "cannot extract from annotation element values (compile-time constant required)",
            );
        }

        if ast::CaseLabelElement::cast(node.clone()).is_some()
            || ast::SwitchLabel::cast(node).is_some()
        {
            return Some("cannot extract from switch case labels (compile-time constant required)");
        }
    }

    None
}

fn extract_variable_crosses_execution_boundary(expr: &ast::Expression) -> Option<&'static str> {
    let expr_range = syntax_range(expr.syntax());

    // Walk up the syntax tree; if we cross into a lambda/switch execution context that cannot
    // contain an inserted statement (without additional wrapping conversions), reject the
    // refactoring.
    for node in expr.syntax().ancestors() {
        if let Some(lambda) = ast::LambdaExpression::cast(node.clone()) {
            if let Some(body_expr) = lambda.body().and_then(|body| body.expression()) {
                let body_range = syntax_range(body_expr.syntax());
                if body_range.start <= expr_range.start && expr_range.end <= body_range.end {
                    return Some("cannot extract from expression-bodied lambda");
                }
            }
        }

        let Some(rule) = ast::SwitchRule::cast(node) else {
            continue;
        };
        let Some(body) = rule.body() else {
            continue;
        };
        if matches!(body, ast::SwitchRuleBody::Block(_)) {
            continue;
        }

        // Only guard when the selection is inside the rule body (not the labels/guard).
        let body_range = syntax_range(body.syntax());
        if !(body_range.start <= expr_range.start && expr_range.end <= body_range.end) {
            continue;
        }

        // Reject when inside a switch *expression* rule body that is not a block, since extracting
        // would either lift evaluation out of the selected case arm or require block/yield
        // conversion (not implemented yet).
        let container = rule.syntax().ancestors().skip(1).find_map(|node| {
            if ast::SwitchExpression::cast(node.clone()).is_some() {
                Some(true)
            } else if ast::SwitchStatement::cast(node).is_some() {
                Some(false)
            } else {
                None
            }
        });
        if container == Some(true) {
            return Some("cannot extract from switch expression rule body");
        }
    }

    None
}

fn infer_expr_type(expr: &ast::Expression) -> String {
    let inferred = match expr {
        ast::Expression::LiteralExpression(lit) => infer_type_from_literal(lit),
        ast::Expression::NewExpression(new_expr) => {
            infer_type_from_new_expression(new_expr).unwrap_or_else(|| "Object".to_string())
        }
        ast::Expression::ArrayCreationExpression(array_expr) => {
            let Some(base_ty) = array_expr.ty() else {
                return "Object".to_string();
            };
            let base = render_java_type(base_ty.syntax());

            let mut dims = 0usize;
            if let Some(dim_exprs) = array_expr.dim_exprs() {
                dims += dim_exprs.dims().count();
            }
            if let Some(dims_node) = array_expr.dims() {
                dims += dims_node.dims().count();
            }

            if dims == 0 {
                base
            } else {
                format!("{base}{}", "[]".repeat(dims))
            }
        }
        ast::Expression::CastExpression(cast) => cast
            .ty()
            .map(|ty| {
                let rendered = render_java_type(ty.syntax());
                // Java intersection types (`A & B`) are not denotable in variable declarations,
                // so avoid emitting them as explicit local variable types.
                if rendered.contains('&') {
                    "Object".to_string()
                } else {
                    rendered
                }
            })
            .unwrap_or_else(|| "Object".to_string()),
        ast::Expression::ConditionalExpression(cond) => {
            let Some(then_branch) = cond.then_branch() else {
                return "Object".to_string();
            };
            let Some(else_branch) = cond.else_branch() else {
                return "Object".to_string();
            };

            let then_ty = infer_expr_type(&then_branch);
            let else_ty = infer_expr_type(&else_branch);
            if then_ty == else_ty {
                then_ty
            } else {
                "Object".to_string()
            }
        }
        ast::Expression::ParenthesizedExpression(expr) => expr
            .expression()
            .map(|inner| infer_expr_type(&inner))
            .unwrap_or_else(|| "Object".to_string()),
        ast::Expression::InstanceofExpression(_) => "boolean".to_string(),
        ast::Expression::UnaryExpression(unary) => infer_type_from_unary_expr(unary),
        ast::Expression::BinaryExpression(binary) => infer_type_from_binary_expr(binary),
        ast::Expression::MethodCallExpression(call) => {
            infer_type_from_method_call(call).unwrap_or_else(|| "Object".to_string())
        }
        ast::Expression::ThisExpression(_)
        | ast::Expression::SuperExpression(_)
        | ast::Expression::NameExpression(_)
        | ast::Expression::ArrayInitializer(_) => "Object".to_string(),
        _ => "Object".to_string(),
    };

    // If we couldn't infer a meaningful type from the expression itself, try a cheap contextual
    // fallback: use the declared type of the variable/field whose initializer contains this
    // expression (when available).
    //
    // This is still parser-only and helps common cases like extracting `null`, `this`, or unknown
    // call expressions from `String x = ...`.
    if inferred == "Object" {
        if let Some(ctx) = infer_type_from_enclosing_declaration(expr) {
            return ctx;
        }
    }

    inferred
}

fn infer_type_from_method_call(call: &ast::MethodCallExpression) -> Option<String> {
    // Best-effort: resolve simple unqualified method calls to method declarations in the nearest
    // enclosing type body. This keeps parser-only type inference useful in unit-test mode
    // (`TextDatabase`) without requiring full type-checker information.
    //
    // Be conservative: only attempt inference for calls of the form `foo(...)` (no explicit
    // receiver). Inferring the return type for `obj.foo(...)` would require knowing the receiver
    // type, and guessing based on the current class would be incorrect.
    let callee = call.callee()?;
    let ast::Expression::NameExpression(name_expr) = callee else {
        return None;
    };
    if name_expr
        .syntax()
        .descendants_with_tokens()
        .any(|el| el.kind() == SyntaxKind::Dot)
    {
        return None;
    }
    let name = name_expr
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| tok.kind().is_identifier_like())
        .last()?
        .text()
        .to_string();

    infer_method_return_type(call.syntax(), &name)
}

fn infer_method_return_type(node: &nova_syntax::SyntaxNode, name: &str) -> Option<String> {
    // Walk outward through nested type bodies to approximate Java's implicit `Outer.this` member
    // access rules for unqualified method calls inside inner classes.
    for ancestor in node.ancestors() {
        let members: Vec<ast::ClassMember> =
            if let Some(body) = ast::ClassBody::cast(ancestor.clone()) {
                body.members().collect()
            } else if let Some(body) = ast::InterfaceBody::cast(ancestor.clone()) {
                body.members().collect()
            } else if let Some(body) = ast::EnumBody::cast(ancestor.clone()) {
                body.members().collect()
            } else if let Some(body) = ast::RecordBody::cast(ancestor.clone()) {
                body.members().collect()
            } else {
                continue;
            };

        let mut inferred: Option<String> = None;
        let mut saw_candidate = false;
        for member in members {
            let ast::ClassMember::MethodDeclaration(method) = member else {
                continue;
            };
            let Some(name_tok) = method.name_token() else {
                continue;
            };
            if name_tok.text() != name {
                continue;
            }
            saw_candidate = true;

            let Some(ret_ty) = method.return_type() else {
                continue;
            };
            let rendered = render_java_type(ret_ty.syntax());
            match inferred.as_deref() {
                None => inferred = Some(rendered),
                Some(existing) if existing == rendered.as_str() => {}
                Some(_) => {
                    // Ambiguous overloads with different return types.
                    inferred = None;
                    break;
                }
            }
        }

        if saw_candidate {
            return inferred;
        }
    }

    None
}

fn infer_type_from_new_expression(expr: &ast::NewExpression) -> Option<String> {
    if let Some(ty) = expr.ty() {
        let rendered = render_java_type(ty.syntax());
        if rendered != "Object" {
            return Some(rendered);
        }
    }

    // Some parse trees model the instantiated name directly as a `NamedType` / `ClassOrInterfaceType`
    // node under the `NewExpression` rather than wrapping it in a `Type` node. Fall back to
    // rendering the first type-like child node.
    for child in expr.syntax().children() {
        match child.kind() {
            SyntaxKind::Type
            | SyntaxKind::NamedType
            | SyntaxKind::PrimitiveType
            | SyntaxKind::ArrayType
            | SyntaxKind::AnnotatedType
            | SyntaxKind::ClassOrInterfaceType
            | SyntaxKind::ClassType
            | SyntaxKind::InterfaceType
            | SyntaxKind::TypeVariable => {
                let rendered = render_java_type(&child);
                if rendered != "Object" {
                    return Some(rendered);
                }
            }
            _ => {}
        }
    }

    None
}

fn infer_type_from_enclosing_declaration(expr: &ast::Expression) -> Option<String> {
    let expr_range = syntax_range(expr.syntax());

    // Walk up to the nearest variable declarator and check if `expr` is within its initializer.
    for node in expr.syntax().ancestors() {
        let Some(declarator) = ast::VariableDeclarator::cast(node.clone()) else {
            continue;
        };
        let Some(initializer) = declarator.initializer() else {
            continue;
        };

        let init_range = syntax_range(initializer.syntax());
        if !(init_range.start <= expr_range.start && expr_range.end <= init_range.end) {
            continue;
        }

        // Prefer the closest declaration site.
        for ancestor in declarator.syntax().ancestors() {
            if let Some(local) = ast::LocalVariableDeclarationStatement::cast(ancestor.clone()) {
                let ty = local.ty()?;
                let rendered = render_java_type(ty.syntax());
                if rendered == "var" {
                    return None;
                }
                return Some(rendered);
            }

            if let Some(field) = ast::FieldDeclaration::cast(ancestor) {
                let ty = field.ty()?;
                return Some(render_java_type(ty.syntax()));
            }
        }
    }

    None
}

fn first_non_trivia_child_token_kind(node: &nova_syntax::SyntaxNode) -> Option<SyntaxKind> {
    node.children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)
        .map(|tok| tok.kind())
}

fn numeric_rank(ty: &str) -> Option<u8> {
    match ty {
        "double" => Some(4),
        "float" => Some(3),
        "long" => Some(2),
        "int" | "char" => Some(1),
        _ => None,
    }
}

fn numeric_type_for_rank(rank: u8) -> &'static str {
    match rank {
        4 => "double",
        3 => "float",
        2 => "long",
        _ => "int",
    }
}

fn integral_rank(ty: &str) -> Option<u8> {
    match ty {
        "long" => Some(2),
        "int" | "char" => Some(1),
        _ => None,
    }
}

fn integral_type_for_rank(rank: u8) -> &'static str {
    match rank {
        2 => "long",
        _ => "int",
    }
}

fn infer_type_from_unary_expr(unary: &ast::UnaryExpression) -> String {
    let Some(op) = first_non_trivia_child_token_kind(unary.syntax()) else {
        return "Object".to_string();
    };

    match op {
        SyntaxKind::Bang => "boolean".to_string(),
        SyntaxKind::Plus | SyntaxKind::Minus => {
            let Some(operand) = unary.operand() else {
                return "Object".to_string();
            };
            let operand_ty = infer_expr_type(&operand);
            let Some(rank) = numeric_rank(&operand_ty) else {
                return "Object".to_string();
            };
            numeric_type_for_rank(rank).to_string()
        }
        SyntaxKind::Tilde => {
            let Some(operand) = unary.operand() else {
                return "Object".to_string();
            };
            let operand_ty = infer_expr_type(&operand);
            let Some(rank) = integral_rank(&operand_ty) else {
                return "Object".to_string();
            };
            integral_type_for_rank(rank).to_string()
        }
        // We don't know the operand type without typeck, and returning an incorrect primitive type
        // here would make the extracted code not compile. Default to Object (boxing).
        SyntaxKind::PlusPlus | SyntaxKind::MinusMinus => "Object".to_string(),
        _ => "Object".to_string(),
    }
}

fn infer_type_from_binary_expr(binary: &ast::BinaryExpression) -> String {
    let Some(op) = first_non_trivia_child_token_kind(binary.syntax()) else {
        return "Object".to_string();
    };

    match op {
        SyntaxKind::Less
        | SyntaxKind::LessEq
        | SyntaxKind::Greater
        | SyntaxKind::GreaterEq
        | SyntaxKind::EqEq
        | SyntaxKind::BangEq
        | SyntaxKind::AmpAmp
        | SyntaxKind::PipePipe => return "boolean".to_string(),
        _ => {}
    }

    let lhs_ty = binary.lhs().map(|lhs| infer_expr_type(&lhs));
    let rhs_ty = binary.rhs().map(|rhs| infer_expr_type(&rhs));

    match op {
        SyntaxKind::Plus => {
            // Best-effort: infer string concatenation when either side is known to be `String`
            // (literal, cast, `new String()`, or a previously-inferred sub-expression).
            if lhs_ty.as_deref() == Some("String") || rhs_ty.as_deref() == Some("String") {
                return "String".to_string();
            }

            let (Some(lhs_ty), Some(rhs_ty)) = (lhs_ty, rhs_ty) else {
                return "Object".to_string();
            };
            let (Some(lhs_rank), Some(rhs_rank)) = (numeric_rank(&lhs_ty), numeric_rank(&rhs_ty))
            else {
                return "Object".to_string();
            };
            numeric_type_for_rank(lhs_rank.max(rhs_rank)).to_string()
        }
        SyntaxKind::Minus | SyntaxKind::Star | SyntaxKind::Slash | SyntaxKind::Percent => {
            let (Some(lhs_ty), Some(rhs_ty)) = (lhs_ty, rhs_ty) else {
                return "Object".to_string();
            };
            let (Some(lhs_rank), Some(rhs_rank)) = (numeric_rank(&lhs_ty), numeric_rank(&rhs_ty))
            else {
                return "Object".to_string();
            };
            numeric_type_for_rank(lhs_rank.max(rhs_rank)).to_string()
        }
        SyntaxKind::LeftShift | SyntaxKind::RightShift | SyntaxKind::UnsignedRightShift => {
            let Some(lhs_ty) = lhs_ty else {
                return "Object".to_string();
            };
            let Some(rank) = integral_rank(&lhs_ty) else {
                return "Object".to_string();
            };
            integral_type_for_rank(rank).to_string()
        }
        SyntaxKind::Amp | SyntaxKind::Pipe | SyntaxKind::Caret => {
            let (Some(lhs_ty), Some(rhs_ty)) = (lhs_ty, rhs_ty) else {
                return "Object".to_string();
            };

            if lhs_ty == "boolean" && rhs_ty == "boolean" {
                return "boolean".to_string();
            }

            let (Some(lhs_rank), Some(rhs_rank)) = (integral_rank(&lhs_ty), integral_rank(&rhs_ty))
            else {
                return "Object".to_string();
            };
            integral_type_for_rank(lhs_rank.max(rhs_rank)).to_string()
        }
        _ => "Object".to_string(),
    }
}

fn var_initializer_requires_explicit_type(expr: &ast::Expression) -> bool {
    match expr {
        ast::Expression::ParenthesizedExpression(par) => par
            .expression()
            .as_ref()
            .is_some_and(var_initializer_requires_explicit_type),
        ast::Expression::LiteralExpression(lit) => literal_is_null(lit),
        ast::Expression::LambdaExpression(_)
        | ast::Expression::MethodReferenceExpression(_)
        | ast::Expression::ConstructorReferenceExpression(_)
        | ast::Expression::ArrayInitializer(_) => true,
        _ => false,
    }
}

fn literal_is_null(lit: &ast::LiteralExpression) -> bool {
    let tok = lit
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof);

    matches!(tok.map(|t| t.kind()), Some(SyntaxKind::NullKw))
}

fn infer_type_from_literal(lit: &ast::LiteralExpression) -> String {
    let tok = lit
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof);
    let Some(tok) = tok else {
        return "Object".to_string();
    };

    match tok.kind() {
        SyntaxKind::IntLiteral => "int".to_string(),
        SyntaxKind::LongLiteral => "long".to_string(),
        SyntaxKind::FloatLiteral => "float".to_string(),
        SyntaxKind::DoubleLiteral => "double".to_string(),
        SyntaxKind::CharLiteral => "char".to_string(),
        SyntaxKind::StringLiteral | SyntaxKind::TextBlock => "String".to_string(),
        SyntaxKind::TrueKw | SyntaxKind::FalseKw => "boolean".to_string(),
        SyntaxKind::NullKw => "Object".to_string(),
        _ => "Object".to_string(),
    }
}

fn render_java_type(node: &nova_syntax::SyntaxNode) -> String {
    // We want Java-source-like but stable output. We therefore drop trivia and insert spaces only
    // when necessary for the token stream to remain readable/valid.
    let mut out = String::new();
    let mut prev_kind: Option<SyntaxKind> = None;
    let mut prev_was_word = false;

    for tok in node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
    {
        let kind = tok.kind();
        if kind.is_trivia() || kind == SyntaxKind::Eof {
            continue;
        }

        let is_word = kind.is_keyword() || kind.is_identifier_like();
        let needs_space = !out.is_empty()
            && ((prev_was_word && is_word)
                || (prev_kind == Some(SyntaxKind::Question) && is_word)
                || (kind == SyntaxKind::At
                    && (prev_was_word || prev_kind == Some(SyntaxKind::RBracket))));

        if needs_space {
            out.push(' ');
        }
        out.push_str(tok.text());
        prev_kind = Some(kind);
        prev_was_word = is_word;
    }

    if out.is_empty() {
        "Object".to_string()
    } else {
        out
    }
}

fn current_indent(text: &str, line_start: usize) -> String {
    let line = &text[line_start..];
    let mut indent = String::new();
    for ch in line.chars() {
        if ch == ' ' || ch == '\t' {
            indent.push(ch);
        } else {
            break;
        }
    }
    indent
}

fn typeck_ty_is_qualified_version_of(parser_ty: &str, typeck_ty: &str) -> bool {
    if typeck_ty == parser_ty {
        return true;
    }

    if typeck_ty.len() <= parser_ty.len() {
        return false;
    }
    if !typeck_ty.ends_with(parser_ty) {
        return false;
    }

    // Only treat as a "qualified version" when the match is on a segment boundary.
    let boundary = typeck_ty.len() - parser_ty.len() - 1;
    if typeck_ty.as_bytes().get(boundary) != Some(&b'.') {
        return false;
    }

    // Heuristic: treat `pkg.Foo` as a redundant qualification of `Foo`, but keep nested-type
    // qualifications like `Outer.Foo`.
    //
    // This is intentionally conservative: we only consider it "package-qualified" when the first
    // segment starts with a lowercase ASCII letter.
    let qualifier = &typeck_ty[..boundary];
    qualifier
        .as_bytes()
        .first()
        .copied()
        .is_some_and(|b| b.is_ascii_lowercase())
}

fn best_type_at_range_display(
    db: &dyn RefactorDatabase,
    file: &FileId,
    text: &str,
    range: TextRange,
) -> Option<String> {
    for offset in type_at_range_offset_candidates(text, range) {
        let Some(ty) = db.type_at_offset_display(file, offset) else {
            continue;
        };
        let ty = ty.trim();
        // `type_at_offset_display` can return values that are not valid Java source types (e.g.
        // Nova placeholders like `<?>`/`<error>`, or `null`/`void` for literals). Filter those out
        // so we don't emit uncompilable declarations like `null value = null;`.
        if ty.is_empty()
            || ty == "<?>"
            || ty == "<?>" // Legacy placeholder.
            || ty == "<error>"
            || ty.eq_ignore_ascii_case("null")
            || ty == "void"
            || ty.starts_with('<')
            // Wildcard types (`? extends Foo`) and intersection types (`A & B`) are not denotable
            // as standalone variable declaration types.
            || ty.starts_with('?')
            || ty.contains(" & ")
        {
            continue;
        }
        return Some(ty.to_string());
    }
    None
}

fn type_at_range_offset_candidates(text: &str, range: TextRange) -> Vec<usize> {
    let bytes = text.as_bytes();
    if range.start >= range.end || range.start >= bytes.len() {
        return Vec::new();
    }

    let mut start = range.start;
    let mut end = range.end.min(bytes.len());
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }

    if start >= end {
        return Vec::new();
    }

    let mut candidates: Vec<(usize, u8, usize)> = Vec::new();
    let mut depth = 0usize;
    for i in start..end {
        let b = bytes[i];
        if !b.is_ascii_whitespace() && !is_java_ident_byte(b) {
            candidates.push((i, b, depth));
        }

        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    let mut offsets: Vec<usize> = Vec::new();
    if let Some(best) = pick_best_punctuation_offset(&candidates) {
        offsets.push(best);
    }
    offsets.push(start);
    if start + 1 < end {
        offsets.push(start + 1);
    }
    offsets.push(end.saturating_sub(1));

    // De-dup while preserving order.
    let mut seen: HashSet<usize> = HashSet::new();
    offsets.retain(|o| seen.insert(*o));
    offsets
}

fn pick_best_punctuation_offset(candidates: &[(usize, u8, usize)]) -> Option<usize> {
    let min_depth = candidates.iter().map(|(_, _, depth)| *depth).min()?;

    let mut last_any: Option<usize> = None;
    let mut last_non_open: Option<usize> = None;

    for &(idx, b, depth) in candidates {
        if depth != min_depth {
            continue;
        }

        last_any = Some(idx);
        if !matches!(b, b'(' | b'[' | b'{') {
            last_non_open = Some(idx);
        }
    }

    last_non_open.or(last_any)
}

fn is_java_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

#[derive(Debug)]
struct LocalVarDeclInfo {
    statement: ast::LocalVariableDeclarationStatement,
    statement_range: TextRange,
    declarator_delete_range: Option<TextRange>,
    other_initializers_have_side_effects: bool,
    initializer: ast::Expression,
}

fn find_local_variable_declaration(
    db: &dyn RefactorDatabase,
    file: &FileId,
    symbol: SymbolId,
    root: &nova_syntax::SyntaxNode,
    name_range: TextRange,
) -> Option<LocalVarDeclInfo> {
    fn info_for_statement(
        db: &dyn RefactorDatabase,
        file: &FileId,
        symbol: SymbolId,
        stmt: ast::LocalVariableDeclarationStatement,
        name_range: TextRange,
    ) -> Option<LocalVarDeclInfo> {
        let list = stmt.declarator_list()?;
        let declarators: Vec<_> = list.declarators().collect();

        // Be tolerant of small range mismatches: some symbol sources (HIR, ad-hoc scanners) may
        // include surrounding trivia in the "name range". Match by overlap against the actual
        // identifier token span.
        let mut target_idx = declarators.iter().position(|decl| {
            decl.name_token()
                .map(|tok| {
                    let range = syntax_token_range(&tok);
                    range.start < name_range.end && name_range.start < range.end
                })
                .unwrap_or(false)
        });

        if target_idx.is_none() {
            // Fallback: match by semantic symbol id when range matching fails.
            target_idx = declarators.iter().position(|decl| {
                let Some(tok) = decl.name_token() else {
                    return false;
                };
                let range = syntax_token_range(&tok);
                db.symbol_at(file, range.start)
                    .is_some_and(|sym| sym == symbol)
            });
        }

        let target_idx = target_idx?;
        let decl = declarators.get(target_idx)?.clone();
        let initializer = decl.initializer()?;
        let statement_range = syntax_range(stmt.syntax());

        let declarator_delete_range = if declarators.len() <= 1 {
            None
        } else {
            // When deleting a single declarator out of a multi-declarator statement (`int a = 1, b
            // = 2;`), we want to preserve comments that are adjacent to the *remaining* declarators.
            //
            // Using a simple trimmed range (`decl..next`) would delete any comment between the
            // comma and `next` because comments are trivia.
            let comma_tokens: Vec<_> = list
                .syntax()
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|tok| tok.kind() == SyntaxKind::Comma)
                .collect();

            let comma_for_target: Option<nova_syntax::SyntaxToken> =
                if comma_tokens.len() == declarators.len().saturating_sub(1) {
                    if target_idx + 1 < declarators.len() {
                        comma_tokens.get(target_idx).cloned()
                    } else if target_idx > 0 {
                        comma_tokens.get(target_idx - 1).cloned()
                    } else {
                        None
                    }
                } else {
                    None
                };

            if let Some(comma_tok) = comma_for_target {
                if target_idx + 1 < declarators.len() {
                    // Remove `decl,` (and *only* the whitespace directly after the comma) when a
                    // next declarator exists. Stop before comments so they stay attached to the
                    // next declarator.
                    let start = trimmed_syntax_range(decl.syntax()).start;
                    let mut end = syntax_token_range(&comma_tok).end;
                    let mut tok = comma_tok;
                    while let Some(next_tok) = tok.next_token() {
                        if next_tok.kind() == SyntaxKind::Whitespace {
                            end = syntax_token_range(&next_tok).end;
                            tok = next_tok;
                        } else {
                            break;
                        }
                    }
                    Some(TextRange::new(start, end))
                } else if target_idx > 0 {
                    // Remove `, decl` (and whitespace directly before the comma) when this is the
                    // last declarator. Stop before comments so they stay attached to the previous
                    // declarator.
                    let end = trimmed_syntax_range(decl.syntax()).end;
                    let mut start = syntax_token_range(&comma_tok).start;
                    let mut tok = comma_tok;
                    while let Some(prev_tok) = tok.prev_token() {
                        if prev_tok.kind() == SyntaxKind::Whitespace {
                            start = syntax_token_range(&prev_tok).start;
                            tok = prev_tok;
                        } else {
                            break;
                        }
                    }
                    Some(TextRange::new(start, end))
                } else {
                    None
                }
            } else if let Some(next) = declarators.get(target_idx + 1) {
                // Fallback: remove `decl, <trivia>` up to the next declarator.
                let start = trimmed_syntax_range(decl.syntax()).start;
                let end = trimmed_syntax_range(next.syntax()).start;
                Some(TextRange::new(start, end))
            } else if target_idx > 0 {
                // Fallback: remove `<trivia>, decl` from the previous declarator end.
                let prev = declarators.get(target_idx - 1)?;
                let start = trimmed_syntax_range(prev.syntax()).end;
                let end = trimmed_syntax_range(decl.syntax()).end;
                Some(TextRange::new(start, end))
            } else {
                None
            }
        };

        let other_initializers_have_side_effects = declarators
            .iter()
            .enumerate()
            .filter(|(idx, _)| *idx != target_idx)
            .filter_map(|(_, decl)| decl.initializer())
            .any(|expr| has_side_effects(expr.syntax()));

        Some(LocalVarDeclInfo {
            statement: stmt,
            statement_range,
            declarator_delete_range,
            other_initializers_have_side_effects,
            initializer,
        })
    }

    // Fast path: scan all local variable declaration statements and match on the declarator token
    // range. (Avoid using `?` inside the loop: some malformed/incomplete statements can omit
    // subtrees, and we still want to keep searching.)
    for stmt in root
        .descendants()
        .filter_map(ast::LocalVariableDeclarationStatement::cast)
    {
        if let Some(info) = info_for_statement(db, file, symbol, stmt, name_range) {
            return Some(info);
        }
    }

    // Fallback: locate the identifier token by range and walk up. This is more robust in the
    // presence of parser quirks where the statement cast exists but the declarator list is missing
    // or trivia shifts statement boundaries.
    let tok = root
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| {
            tok.kind() == SyntaxKind::Identifier && {
                let range = syntax_token_range(tok);
                range.start <= name_range.start && name_range.end <= range.end
            }
        })?;

    let stmt = tok.parent().and_then(|node| {
        node.ancestors()
            .find_map(ast::LocalVariableDeclarationStatement::cast)
    })?;

    info_for_statement(db, file, symbol, stmt, name_range)
}

fn syntax_token_range(tok: &nova_syntax::SyntaxToken) -> TextRange {
    let range = tok.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn trimmed_syntax_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    for el in node.descendants_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        if tok.kind().is_trivia() {
            continue;
        }
        let range = tok.text_range();
        let tok_start = u32::from(range.start()) as usize;
        let tok_end = u32::from(range.end()) as usize;
        if start.is_none() {
            start = Some(tok_start);
        }
        end = Some(tok_end);
    }

    match (start, end) {
        (Some(start), Some(end)) => TextRange::new(start, end),
        _ => syntax_range(node),
    }
}

/// Compute the byte range to replace when rewriting an extracted expression occurrence.
///
/// Some expression node ranges include trailing trivia (whitespace/comments). For extraction we:
/// - Use the token span (see [`trimmed_syntax_range`]) for the extracted expression text so we
///   don't move trailing inline comments into the new declaration.
/// - Use a slightly wider range for replacement so we keep legacy behavior of trimming stray
///   whitespace after the expression (e.g. before `;`) when there is *no* trailing comment.
///
/// When a trailing comment is present, we preserve it (and any preceding whitespace) by limiting
/// the replacement to the token span.
fn expr_replacement_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let full = syntax_range(node);
    let token = trimmed_syntax_range(node);

    // Always preserve leading trivia by starting replacement at the first non-trivia token.
    let start = token.start;

    // If trailing trivia contains a comment token, avoid replacing it (keep it at the usage site).
    let mut has_trailing_comment = false;
    for el in node.descendants_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        if !tok.kind().is_trivia() {
            continue;
        }

        let range = tok.text_range();
        let tok_start = u32::from(range.start()) as usize;

        // Only consider trivia that occurs *after* the last non-trivia token.
        if tok_start < token.end {
            continue;
        }

        if matches!(
            tok.kind(),
            SyntaxKind::LineComment | SyntaxKind::BlockComment | SyntaxKind::DocComment
        ) {
            has_trailing_comment = true;
            break;
        }
    }

    let end = if has_trailing_comment {
        token.end
    } else {
        full.end
    };

    TextRange::new(start, end)
}

fn reject_unsafe_extract_variable_context(
    expr: &ast::Expression,
    enclosing_stmt: &ast::Statement,
) -> Result<(), RefactorError> {
    let expr_range = syntax_range(expr.syntax());
    let enclosing_stmt_syntax = enclosing_stmt.syntax().clone();

    for ancestor in expr.syntax().ancestors() {
        if let Some(while_stmt) = ast::WhileStatement::cast(ancestor.clone()) {
            if let Some(cond) = while_stmt.condition() {
                if contains_range(syntax_range(cond.syntax()), expr_range) {
                    return Err(RefactorError::ExtractNotSupported {
                        reason: "cannot extract from while condition",
                    });
                }
            }
        }

        if let Some(do_while) = ast::DoWhileStatement::cast(ancestor.clone()) {
            if let Some(cond) = do_while.condition() {
                if contains_range(syntax_range(cond.syntax()), expr_range) {
                    return Err(RefactorError::ExtractNotSupported {
                        reason: "cannot extract from do-while condition",
                    });
                }
            }
        }

        if let Some(for_stmt) = ast::ForStatement::cast(ancestor.clone()) {
            if let Some(header) = for_stmt.header() {
                if for_header_has_unsafe_eval_context(&header, expr_range) {
                    return Err(RefactorError::ExtractNotSupported {
                        reason: "cannot extract from for-loop condition or update",
                    });
                }
            }
        }

        if let Some(binary) = ast::BinaryExpression::cast(ancestor.clone()) {
            if let Some(op) = binary_short_circuit_operator_kind(&binary) {
                if matches!(op, SyntaxKind::AmpAmp | SyntaxKind::PipePipe) {
                    if let Some(rhs) = binary.rhs() {
                        if contains_range(syntax_range(rhs.syntax()), expr_range) {
                            return Err(RefactorError::ExtractNotSupported {
                                reason: "cannot extract from right-hand side of `&&` / `||`",
                            });
                        }
                    }
                }
            }
        }

        if let Some(cond_expr) = ast::ConditionalExpression::cast(ancestor.clone()) {
            let cond_range = syntax_range(cond_expr.syntax());
            // Allow extracting the whole conditional expression.
            if cond_range != expr_range {
                if let Some(then_branch) = cond_expr.then_branch() {
                    if contains_range(syntax_range(then_branch.syntax()), expr_range) {
                        return Err(RefactorError::ExtractNotSupported {
                            reason: "cannot extract from conditional (`?:`) branch",
                        });
                    }
                }
                if let Some(else_branch) = cond_expr.else_branch() {
                    if contains_range(syntax_range(else_branch.syntax()), expr_range) {
                        return Err(RefactorError::ExtractNotSupported {
                            reason: "cannot extract from conditional (`?:`) branch",
                        });
                    }
                }
            }
        }

        if ancestor == enclosing_stmt_syntax {
            break;
        }
    }

    Ok(())
}

fn reject_extract_variable_eval_order_guard(
    source: &str,
    selection: TextRange,
    expr: &ast::Expression,
    enclosing_stmt: &ast::Statement,
) -> Result<(), RefactorError> {
    let selection = trim_range(source, selection);
    let expr_range = trim_range(source, syntax_range(expr.syntax()));
    if selection.len() == 0 || expr_range.len() == 0 {
        return Ok(());
    }

    // When we extract an expression into a fresh statement, we evaluate it *before* the enclosing
    // statement. This can change the relative order of side effects and thrown exceptions with
    // respect to other expressions.
    //
    // Be conservative and reject when there are other side-effectful expressions outside the
    // selection in the portion of the statement that's evaluated as part of the "header".
    //
    // Important: for statements with nested bodies (`if`, `switch`, `synchronized`), extracting a
    // header expression does *not* reorder side effects inside the body. We therefore restrict
    // scanning to the header expression when the selection is inside it.
    //
    // Similarly, when extracting a `switch` *expression* selector, we can ignore side effects
    // inside the switch body since they're evaluated after the selector.
    let scan_root = eval_order_guard_scan_root(source, enclosing_stmt, expr_range);
    let excluded_ranges = eval_order_guard_excluded_ranges(source, expr, expr_range);
    if has_order_sensitive_expr_outside_selection(source, &scan_root, expr_range, &excluded_ranges)
    {
        return Err(RefactorError::ExtractNotSupported {
            reason: "cannot extract because it may change evaluation order",
        });
    }

    Ok(())
}

fn eval_order_guard_scan_root(
    source: &str,
    enclosing_stmt: &ast::Statement,
    expr_range: TextRange,
) -> nova_syntax::SyntaxNode {
    match enclosing_stmt {
        ast::Statement::LocalVariableDeclarationStatement(stmt) => stmt
            .declarator_list()
            .and_then(|list| {
                list.declarators().find_map(|decl| {
                    decl.initializer().and_then(|init| {
                        let init_range = trim_range(source, syntax_range(init.syntax()));
                        contains_range(init_range, expr_range).then(|| init.syntax().clone())
                    })
                })
            })
            .unwrap_or_else(|| enclosing_stmt.syntax().clone()),
        ast::Statement::IfStatement(stmt) => stmt
            .condition()
            .filter(|cond| {
                contains_range(trim_range(source, syntax_range(cond.syntax())), expr_range)
            })
            .map(|cond| cond.syntax().clone())
            .unwrap_or_else(|| enclosing_stmt.syntax().clone()),
        ast::Statement::SwitchStatement(stmt) => stmt
            .expression()
            .filter(|selector| {
                contains_range(
                    trim_range(source, syntax_range(selector.syntax())),
                    expr_range,
                )
            })
            .map(|selector| selector.syntax().clone())
            .unwrap_or_else(|| enclosing_stmt.syntax().clone()),
        ast::Statement::SynchronizedStatement(stmt) => stmt
            .expression()
            .filter(|lock_expr| {
                contains_range(
                    trim_range(source, syntax_range(lock_expr.syntax())),
                    expr_range,
                )
            })
            .map(|lock_expr| lock_expr.syntax().clone())
            .unwrap_or_else(|| enclosing_stmt.syntax().clone()),
        _ => enclosing_stmt.syntax().clone(),
    }
}

fn eval_order_guard_excluded_ranges(
    source: &str,
    expr: &ast::Expression,
    expr_range: TextRange,
) -> Vec<TextRange> {
    // Exclude switch *expression* bodies when extracting from the selector expression. The body is
    // evaluated after the selector, so hoisting a selector sub-expression does not reorder body
    // side effects.
    let mut excluded = Vec::new();

    for node in expr.syntax().ancestors() {
        let Some(switch_expr) = ast::SwitchExpression::cast(node.clone()) else {
            continue;
        };
        let Some(selector) = switch_expr.expression() else {
            continue;
        };
        if !contains_range(
            trim_range(source, syntax_range(selector.syntax())),
            expr_range,
        ) {
            continue;
        }
        if let Some(block) = switch_expr.block() {
            excluded.push(trim_range(source, syntax_range(block.syntax())));
        }
        break;
    }

    excluded
}

fn has_order_sensitive_expr_outside_selection(
    source: &str,
    scan_root: &nova_syntax::SyntaxNode,
    selection: TextRange,
    excluded_ranges: &[TextRange],
) -> bool {
    let selection = trim_range(source, selection);
    if selection.len() == 0 {
        return false;
    }

    scan_root
        .descendants()
        .filter_map(ast::Expression::cast)
        .any(|expr| {
            let range = trim_range(source, syntax_range(expr.syntax()));
            if excluded_ranges
                .iter()
                .any(|excluded| contains_range(*excluded, range))
            {
                return false;
            }
            ranges_disjoint(range, selection) && has_side_effects(expr.syntax())
        })
}

fn ranges_disjoint(a: TextRange, b: TextRange) -> bool {
    a.end <= b.start || b.end <= a.start
}
fn contains_range(outer: TextRange, inner: TextRange) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

fn for_header_has_unsafe_eval_context(header: &ast::ForHeader, expr_range: TextRange) -> bool {
    let mut semicolons = Vec::new();
    let mut r_paren = None;

    for el in header.syntax().children_with_tokens() {
        let Some(tok) = el.into_token() else {
            continue;
        };
        match tok.kind() {
            SyntaxKind::Semicolon => semicolons.push(tok),
            SyntaxKind::RParen => r_paren = Some(tok),
            _ => {}
        }
    }

    // Classic for loops always have two semicolons in the header.
    if semicolons.len() < 2 {
        return false;
    }
    let Some(r_paren) = r_paren else {
        return false;
    };

    let first_semi = syntax_token_range(&semicolons[0]);
    let second_semi = syntax_token_range(&semicolons[1]);
    let r_paren = syntax_token_range(&r_paren);

    let condition_segment = TextRange::new(first_semi.end, second_semi.start);
    let update_segment = TextRange::new(second_semi.end, r_paren.start);

    contains_range(condition_segment, expr_range) || contains_range(update_segment, expr_range)
}

fn binary_short_circuit_operator_kind(binary: &ast::BinaryExpression) -> Option<SyntaxKind> {
    let lhs = binary.lhs()?;
    let rhs = binary.rhs()?;

    let lhs = lhs.syntax().clone();
    let rhs = rhs.syntax().clone();
    let mut seen_lhs = false;
    for el in binary.syntax().children_with_tokens() {
        match el {
            nova_syntax::SyntaxElement::Node(node) => {
                if node == lhs {
                    seen_lhs = true;
                    continue;
                }
                if node == rhs {
                    break;
                }
            }
            nova_syntax::SyntaxElement::Token(tok) => {
                if !seen_lhs {
                    continue;
                }
                if tok.kind().is_trivia() {
                    continue;
                }
                return Some(tok.kind());
            }
        }
    }

    None
}

fn has_side_effects(expr: &nova_syntax::SyntaxNode) -> bool {
    fn node_has_side_effects(node: &nova_syntax::SyntaxNode) -> bool {
        matches!(
            node.kind(),
            SyntaxKind::MethodCallExpression
                | SyntaxKind::NewExpression
                | SyntaxKind::ArrayCreationExpression
                | SyntaxKind::AssignmentExpression
                | SyntaxKind::LambdaExpression
        )
    }

    if node_has_side_effects(expr) || expr.descendants().any(|node| node_has_side_effects(&node)) {
        return true;
    }

    // Include ++/-- (both prefix and postfix) as side effects.
    expr.descendants_with_tokens()
        .any(|el| matches!(el.kind(), SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
}

/// Conservative predicate for whether an initializer expression is "order sensitive".
///
/// Even expressions that are otherwise side-effect-free can still be order-sensitive because they
/// may throw exceptions (e.g. NPE, AIOOBE, ClassCastException). When inlining deletes the
/// declaration, the initializer can be moved later in the block, changing when (or if) it throws.
fn initializer_is_order_sensitive(expr: &nova_syntax::SyntaxNode) -> bool {
    fn node_is_order_sensitive(node: &nova_syntax::SyntaxNode) -> bool {
        matches!(
            node.kind(),
            SyntaxKind::MethodCallExpression
                | SyntaxKind::NewExpression
                | SyntaxKind::AssignmentExpression
                | SyntaxKind::FieldAccessExpression
                | SyntaxKind::ArrayAccessExpression
                | SyntaxKind::CastExpression
                | SyntaxKind::ArrayCreationExpression
        )
    }

    if node_is_order_sensitive(expr)
        || expr
            .descendants()
            .any(|node| node_is_order_sensitive(&node))
    {
        return true;
    }

    // `++`/`--` are always order sensitive.
    expr.descendants_with_tokens()
        .any(|el| matches!(el.kind(), SyntaxKind::PlusPlus | SyntaxKind::MinusMinus))
}

fn parenthesize_initializer(text: &str, expr: &ast::Expression) -> String {
    if matches!(expr, ast::Expression::ParenthesizedExpression(_)) {
        return text.to_string();
    }

    let is_simple_primary = matches!(
        expr,
        ast::Expression::NameExpression(_)
            | ast::Expression::LiteralExpression(_)
            | ast::Expression::ThisExpression(_)
            | ast::Expression::SuperExpression(_)
            | ast::Expression::NewExpression(_)
            | ast::Expression::MethodCallExpression(_)
            | ast::Expression::FieldAccessExpression(_)
            | ast::Expression::ArrayAccessExpression(_)
    );

    if is_simple_primary {
        text.to_string()
    } else {
        format!("({text})")
    }
}

fn statement_end_including_trailing_newline(text: &str, stmt_end: usize) -> usize {
    let mut offset = stmt_end.min(text.len());

    // Consume trailing spaces/tabs at end-of-line so we don't leave whitespace-only lines behind.
    while offset < text.len() {
        match text.as_bytes()[offset] {
            b' ' | b'\t' => offset += 1,
            _ => break,
        }
    }

    let newline = NewlineStyle::detect(text);
    let newline_str = newline.as_str();

    if text
        .get(offset..)
        .unwrap_or_default()
        .starts_with(newline_str)
    {
        return offset + newline_str.len();
    }

    // Mixed-newline fallback.
    if text.get(offset..).unwrap_or_default().starts_with("\r\n") {
        return offset + 2;
    }
    if text.get(offset..).unwrap_or_default().starts_with('\n') {
        return offset + 1;
    }
    if text.get(offset..).unwrap_or_default().starts_with('\r') {
        return offset + 1;
    }

    offset
}

fn statement_end_including_trailing_inline_comment(text: &str, stmt_end: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut cursor = stmt_end.min(bytes.len());
    let line_end = line_break_start(text, cursor);
    let mut end = stmt_end;
    let mut saw_comment = false;

    loop {
        while cursor < line_end && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }

        if cursor + 1 >= line_end {
            break;
        }

        if bytes[cursor] == b'/' && bytes[cursor + 1] == b'/' {
            // Line comment: delete to (but not including) the line break.
            saw_comment = true;
            end = line_end;
            break;
        }

        if bytes[cursor] == b'/' && bytes[cursor + 1] == b'*' {
            let mut i = cursor + 2;
            let mut found = None;
            while i + 1 < line_end {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    found = Some(i + 2);
                    break;
                }
                i += 1;
            }
            let Some(comment_end) = found else {
                break;
            };
            saw_comment = true;
            end = comment_end;
            cursor = comment_end;
            continue;
        }

        break;
    }

    if !saw_comment {
        return None;
    }

    // If the trailing comment reached the end of the line (modulo whitespace), also delete the
    // remaining whitespace so we don't leave a whitespace-only line tail behind.
    let mut i = end.min(bytes.len());
    while i < line_end && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    if i == line_end {
        end = line_end;
    }

    Some(end)
}

fn line_break_start(text: &str, offset: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = offset.min(bytes.len());
    while i < bytes.len() {
        if bytes[i] == b'\n' || bytes[i] == b'\r' {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

fn find_innermost_statement_containing_range(
    root: &nova_syntax::SyntaxNode,
    range: TextRange,
) -> Option<ast::Statement> {
    root.descendants()
        .filter_map(ast::Statement::cast)
        .filter(|stmt| {
            let stmt_range = syntax_range(stmt.syntax());
            stmt_range.start <= range.start && range.end <= stmt_range.end
        })
        .min_by_key(|stmt| syntax_range(stmt.syntax()).len())
}

fn statement_list_container_and_index(
    stmt: &ast::Statement,
) -> Option<(nova_syntax::SyntaxNode, usize)> {
    let parent = stmt.syntax().parent()?;

    if let Some(block) = ast::Block::cast(parent.clone()) {
        let idx = block
            .statements()
            .position(|candidate| candidate.syntax() == stmt.syntax())?;
        return Some((block.syntax().clone(), idx));
    }

    if let Some(block) = ast::SwitchBlock::cast(parent.clone()) {
        let idx = block
            .statements()
            .position(|candidate| candidate.syntax() == stmt.syntax())?;
        return Some((block.syntax().clone(), idx));
    }

    if let Some(group) = ast::SwitchGroup::cast(parent) {
        let idx = group
            .statements()
            .position(|candidate| candidate.syntax() == stmt.syntax())?;
        return Some((group.syntax().clone(), idx));
    }

    None
}

fn check_order_sensitive_inline_order(
    root: &nova_syntax::SyntaxNode,
    decl_stmt: &ast::LocalVariableDeclarationStatement,
    targets: &[crate::semantic::Reference],
    decl_file: &FileId,
) -> Result<(), RefactorError> {
    let decl_stmt = ast::Statement::cast(decl_stmt.syntax().clone())
        .ok_or(RefactorError::InlineNotSupported)?;
    let (decl_container, decl_index) =
        statement_list_container_and_index(&decl_stmt).ok_or(RefactorError::InlineNotSupported)?;

    let mut earliest_usage_index: Option<usize> = None;
    for target in targets {
        // The statement-order check only supports analyzing the declaration file.
        if &target.file != decl_file {
            return Err(RefactorError::InlineNotSupported);
        }

        let usage_stmt = find_innermost_statement_containing_range(root, target.range)
            .ok_or(RefactorError::InlineNotSupported)?;
        let (usage_container, usage_index) = statement_list_container_and_index(&usage_stmt)
            .ok_or(RefactorError::InlineNotSupported)?;

        if usage_container != decl_container {
            return Err(RefactorError::InlineNotSupported);
        }

        earliest_usage_index = Some(match earliest_usage_index {
            Some(existing) => existing.min(usage_index),
            None => usage_index,
        });
    }

    let Some(earliest) = earliest_usage_index else {
        return Err(RefactorError::InlineNotSupported);
    };

    match decl_index.checked_add(1) {
        Some(expected) if expected == earliest => Ok(()),
        _ => Err(RefactorError::InlineNotSupported),
    }
}

#[derive(Clone, Debug)]
struct ImportDecl {
    is_static: bool,
    path: String,
    trailing_comment: String,
}

impl ImportDecl {
    fn is_wildcard(&self) -> bool {
        self.path.ends_with(".*")
    }

    fn wildcard_package(&self) -> Option<&str> {
        self.path.strip_suffix(".*")
    }

    fn split_package_and_name(&self) -> Option<(&str, &str)> {
        self.path.rsplit_once('.')
    }

    fn simple_name(&self) -> Option<&str> {
        if self.is_wildcard() {
            return None;
        }
        self.split_package_and_name()
            .map(|(_, name)| name)
            .or(Some(self.path.as_str()))
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("import ");
        if self.is_static {
            out.push_str("static ");
        }
        out.push_str(&self.path);
        out.push(';');
        if !self.trailing_comment.is_empty() {
            out.push(' ');
            out.push_str(&self.trailing_comment);
        }
        out
    }
}

#[derive(Clone, Debug)]
struct ImportBlock {
    range: TextRange,
    imports: Vec<ImportDecl>,
}

fn parse_import_block(text: &str) -> ImportBlock {
    let mut scanner = JavaScanner::new(text);
    let mut stage = HeaderStage::BeforePackageOrImport;
    let mut imports: Vec<ImportDecl> = Vec::new();
    let mut first_import_start: Option<usize> = None;
    let mut last_import_line_end: Option<usize> = None;

    while let Some(token) = scanner.next_token() {
        match stage {
            HeaderStage::BeforePackageOrImport => match token.kind {
                TokenKind::Ident("package") => {
                    scanner.consume_until_semicolon();
                    stage = HeaderStage::AfterPackage;
                }
                TokenKind::Ident("import") => {
                    let start = token.start;
                    if first_import_start.is_none() {
                        first_import_start = Some(start);
                    }
                    if let Some((decl, end)) = scanner.parse_import_decl(start) {
                        last_import_line_end = Some(end);
                        imports.push(decl);
                        stage = HeaderStage::InImports;
                    } else {
                        break;
                    }
                }
                TokenKind::Ident(word) if is_declaration_start_keyword(word) => break,
                _ => {}
            },
            HeaderStage::AfterPackage => match token.kind {
                TokenKind::Ident("import") => {
                    let start = token.start;
                    if first_import_start.is_none() {
                        first_import_start = Some(start);
                    }
                    if let Some((decl, end)) = scanner.parse_import_decl(start) {
                        last_import_line_end = Some(end);
                        imports.push(decl);
                        stage = HeaderStage::InImports;
                    } else {
                        break;
                    }
                }
                TokenKind::Symbol('@') => break,
                TokenKind::Ident(word) if is_declaration_start_keyword(word) => break,
                _ => {}
            },
            HeaderStage::InImports => match token.kind {
                TokenKind::Ident("import") => {
                    let start = token.start;
                    if first_import_start.is_none() {
                        first_import_start = Some(start);
                    }
                    if let Some((decl, end)) = scanner.parse_import_decl(start) {
                        last_import_line_end = Some(end);
                        imports.push(decl);
                    } else {
                        break;
                    }
                }
                TokenKind::Symbol('@') => break,
                TokenKind::Ident(word) if is_declaration_start_keyword(word) => break,
                _ => break,
            },
        }
    }

    let Some(start) = first_import_start else {
        return ImportBlock {
            range: TextRange::new(0, 0),
            imports: Vec::new(),
        };
    };
    let last_end = last_import_line_end.unwrap_or(start);
    let end = first_non_whitespace(text, last_end);
    ImportBlock {
        range: TextRange::new(start, end),
        imports,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderStage {
    BeforePackageOrImport,
    AfterPackage,
    InImports,
}

fn is_declaration_start_keyword(keyword: &str) -> bool {
    matches!(
        keyword,
        "class"
            | "interface"
            | "enum"
            | "record"
            | "module"
            | "open"
            | "public"
            | "private"
            | "protected"
            | "abstract"
            | "final"
            | "strictfp"
    )
}

fn first_non_whitespace(text: &str, mut offset: usize) -> usize {
    let bytes = text.as_bytes();
    while offset < bytes.len() && (bytes[offset] as char).is_ascii_whitespace() {
        offset += 1;
    }
    offset
}

#[derive(Clone, Debug)]
struct Token<'a> {
    kind: TokenKind<'a>,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
enum TokenKind<'a> {
    Ident(&'a str),
    Symbol(char),
    DoubleColon,
    StringLiteral,
    CharLiteral,
}

struct JavaScanner<'a> {
    text: &'a str,
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> JavaScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            offset: 0,
        }
    }

    fn next_token(&mut self) -> Option<Token<'a>> {
        self.skip_trivia();
        if self.offset >= self.bytes.len() {
            return None;
        }

        let start = self.offset;
        let b = self.bytes[self.offset];

        if b == b':' && self.offset + 1 < self.bytes.len() && self.bytes[self.offset + 1] == b':' {
            self.offset += 2;
            return Some(Token {
                kind: TokenKind::DoubleColon,
                start,
                end: self.offset,
            });
        }

        let c = b as char;
        if is_ident_start(c) {
            self.offset += 1;
            while self.offset < self.bytes.len()
                && is_ident_continue(self.bytes[self.offset] as char)
            {
                self.offset += 1;
            }
            return Some(Token {
                kind: TokenKind::Ident(&self.text[start..self.offset]),
                start,
                end: self.offset,
            });
        }

        if c == '"' {
            self.consume_string_literal();
            return Some(Token {
                kind: TokenKind::StringLiteral,
                start,
                end: self.offset,
            });
        }

        if c == '\'' {
            self.consume_char_literal();
            return Some(Token {
                kind: TokenKind::CharLiteral,
                start,
                end: self.offset,
            });
        }

        self.offset += 1;
        Some(Token {
            kind: TokenKind::Symbol(c),
            start,
            end: self.offset,
        })
    }

    fn skip_trivia(&mut self) {
        while self.offset < self.bytes.len() {
            let b = self.bytes[self.offset];
            let c = b as char;
            if c.is_ascii_whitespace() {
                self.offset += 1;
                continue;
            }

            if b == b'/' && self.offset + 1 < self.bytes.len() {
                match self.bytes[self.offset + 1] {
                    b'/' => {
                        self.offset += 2;
                        while self.offset < self.bytes.len() && self.bytes[self.offset] != b'\n' {
                            self.offset += 1;
                        }
                        continue;
                    }
                    b'*' => {
                        self.offset += 2;
                        while self.offset + 1 < self.bytes.len() {
                            if self.bytes[self.offset] == b'*'
                                && self.bytes[self.offset + 1] == b'/'
                            {
                                self.offset += 2;
                                break;
                            }
                            self.offset += 1;
                        }
                        continue;
                    }
                    _ => {}
                }
            }

            break;
        }
    }

    fn consume_until_semicolon(&mut self) {
        while let Some(tok) = self.next_token() {
            if matches!(tok.kind, TokenKind::Symbol(';')) {
                break;
            }
        }
    }

    fn parse_import_decl(&mut self, _start: usize) -> Option<(ImportDecl, usize)> {
        let mut is_static = false;

        // The `import` keyword has already been consumed. Parse optional `static`.
        let mut tok = self.next_token()?;
        if matches!(tok.kind, TokenKind::Ident("static")) {
            is_static = true;
            tok = self.next_token()?;
        }

        let TokenKind::Ident(first) = tok.kind else {
            return None;
        };
        let mut path = first.to_string();

        loop {
            let tok = self.next_token()?;
            match tok.kind {
                TokenKind::Symbol('.') => {
                    let tok = self.next_token()?;
                    match tok.kind {
                        TokenKind::Ident(seg) => {
                            path.push('.');
                            path.push_str(seg);
                        }
                        TokenKind::Symbol('*') => {
                            path.push_str(".*");
                        }
                        _ => return None,
                    }
                }
                TokenKind::Symbol(';') => {
                    let (comment, line_end) = scan_trailing_comment(self.text, tok.end);
                    self.offset = line_end;
                    return Some((
                        ImportDecl {
                            is_static,
                            path,
                            trailing_comment: comment,
                        },
                        line_end,
                    ));
                }
                _ => return None,
            }
        }
    }

    fn consume_string_literal(&mut self) {
        // Handles both normal strings and Java text blocks (`"""..."""`).
        if self.offset + 2 < self.bytes.len()
            && self.bytes[self.offset] == b'"'
            && self.bytes[self.offset + 1] == b'"'
            && self.bytes[self.offset + 2] == b'"'
        {
            self.offset += 3;
            while self.offset + 2 < self.bytes.len() {
                if self.bytes[self.offset] == b'"'
                    && self.bytes[self.offset + 1] == b'"'
                    && self.bytes[self.offset + 2] == b'"'
                {
                    self.offset += 3;
                    break;
                }
                self.offset += 1;
            }
            return;
        }

        self.offset += 1;
        while self.offset < self.bytes.len() {
            let b = self.bytes[self.offset];
            if b == b'\\' {
                self.offset = (self.offset + 2).min(self.bytes.len());
                continue;
            }
            self.offset += 1;
            if b == b'"' {
                break;
            }
        }
    }

    fn consume_char_literal(&mut self) {
        self.offset += 1;
        while self.offset < self.bytes.len() {
            let b = self.bytes[self.offset];
            if b == b'\\' {
                self.offset = (self.offset + 2).min(self.bytes.len());
                continue;
            }
            self.offset += 1;
            if b == b'\'' {
                break;
            }
        }
    }
}

fn scan_trailing_comment(text: &str, mut offset: usize) -> (String, usize) {
    let bytes = text.as_bytes();
    let len = bytes.len();

    while offset < len {
        match bytes[offset] {
            b' ' | b'\t' | b'\r' => offset += 1,
            _ => break,
        }
    }

    let mut comment = String::new();
    if offset + 1 < len && bytes[offset] == b'/' {
        match bytes[offset + 1] {
            b'/' => {
                let line_end = bytes[offset..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|o| offset + o)
                    .unwrap_or(len);
                comment = text[offset..line_end].trim_end_matches('\r').to_string();
            }
            b'*' => {
                // Preserve single-line block comments; multi-line ones are uncommon here.
                let line_end = bytes[offset..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|o| offset + o)
                    .unwrap_or(len);
                comment = text[offset..line_end].trim_end_matches('\r').to_string();
            }
            _ => {}
        }
    }

    let line_end = bytes[offset..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|o| offset + o + 1)
        .unwrap_or(len);

    (comment, line_end)
}

#[derive(Default)]
struct IdentifierUsage {
    all: HashSet<String>,
    unqualified: HashSet<String>,
}

fn collect_identifier_usage(text: &str) -> IdentifierUsage {
    let mut usage = IdentifierUsage::default();
    let mut i = 0;
    let bytes = text.as_bytes();

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum PrevSig {
        Dot,
        DoubleColon,
        Other,
    }

    let mut prev = PrevSig::Other;
    while i < bytes.len() {
        let c = bytes[i] as char;

        if c == '"' {
            i = skip_string_literal(text, i);
            continue;
        }

        if c == '\'' {
            i = skip_char_literal(text, i);
            continue;
        }

        if c == '/' && i + 1 < bytes.len() {
            let next = bytes[i + 1] as char;
            if next == '/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if next == '*' {
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
        }

        if c == ':' && i + 1 < bytes.len() && bytes[i + 1] == b':' {
            prev = PrevSig::DoubleColon;
            i += 2;
            continue;
        }

        if c == '.' {
            prev = PrevSig::Dot;
            i += 1;
            continue;
        }

        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = &text[start..i];
            usage.all.insert(ident.to_string());
            if prev != PrevSig::Dot && prev != PrevSig::DoubleColon {
                usage.unqualified.insert(ident.to_string());
            }
            prev = PrevSig::Other;
            continue;
        }

        if !c.is_ascii_whitespace() {
            prev = PrevSig::Other;
        }
        i += 1;
    }

    usage
}

fn skip_string_literal(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    if i + 2 < bytes.len() && bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
        // Java text block.
        i += 3;
        while i + 2 < bytes.len() {
            if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                return i + 3;
            }
            i += 1;
        }
        return bytes.len();
    }

    i += 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        i += 1;
        if b == b'"' {
            break;
        }
    }
    i
}

fn skip_char_literal(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    i += 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        i += 1;
        if b == b'\'' {
            break;
        }
    }
    i
}

fn collect_declared_type_names(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut i = 0;
    let bytes = text.as_bytes();
    let mut prev_was_dot = false;
    let mut expect_name = false;

    while i < bytes.len() {
        let c = bytes[i] as char;

        if c == '"' {
            i = skip_string_literal(text, i);
            continue;
        }

        if c == '\'' {
            i = skip_char_literal(text, i);
            continue;
        }

        if c == '/' && i + 1 < bytes.len() {
            let next = bytes[i + 1] as char;
            if next == '/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if next == '*' {
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
        }

        if c == '.' {
            prev_was_dot = true;
            i += 1;
            continue;
        }

        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = &text[start..i];

            if expect_name {
                out.insert(ident.to_string());
                expect_name = false;
                prev_was_dot = false;
                continue;
            }

            if !prev_was_dot && matches!(ident, "class" | "interface" | "enum" | "record") {
                expect_name = true;
            }

            prev_was_dot = false;
            continue;
        }

        if !c.is_ascii_whitespace() {
            prev_was_dot = false;
        }
        i += 1;
    }

    out
}

fn collect_uncovered_type_identifiers(
    unqualified: &HashSet<String>,
    explicitly_imported: &HashSet<String>,
    declared_types: &HashSet<String>,
) -> HashSet<String> {
    let java_lang: HashSet<&'static str> = [
        "String",
        "Object",
        "Class",
        "Throwable",
        "Exception",
        "RuntimeException",
        "Error",
        "Integer",
        "Long",
        "Short",
        "Byte",
        "Boolean",
        "Character",
        "Double",
        "Float",
        "Void",
        "Math",
        "System",
    ]
    .into_iter()
    .collect();

    unqualified
        .iter()
        .filter(|ident| {
            let Some(first) = ident.chars().next() else {
                return false;
            };
            if !first.is_ascii_uppercase() {
                return false;
            }
            if ident.len() == 1 {
                // Likely a generic type parameter (`T`, `E`, ...).
                return false;
            }
            if java_lang.contains(ident.as_str()) {
                return false;
            }
            if declared_types.contains(*ident) {
                return false;
            }
            if explicitly_imported.contains(*ident) {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

// Keep the public re-exports in lib.rs tidy.
#[allow(dead_code)]
fn _apply_edit_to_file(
    text: &str,
    file: FileId,
    edits: Vec<TextEdit>,
) -> Result<String, RefactorError> {
    Ok(apply_text_edits(
        text,
        &edits
            .into_iter()
            .map(|mut e| {
                e.file = file.clone();
                e
            })
            .collect::<Vec<_>>(),
    )?)
}
