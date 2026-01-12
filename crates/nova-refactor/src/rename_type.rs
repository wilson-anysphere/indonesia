use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use nova_core::QualifiedName;
use nova_hir::ast_id::AstIdMap;
use nova_hir::ids::{
    AnnotationId, ClassId, ConstructorId, EnumId, InitializerId, InterfaceId, ItemId, MethodId,
    RecordId,
};
use nova_hir::item_tree::ItemTree;
use nova_hir::queries::HirDatabase;
use nova_resolve::{Resolver, TypeResolution, WorkspaceDefMap};
use nova_syntax::ast::{self, support, AstNode};
use nova_syntax::{JavaParseResult, SyntaxKind, SyntaxNode, SyntaxToken};
use thiserror::Error;

use crate::edit::{FileId, TextEdit, TextRange, WorkspaceEdit};

#[derive(Debug, Error)]
pub enum RenameTypeError {
    #[error("unknown file {0:?}")]
    UnknownFile(FileId),
    #[error("no type found at offset {offset} in {file:?}")]
    NoTypeAtOffset { file: FileId, offset: usize },
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameTypeParams {
    pub file: FileId,
    pub offset: usize,
    pub new_name: String,
}

#[derive(Debug)]
struct FileAnalysis {
    file: FileId,
    db_file: nova_core::FileId,
    parse: JavaParseResult,
    ast_id_map: AstIdMap,
    item_tree: Arc<ItemTree>,
    scopes: nova_resolve::ItemTreeScopeBuildResult,
}

#[derive(Debug)]
struct WorkspaceAnalysis {
    files: BTreeMap<FileId, FileAnalysis>,
    reverse_file_ids: HashMap<nova_core::FileId, FileId>,
    workspace_def_map: WorkspaceDefMap,
}

/// Rename a Java type (class/interface/enum/record/annotation) and update all references in the
/// provided workspace `files`.
///
/// This is currently a best-effort semantic rename:
/// - It resolves candidate references through Nova's scope graph + resolver.
/// - It updates type references expressed via `ast::Type` nodes.
/// - It also updates qualified `this`/`super` expressions (`Outer.this`, `Outer.super`), whose
///   qualifiers are *type names* but live on `ThisExpression.qualifier` / `SuperExpression.qualifier`
///   rather than in `ast::Type`.
pub fn rename_type(
    files: &BTreeMap<FileId, String>,
    params: RenameTypeParams,
) -> Result<WorkspaceEdit, RenameTypeError> {
    let analysis = WorkspaceAnalysis::new(files)?;
    let target = analysis.resolve_type_at_offset(&params.file, params.offset)?;

    let jdk = nova_jdk::JdkIndex::new();
    let resolver = Resolver::new(&jdk)
        // Allow fast-path fully-qualified lookups to consult workspace definitions.
        .with_classpath(&analysis.workspace_def_map)
        // Enable `TypeResolution::Source` results when a `TypeName` corresponds to workspace code.
        .with_workspace(&analysis.workspace_def_map);

    let mut edits = Vec::new();

    // 1) Rename the declaration itself.
    let decl_file = analysis
        .reverse_file_ids
        .get(&target.file())
        .cloned()
        .ok_or_else(|| RenameTypeError::UnknownFile(params.file.clone()))?;
    let decl_analysis = analysis
        .files
        .get(&decl_file)
        .ok_or_else(|| RenameTypeError::UnknownFile(decl_file.clone()))?;
    if let Some(range) = item_name_range(&decl_analysis.item_tree, target) {
        edits.push(TextEdit::replace(
            decl_file.clone(),
            TextRange::new(range.start, range.end),
            params.new_name.clone(),
        ));
    }

    // 2) Rename references across the workspace.
    for file in analysis.files.values() {
        edits.extend(collect_type_reference_edits(
            &resolver,
            file,
            target,
            &params.new_name,
        ));
        edits.extend(collect_qualified_this_super_edits(
            &resolver,
            file,
            target,
            &params.new_name,
        ));
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

impl WorkspaceAnalysis {
    fn new(files: &BTreeMap<FileId, String>) -> Result<Self, RenameTypeError> {
        let mut texts = HashMap::<nova_core::FileId, Arc<str>>::new();
        let mut reverse_file_ids = HashMap::<nova_core::FileId, FileId>::new();

        for (idx, (file, text)) in files.iter().enumerate() {
            let id = nova_core::FileId::from_raw(idx as u32);
            texts.insert(id, Arc::<str>::from(text.as_str()));
            reverse_file_ids.insert(id, file.clone());
        }

        let db = InMemoryHirDb { texts };

        // Build workspace type namespace.
        let mut workspace_def_map = WorkspaceDefMap::default();
        let mut analyses: BTreeMap<FileId, FileAnalysis> = BTreeMap::new();

        for (file, db_file) in reverse_file_ids
            .iter()
            .map(|(id, file)| (file.clone(), *id))
        {
            let text = db
                .texts
                .get(&db_file)
                .cloned()
                .unwrap_or_else(|| Arc::<str>::from(""));
            let parse = nova_syntax::parse_java(&text);
            let ast_id_map = AstIdMap::new(&parse.syntax());
            let item_tree = db.hir_item_tree(db_file);
            let scopes = nova_resolve::build_scopes_for_item_tree(db_file, &item_tree);

            let def_map = nova_resolve::DefMap::from_item_tree(db_file, &item_tree);
            workspace_def_map.extend_from_def_map(&def_map);

            analyses.insert(
                file.clone(),
                FileAnalysis {
                    file,
                    db_file,
                    parse,
                    ast_id_map,
                    item_tree,
                    scopes,
                },
            );
        }

        Ok(Self {
            files: analyses,
            reverse_file_ids,
            workspace_def_map,
        })
    }

    fn resolve_type_at_offset(
        &self,
        file: &FileId,
        offset: usize,
    ) -> Result<ItemId, RenameTypeError> {
        let analysis = self
            .files
            .get(file)
            .ok_or_else(|| RenameTypeError::UnknownFile(file.clone()))?;

        // 1) Prefer declarations (type name tokens).
        if let Some(item) = find_type_decl_at_offset(&analysis.item_tree, offset) {
            return Ok(item);
        }

        // 2) Otherwise, try resolving a type name at this offset.
        let token = analysis
            .parse
            .token_at_offset(offset as u32)
            .right_biased()
            .or_else(|| analysis.parse.token_at_offset(offset as u32).left_biased())
            .ok_or_else(|| RenameTypeError::NoTypeAtOffset {
                file: file.clone(),
                offset,
            })?;

        let Some(parent) = token.parent() else {
            return Err(RenameTypeError::NoTypeAtOffset {
                file: file.clone(),
                offset,
            });
        };

        // Prefer resolving a typed `Name` node (common for `ast::Type` and many other type
        // positions). Qualified `this` / `super` qualifiers are currently represented as a
        // `NameExpression` that may not contain an `ast::Name`, so handle those separately.
        let mut resolved: Option<(QualifiedName, SyntaxNode)> =
            parent.ancestors().find_map(|node| {
                let name = ast::Name::cast(node.clone())?;
                let (qname, _) = name_to_qname_and_last_token_range(&name)?;
                Some((qname, node))
            });

        if resolved.is_none() {
            let token_range = token.text_range();
            let token_start = u32::from(token_range.start());

            if let Some(expr) = parent.ancestors().find_map(ast::ThisExpression::cast) {
                if let Some(qual) = expr.qualifier() {
                    let qual_range = qual.syntax().text_range();
                    if token_start >= u32::from(qual_range.start())
                        && token_start <= u32::from(qual_range.end())
                    {
                        if let Some((qname, _)) = qualifier_expression_name(&qual) {
                            resolved = Some((qname, expr.syntax().clone()));
                        }
                    }
                }
            }
        }

        if resolved.is_none() {
            let token_range = token.text_range();
            let token_start = u32::from(token_range.start());

            if let Some(expr) = parent.ancestors().find_map(ast::SuperExpression::cast) {
                if let Some(qual) = expr.qualifier() {
                    let qual_range = qual.syntax().text_range();
                    if token_start >= u32::from(qual_range.start())
                        && token_start <= u32::from(qual_range.end())
                    {
                        if let Some((qname, _)) = qualifier_expression_name(&qual) {
                            resolved = Some((qname, expr.syntax().clone()));
                        }
                    }
                }
            }
        }

        let Some((qname, scope_node)) = resolved else {
            return Err(RenameTypeError::NoTypeAtOffset {
                file: file.clone(),
                offset,
            });
        };

        let scope = scope_for_node(
            &analysis.scopes,
            &analysis.ast_id_map,
            &scope_node,
            analysis.db_file,
        );

        let jdk = nova_jdk::JdkIndex::new();
        let resolver = Resolver::new(&jdk)
            .with_classpath(&self.workspace_def_map)
            .with_workspace(&self.workspace_def_map);

        let resolved = resolver.resolve_qualified_type_resolution_in_scope(
            &analysis.scopes.scopes,
            scope,
            &qname,
        );
        match resolved {
            Some(TypeResolution::Source(item)) => Ok(item),
            _ => Err(RenameTypeError::NoTypeAtOffset {
                file: file.clone(),
                offset,
            }),
        }
    }
}

struct InMemoryHirDb {
    texts: HashMap<nova_core::FileId, Arc<str>>,
}

impl HirDatabase for InMemoryHirDb {
    fn file_text(&self, file: nova_core::FileId) -> Arc<str> {
        self.texts
            .get(&file)
            .cloned()
            .unwrap_or_else(|| Arc::<str>::from(""))
    }
}

fn collect_type_reference_edits(
    resolver: &Resolver<'_>,
    file: &FileAnalysis,
    target: ItemId,
    new_name: &str,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();

    for ty in file
        .parse
        .syntax()
        .descendants()
        .filter_map(ast::Type::cast)
    {
        let Some(named) = ty.named() else {
            continue;
        };
        let Some(name) = support::child::<ast::Name>(named.syntax()) else {
            continue;
        };

        let Some((qname, last_range)) = name_to_qname_and_last_token_range(&name) else {
            continue;
        };

        let scope = scope_for_node(&file.scopes, &file.ast_id_map, name.syntax(), file.db_file);
        let resolved =
            resolver.resolve_qualified_type_resolution_in_scope(&file.scopes.scopes, scope, &qname);
        if matches!(resolved, Some(TypeResolution::Source(item)) if item == target) {
            edits.push(TextEdit::replace(
                file.file.clone(),
                last_range,
                new_name.to_string(),
            ));
        }
    }

    edits
}

fn collect_qualified_this_super_edits(
    resolver: &Resolver<'_>,
    file: &FileAnalysis,
    target: ItemId,
    new_name: &str,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    let root = file.parse.syntax();

    // Qualified `this` expressions (`Outer.this`).
    for expr in root.descendants().filter_map(ast::ThisExpression::cast) {
        let Some(qual) = expr.qualifier() else {
            continue;
        };
        let Some((qname, last_range)) = qualifier_expression_name(&qual) else {
            continue;
        };
        let scope = scope_for_node(&file.scopes, &file.ast_id_map, expr.syntax(), file.db_file);
        let resolved =
            resolver.resolve_qualified_type_resolution_in_scope(&file.scopes.scopes, scope, &qname);
        if matches!(resolved, Some(TypeResolution::Source(item)) if item == target) {
            edits.push(TextEdit::replace(
                file.file.clone(),
                last_range,
                new_name.to_string(),
            ));
        }
    }

    // Qualified `super` expressions (`Outer.super`).
    for expr in root.descendants().filter_map(ast::SuperExpression::cast) {
        let Some(qual) = expr.qualifier() else {
            continue;
        };
        let Some((qname, last_range)) = qualifier_expression_name(&qual) else {
            continue;
        };
        let scope = scope_for_node(&file.scopes, &file.ast_id_map, expr.syntax(), file.db_file);
        let resolved =
            resolver.resolve_qualified_type_resolution_in_scope(&file.scopes.scopes, scope, &qname);
        if matches!(resolved, Some(TypeResolution::Source(item)) if item == target) {
            edits.push(TextEdit::replace(
                file.file.clone(),
                last_range,
                new_name.to_string(),
            ));
        }
    }

    edits
}

fn qualifier_expression_name(expr: &ast::Expression) -> Option<(QualifiedName, TextRange)> {
    match expr {
        ast::Expression::NameExpression(it) => {
            // `NameExpression` does not currently expose a typed accessor for its name and the
            // tree shape may or may not include an explicit `ast::Name` child. Be defensive:
            // - prefer a `Name` node when present (gives trivia-free text + direct ident tokens)
            // - otherwise fall back to concatenating non-trivia direct child tokens
            if let Some(name) = it.syntax().children().find_map(ast::Name::cast) {
                return name_to_qname_and_last_token_range(&name);
            }

            syntax_to_qname_and_last_token_range(it.syntax())
        }
        _ => None,
    }
}

fn name_to_qname_and_last_token_range(name: &ast::Name) -> Option<(QualifiedName, TextRange)> {
    let text = name.text();
    if text.is_empty() || text.contains('*') {
        return None;
    }

    let last_ident = support::ident_tokens(name.syntax()).last()?;
    let last_range = token_text_range(&last_ident);

    Some((QualifiedName::from_dotted(&text), last_range))
}

fn syntax_to_qname_and_last_token_range(node: &SyntaxNode) -> Option<(QualifiedName, TextRange)> {
    let mut text = String::new();
    let mut last_ident: Option<SyntaxToken> = None;

    for tok in node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| !tok.kind().is_trivia())
    {
        if tok.kind().is_identifier_like() {
            last_ident = Some(tok.clone());
        }
        text.push_str(tok.text());
    }

    if text.is_empty() || text.contains('*') {
        return None;
    }

    let last_ident = last_ident?;
    let last_range = token_text_range(&last_ident);
    Some((QualifiedName::from_dotted(&text), last_range))
}

fn token_text_range(token: &SyntaxToken) -> TextRange {
    let range = token.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn scope_for_node(
    scopes: &nova_resolve::ItemTreeScopeBuildResult,
    ast_id_map: &AstIdMap,
    node: &SyntaxNode,
    file: nova_core::FileId,
) -> nova_resolve::ScopeId {
    for ancestor in node.ancestors() {
        match ancestor.kind() {
            SyntaxKind::MethodDeclaration => {
                if let Some(ast_id) = ast_id_map.ast_id(&ancestor) {
                    let id = MethodId::new(file, ast_id);
                    if let Some(scope) = scopes.method_scopes.get(&id) {
                        return *scope;
                    }
                }
            }
            SyntaxKind::ConstructorDeclaration | SyntaxKind::CompactConstructorDeclaration => {
                if let Some(ast_id) = ast_id_map.ast_id(&ancestor) {
                    let id = ConstructorId::new(file, ast_id);
                    if let Some(scope) = scopes.constructor_scopes.get(&id) {
                        return *scope;
                    }
                }
            }
            SyntaxKind::InitializerBlock => {
                if let Some(ast_id) = ast_id_map.ast_id(&ancestor) {
                    let id = InitializerId::new(file, ast_id);
                    if let Some(scope) = scopes.initializer_scopes.get(&id) {
                        return *scope;
                    }
                }
            }
            SyntaxKind::ClassDeclaration => {
                if let Some(ast_id) = ast_id_map.ast_id(&ancestor) {
                    let id = ItemId::Class(ClassId::new(file, ast_id));
                    if let Some(scope) = scopes.class_scopes.get(&id) {
                        return *scope;
                    }
                }
            }
            SyntaxKind::InterfaceDeclaration => {
                if let Some(ast_id) = ast_id_map.ast_id(&ancestor) {
                    let id = ItemId::Interface(InterfaceId::new(file, ast_id));
                    if let Some(scope) = scopes.class_scopes.get(&id) {
                        return *scope;
                    }
                }
            }
            SyntaxKind::EnumDeclaration => {
                if let Some(ast_id) = ast_id_map.ast_id(&ancestor) {
                    let id = ItemId::Enum(EnumId::new(file, ast_id));
                    if let Some(scope) = scopes.class_scopes.get(&id) {
                        return *scope;
                    }
                }
            }
            SyntaxKind::RecordDeclaration => {
                if let Some(ast_id) = ast_id_map.ast_id(&ancestor) {
                    let id = ItemId::Record(RecordId::new(file, ast_id));
                    if let Some(scope) = scopes.class_scopes.get(&id) {
                        return *scope;
                    }
                }
            }
            SyntaxKind::AnnotationTypeDeclaration => {
                if let Some(ast_id) = ast_id_map.ast_id(&ancestor) {
                    let id = ItemId::Annotation(AnnotationId::new(file, ast_id));
                    if let Some(scope) = scopes.class_scopes.get(&id) {
                        return *scope;
                    }
                }
            }
            _ => {}
        }
    }

    scopes.file_scope
}

fn find_type_decl_at_offset(tree: &ItemTree, offset: usize) -> Option<ItemId> {
    for &item in &tree.items {
        let id = item_to_item_id(item);
        if let Some(found) = find_type_decl_at_offset_in(tree, id, offset) {
            return Some(found);
        }
    }
    None
}

fn find_type_decl_at_offset_in(tree: &ItemTree, item: ItemId, offset: usize) -> Option<ItemId> {
    let name_range = item_name_range(tree, item)?;
    if offset >= name_range.start && offset <= name_range.end {
        return Some(item);
    }

    for member in item_members(tree, item) {
        if let nova_hir::item_tree::Member::Type(child) = *member {
            let child_id = item_to_item_id(child);
            if let Some(found) = find_type_decl_at_offset_in(tree, child_id, offset) {
                return Some(found);
            }
        }
    }

    None
}

fn item_name_range(tree: &ItemTree, item: ItemId) -> Option<nova_types::Span> {
    Some(match item {
        ItemId::Class(id) => tree.class(id).name_range,
        ItemId::Interface(id) => tree.interface(id).name_range,
        ItemId::Enum(id) => tree.enum_(id).name_range,
        ItemId::Record(id) => tree.record(id).name_range,
        ItemId::Annotation(id) => tree.annotation(id).name_range,
    })
}

fn item_members(tree: &ItemTree, item: ItemId) -> &[nova_hir::item_tree::Member] {
    match item {
        ItemId::Class(id) => &tree.class(id).members,
        ItemId::Interface(id) => &tree.interface(id).members,
        ItemId::Enum(id) => &tree.enum_(id).members,
        ItemId::Record(id) => &tree.record(id).members,
        ItemId::Annotation(id) => &tree.annotation(id).members,
    }
}

fn item_to_item_id(item: nova_hir::item_tree::Item) -> ItemId {
    match item {
        nova_hir::item_tree::Item::Class(id) => ItemId::Class(id),
        nova_hir::item_tree::Item::Interface(id) => ItemId::Interface(id),
        nova_hir::item_tree::Item::Enum(id) => ItemId::Enum(id),
        nova_hir::item_tree::Item::Record(id) => ItemId::Record(id),
        nova_hir::item_tree::Item::Annotation(id) => ItemId::Annotation(id),
    }
}
