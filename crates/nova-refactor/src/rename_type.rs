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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    text: String,
    range: TextRange,
}

/// Rename a Java type (class/interface/enum/record/annotation) and update all references in the
/// provided workspace `files`.
///
/// This is a best-effort semantic rename:
/// - It resolves candidate references through Nova's scope graph + resolver.
/// - It updates type references expressed via `ast::Type` nodes.
/// - It updates qualified type names by resolving *all* prefixes of a qualified name as potential
///   types. This ensures renaming an enclosing type updates `Outer.Inner` occurrences.
/// - It updates `ast::Name` nodes (imports/module directives/etc.), including `import static` owner
///   chains.
/// - It also updates qualified `this`/`super` expressions (`Outer.this`, `Outer.super`).
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
        edits.extend(collect_name_reference_edits(
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
        // positions). Qualified `this` / `super` qualifiers are represented as `NameExpression`s
        // that may not contain an `ast::Name`, so handle those separately.
        let token_start = u32::from(token.text_range().start()) as usize;

        let mut segments: Option<Vec<Segment>> = parent
            .ancestors()
            .find_map(ast::Name::cast)
            .map(|name| segments_from_name(&name));
        let mut scope_node: Option<SyntaxNode> = parent
            .ancestors()
            .find_map(ast::Name::cast)
            .map(|name| name.syntax().clone());

        if segments.is_none() {
            if let Some(expr) = parent.ancestors().find_map(ast::ThisExpression::cast) {
                if let Some(qual) = expr.qualifier() {
                    let qual_range = qual.syntax().text_range();
                    let start = u32::from(qual_range.start()) as usize;
                    let end = u32::from(qual_range.end()) as usize;
                    if token_start >= start && token_start <= end {
                        segments = segments_from_qualifier_expression(&qual);
                        scope_node = Some(expr.syntax().clone());
                    }
                }
            }
        }

        if segments.is_none() {
            if let Some(expr) = parent.ancestors().find_map(ast::SuperExpression::cast) {
                if let Some(qual) = expr.qualifier() {
                    let qual_range = qual.syntax().text_range();
                    let start = u32::from(qual_range.start()) as usize;
                    let end = u32::from(qual_range.end()) as usize;
                    if token_start >= start && token_start <= end {
                        segments = segments_from_qualifier_expression(&qual);
                        scope_node = Some(expr.syntax().clone());
                    }
                }
            }
        }

        let Some(segments) = segments else {
            return Err(RenameTypeError::NoTypeAtOffset {
                file: file.clone(),
                offset,
            });
        };

        let Some(scope_node) = scope_node else {
            return Err(RenameTypeError::NoTypeAtOffset {
                file: file.clone(),
                offset,
            });
        };

        let Some(seg_idx) = segment_index_at_offset(&segments, offset) else {
            return Err(RenameTypeError::NoTypeAtOffset {
                file: file.clone(),
                offset,
            });
        };

        let prefix_qname = prefix_qname(&segments[..=seg_idx]);

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
            &prefix_qname,
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

        // Prefer a `Name` child when available (gives stable token boundaries).
        let segments = support::child::<ast::Name>(named.syntax())
            .map(|name| segments_from_name(&name))
            .unwrap_or_else(|| segments_from_syntax(named.syntax()));

        if segments.is_empty() {
            continue;
        }

        let scope = scope_for_node(&file.scopes, &file.ast_id_map, ty.syntax(), file.db_file);
        record_qualified_type_prefix_matches(
            resolver,
            &file.scopes.scopes,
            scope,
            &segments,
            target,
            &mut edits,
            &file.file,
            new_name,
        );
    }

    edits
}

fn collect_name_reference_edits(
    resolver: &Resolver<'_>,
    file: &FileAnalysis,
    target: ItemId,
    new_name: &str,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();

    for name in file
        .parse
        .syntax()
        .descendants()
        .filter_map(ast::Name::cast)
    {
        let segments = segments_from_name(&name);
        if segments.is_empty() {
            continue;
        }

        let scope = scope_for_node(&file.scopes, &file.ast_id_map, name.syntax(), file.db_file);
        record_qualified_type_prefix_matches(
            resolver,
            &file.scopes.scopes,
            scope,
            &segments,
            target,
            &mut edits,
            &file.file,
            new_name,
        );
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
        let Some(segments) = segments_from_qualifier_expression(&qual) else {
            continue;
        };
        if segments.is_empty() {
            continue;
        }

        let scope = scope_for_node(&file.scopes, &file.ast_id_map, expr.syntax(), file.db_file);
        record_qualified_type_prefix_matches(
            resolver,
            &file.scopes.scopes,
            scope,
            &segments,
            target,
            &mut edits,
            &file.file,
            new_name,
        );
    }

    // Qualified `super` expressions (`Outer.super`).
    for expr in root.descendants().filter_map(ast::SuperExpression::cast) {
        let Some(qual) = expr.qualifier() else {
            continue;
        };
        let Some(segments) = segments_from_qualifier_expression(&qual) else {
            continue;
        };
        if segments.is_empty() {
            continue;
        }

        let scope = scope_for_node(&file.scopes, &file.ast_id_map, expr.syntax(), file.db_file);
        record_qualified_type_prefix_matches(
            resolver,
            &file.scopes.scopes,
            scope,
            &segments,
            target,
            &mut edits,
            &file.file,
            new_name,
        );
    }

    edits
}

fn segments_from_qualifier_expression(expr: &ast::Expression) -> Option<Vec<Segment>> {
    match expr {
        ast::Expression::NameExpression(it) => {
            // `NameExpression` does not currently expose a typed accessor for its name and the
            // tree shape may or may not include an explicit `ast::Name` child. Be defensive:
            // - prefer a `Name` node when present
            // - otherwise fall back to identifier-like direct child tokens
            if let Some(name) = it.syntax().children().find_map(ast::Name::cast) {
                return Some(segments_from_name(&name));
            }

            Some(segments_from_syntax(it.syntax()))
        }
        _ => None,
    }
}

fn segments_from_name(name: &ast::Name) -> Vec<Segment> {
    support::ident_tokens(name.syntax())
        .map(|tok| Segment {
            text: tok.text().to_string(),
            range: token_text_range(&tok),
        })
        .collect()
}

fn segments_from_syntax(node: &SyntaxNode) -> Vec<Segment> {
    node.children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| tok.kind().is_identifier_like())
        .map(|tok| Segment {
            text: tok.text().to_string(),
            range: token_text_range(&tok),
        })
        .collect()
}

fn segment_index_at_offset(segments: &[Segment], offset: usize) -> Option<usize> {
    if segments.is_empty() {
        return None;
    }

    // Prefer a segment whose identifier token contains the offset.
    if let Some((idx, _)) = segments
        .iter()
        .enumerate()
        .find(|(_, seg)| seg.range.start <= offset && offset < seg.range.end)
    {
        return Some(idx);
    }

    // If the cursor is on a dot or between tokens, prefer the previous segment.
    segments
        .iter()
        .enumerate()
        .filter(|(_, seg)| seg.range.end <= offset)
        .map(|(idx, _)| idx)
        .last()
        .or(Some(segments.len().saturating_sub(1)))
}

fn prefix_qname(segments: &[Segment]) -> QualifiedName {
    let mut prefix = String::new();
    for (idx, seg) in segments.iter().enumerate() {
        if idx > 0 {
            prefix.push('.');
        }
        prefix.push_str(&seg.text);
    }
    QualifiedName::from_dotted(&prefix)
}

fn record_qualified_type_prefix_matches(
    resolver: &Resolver<'_>,
    scopes: &nova_resolve::ScopeGraph,
    scope: nova_resolve::ScopeId,
    segments: &[Segment],
    target: ItemId,
    edits: &mut Vec<TextEdit>,
    file: &FileId,
    new_name: &str,
) {
    let mut prefix = String::new();
    for (idx, seg) in segments.iter().enumerate() {
        if idx > 0 {
            prefix.push('.');
        }
        prefix.push_str(&seg.text);

        let qn = QualifiedName::from_dotted(&prefix);
        let Some(TypeResolution::Source(item)) =
            resolver.resolve_qualified_type_resolution_in_scope(scopes, scope, &qn)
        else {
            continue;
        };

        if item == target {
            edits.push(TextEdit::replace(
                file.clone(),
                seg.range,
                new_name.to_string(),
            ));
        }
    }
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
