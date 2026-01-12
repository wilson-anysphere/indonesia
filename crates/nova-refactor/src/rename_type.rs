use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use nova_core::{Name, QualifiedName};
use nova_hir::ast_id::AstIdMap;
use nova_hir::hir;
use nova_hir::ids::{
    AnnotationId, ClassId, ConstructorId, EnumId, InitializerId, InterfaceId, ItemId, MethodId,
    RecordId,
};
use nova_hir::item_tree::ItemTree;
use nova_hir::queries::HirDatabase;
use nova_resolve::expr_scopes::{ExprScopes, ScopeId as ExprScopeId};
use nova_resolve::ids::{DefWithBodyId, ParamId};
use nova_resolve::{Resolution, Resolver, TypeResolution, WorkspaceDefMap};
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
    db: InMemoryHirDb,
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
        edits.extend(collect_method_call_reference_edits(
            &resolver,
            file,
            &analysis.db,
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
            db,
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
        let mut from_name_expression = false;

        // Many type positions are represented by `ast::Type` nodes whose identifier tokens are
        // direct children rather than an explicit `ast::Name`. Handle that shape so rename can be
        // invoked from usages like `Outer.Inner` (including when `Outer` is the type being renamed).
        if segments.is_none() {
            if let Some(ty) = parent.ancestors().find_map(ast::Type::cast) {
                if let Some(named) = ty.named() {
                    let segs = support::child::<ast::Name>(named.syntax())
                        .map(|name| segments_from_name(&name))
                        .unwrap_or_else(|| segments_from_syntax(named.syntax()));
                    if !segs.is_empty() {
                        segments = Some(segs);
                        scope_node = Some(ty.syntax().clone());
                    }
                }
            }
        }

        // `TypeName.Identifier(...)` and related constructs are represented as `NameExpression`s in
        // some tree shapes (e.g. `Outer.Inner.m()` -> `NameExpression("Outer.Inner.m")`). Allow
        // rename to be invoked from those qualified-name occurrences as well.
        //
        // Restrict this fallback to qualified names to avoid accidentally treating plain identifier
        // expressions (locals/fields/etc.) as type names.
        if segments.is_none() {
            if let Some(expr) = parent.ancestors().find_map(ast::NameExpression::cast) {
                let segs = segments_from_syntax(expr.syntax());
                if segs.len() > 1 {
                    segments = Some(segs);
                    scope_node = Some(expr.syntax().clone());
                    from_name_expression = true;
                }
            }
        }

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

        // If the qualified name appears in an expression context (e.g. `Foo.bar()`), prefer value
        // namespace resolution. This avoids treating local variables/fields as type names when
        // they share the same identifier as a type.
        if from_name_expression {
            if let Some(first) = segments.first() {
                let name = Name::new(first.text.as_str());
                let mut body_scopes_cache: HashMap<DefWithBodyId, BodyScopeCacheEntry> =
                    HashMap::new();
                if resolves_to_local_or_param(
                    &self.db,
                    analysis,
                    &mut body_scopes_cache,
                    &scope_node,
                    offset,
                    &name,
                ) {
                    return Err(RenameTypeError::NoTypeAtOffset {
                        file: file.clone(),
                        offset,
                    });
                }
                match resolver.resolve_name(&analysis.scopes.scopes, scope, &name) {
                    Some(Resolution::Type(_) | Resolution::Package(_)) | None => {}
                    Some(_) => {
                        return Err(RenameTypeError::NoTypeAtOffset {
                            file: file.clone(),
                            offset,
                        });
                    }
                }
            }
        }

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

#[derive(Debug)]
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

#[derive(Debug)]
struct BodyScopeCacheEntry {
    body: Arc<hir::Body>,
    scopes: ExprScopes,
}

fn def_with_body_for_node(
    ast_id_map: &AstIdMap,
    node: &SyntaxNode,
    file: nova_core::FileId,
) -> Option<DefWithBodyId> {
    for ancestor in node.ancestors() {
        match ancestor.kind() {
            SyntaxKind::MethodDeclaration => {
                let ast_id = ast_id_map.ast_id(&ancestor)?;
                return Some(DefWithBodyId::Method(MethodId::new(file, ast_id)));
            }
            SyntaxKind::ConstructorDeclaration | SyntaxKind::CompactConstructorDeclaration => {
                let ast_id = ast_id_map.ast_id(&ancestor)?;
                return Some(DefWithBodyId::Constructor(ConstructorId::new(file, ast_id)));
            }
            SyntaxKind::InitializerBlock => {
                let ast_id = ast_id_map.ast_id(&ancestor)?;
                return Some(DefWithBodyId::Initializer(InitializerId::new(file, ast_id)));
            }
            _ => {}
        }
    }
    None
}

fn build_body_scope_cache_entry(
    db: &dyn HirDatabase,
    file: &FileAnalysis,
    owner: DefWithBodyId,
) -> BodyScopeCacheEntry {
    match owner {
        DefWithBodyId::Method(method) => {
            let body = db.hir_body(method);
            let data = file.item_tree.method(method);
            let params: Vec<ParamId> = (0..data.params.len())
                .map(|idx| ParamId::new(owner, idx as u32))
                .collect();
            let scopes = ExprScopes::new(&body, &params, |param| {
                let idx = param.index as usize;
                Name::from(data.params[idx].name.as_str())
            });
            BodyScopeCacheEntry { body, scopes }
        }
        DefWithBodyId::Constructor(constructor) => {
            let body = db.hir_constructor_body(constructor);
            let data = file.item_tree.constructor(constructor);
            let params: Vec<ParamId> = (0..data.params.len())
                .map(|idx| ParamId::new(owner, idx as u32))
                .collect();
            let scopes = ExprScopes::new(&body, &params, |param| {
                let idx = param.index as usize;
                Name::from(data.params[idx].name.as_str())
            });
            BodyScopeCacheEntry { body, scopes }
        }
        DefWithBodyId::Initializer(initializer) => {
            let body = db.hir_initializer_body(initializer);
            let scopes = ExprScopes::new(&body, &[], |_| Name::new(""));
            BodyScopeCacheEntry { body, scopes }
        }
    }
}

fn expr_scope_for_offset(
    body: &hir::Body,
    scopes: &ExprScopes,
    offset: usize,
) -> Option<ExprScopeId> {
    fn contains(range: nova_types::Span, offset: usize) -> bool {
        range.start <= offset && offset < range.end
    }

    fn visit_expr(
        body: &hir::Body,
        expr_id: hir::ExprId,
        offset: usize,
        best_expr: &mut Option<(usize, hir::ExprId)>,
        best_stmt: &mut Option<(usize, hir::StmtId)>,
    ) {
        use hir::Expr;

        let expr = &body.exprs[expr_id];
        let range = match expr {
            Expr::Name { range, .. }
            | Expr::Literal { range, .. }
            | Expr::Null { range }
            | Expr::This { range }
            | Expr::Super { range }
            | Expr::FieldAccess { range, .. }
            | Expr::ArrayAccess { range, .. }
            | Expr::MethodReference { range, .. }
            | Expr::ConstructorReference { range, .. }
            | Expr::ClassLiteral { range, .. }
            | Expr::Cast { range, .. }
            | Expr::Call { range, .. }
            | Expr::New { range, .. }
            | Expr::ArrayCreation { range, .. }
            | Expr::ArrayInitializer { range, .. }
            | Expr::Unary { range, .. }
            | Expr::Binary { range, .. }
            | Expr::Instanceof { range, .. }
            | Expr::Assign { range, .. }
            | Expr::Conditional { range, .. }
            | Expr::Lambda { range, .. }
            | Expr::Switch { range, .. }
            | Expr::Invalid { range, .. }
            | Expr::Missing { range } => *range,
        };

        if !contains(range, offset) {
            return;
        }

        let len = range.len();
        if best_expr
            .map(|(best_len, _)| len < best_len)
            .unwrap_or(true)
        {
            *best_expr = Some((len, expr_id));
        }

        match expr {
            Expr::FieldAccess { receiver, .. } => {
                visit_expr(body, *receiver, offset, best_expr, best_stmt)
            }
            Expr::ArrayAccess { array, index, .. } => {
                visit_expr(body, *array, offset, best_expr, best_stmt);
                visit_expr(body, *index, offset, best_expr, best_stmt);
            }
            Expr::MethodReference { receiver, .. }
            | Expr::ConstructorReference { receiver, .. } => {
                visit_expr(body, *receiver, offset, best_expr, best_stmt);
            }
            Expr::ClassLiteral { ty, .. } => visit_expr(body, *ty, offset, best_expr, best_stmt),
            Expr::Cast { expr, .. } => visit_expr(body, *expr, offset, best_expr, best_stmt),
            Expr::Call { callee, args, .. } => {
                visit_expr(body, *callee, offset, best_expr, best_stmt);
                for arg in args {
                    visit_expr(body, *arg, offset, best_expr, best_stmt);
                }
            }
            Expr::New { args, .. } => {
                for arg in args {
                    visit_expr(body, *arg, offset, best_expr, best_stmt);
                }
            }
            Expr::ArrayCreation {
                dim_exprs,
                initializer,
                ..
            } => {
                for dim in dim_exprs {
                    visit_expr(body, *dim, offset, best_expr, best_stmt);
                }
                if let Some(initializer) = initializer {
                    visit_expr(body, *initializer, offset, best_expr, best_stmt);
                }
            }
            Expr::ArrayInitializer { items, .. } => {
                for item in items {
                    visit_expr(body, *item, offset, best_expr, best_stmt);
                }
            }
            Expr::Unary { expr, .. } => visit_expr(body, *expr, offset, best_expr, best_stmt),
            Expr::Binary { lhs, rhs, .. } => {
                visit_expr(body, *lhs, offset, best_expr, best_stmt);
                visit_expr(body, *rhs, offset, best_expr, best_stmt);
            }
            Expr::Instanceof { expr, .. } => visit_expr(body, *expr, offset, best_expr, best_stmt),
            Expr::Assign { lhs, rhs, .. } => {
                visit_expr(body, *lhs, offset, best_expr, best_stmt);
                visit_expr(body, *rhs, offset, best_expr, best_stmt);
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                visit_expr(body, *condition, offset, best_expr, best_stmt);
                visit_expr(body, *then_expr, offset, best_expr, best_stmt);
                visit_expr(body, *else_expr, offset, best_expr, best_stmt);
            }
            Expr::Switch {
                selector, body: b, ..
            } => {
                visit_expr(body, *selector, offset, best_expr, best_stmt);
                visit_stmt(body, *b, offset, best_expr, best_stmt);
            }
            Expr::Lambda {
                body: lambda_body, ..
            } => match lambda_body {
                hir::LambdaBody::Expr(expr) => {
                    visit_expr(body, *expr, offset, best_expr, best_stmt)
                }
                hir::LambdaBody::Block(stmt) => {
                    visit_stmt(body, *stmt, offset, best_expr, best_stmt)
                }
            },
            Expr::Invalid { children, .. } => {
                for child in children {
                    visit_expr(body, *child, offset, best_expr, best_stmt);
                }
            }
            Expr::Name { .. }
            | Expr::Literal { .. }
            | Expr::Null { .. }
            | Expr::This { .. }
            | Expr::Super { .. }
            | Expr::Missing { .. } => {}
        }
    }

    fn visit_stmt(
        body: &hir::Body,
        stmt_id: hir::StmtId,
        offset: usize,
        best_expr: &mut Option<(usize, hir::ExprId)>,
        best_stmt: &mut Option<(usize, hir::StmtId)>,
    ) {
        use hir::Stmt;

        let stmt = &body.stmts[stmt_id];
        let range = match stmt {
            Stmt::Block { range, .. }
            | Stmt::Let { range, .. }
            | Stmt::Expr { range, .. }
            | Stmt::Yield { range, .. }
            | Stmt::Assert { range, .. }
            | Stmt::Return { range, .. }
            | Stmt::If { range, .. }
            | Stmt::While { range, .. }
            | Stmt::For { range, .. }
            | Stmt::ForEach { range, .. }
            | Stmt::Synchronized { range, .. }
            | Stmt::Switch { range, .. }
            | Stmt::Try { range, .. }
            | Stmt::Throw { range, .. }
            | Stmt::Break { range }
            | Stmt::Continue { range }
            | Stmt::Empty { range } => *range,
        };

        if !contains(range, offset) {
            return;
        }

        let len = range.len();
        if best_stmt
            .map(|(best_len, _)| len < best_len)
            .unwrap_or(true)
        {
            *best_stmt = Some((len, stmt_id));
        }

        match stmt {
            Stmt::Block { statements, .. } => {
                for stmt in statements {
                    visit_stmt(body, *stmt, offset, best_expr, best_stmt);
                }
            }
            Stmt::Let { initializer, .. } => {
                if let Some(expr) = initializer {
                    visit_expr(body, *expr, offset, best_expr, best_stmt);
                }
            }
            Stmt::Expr { expr, .. } => visit_expr(body, *expr, offset, best_expr, best_stmt),
            Stmt::Yield { expr, .. } => {
                if let Some(expr) = expr {
                    visit_expr(body, *expr, offset, best_expr, best_stmt);
                }
            }
            Stmt::Assert {
                condition, message, ..
            } => {
                visit_expr(body, *condition, offset, best_expr, best_stmt);
                if let Some(expr) = message {
                    visit_expr(body, *expr, offset, best_expr, best_stmt);
                }
            }
            Stmt::Return { expr, .. } => {
                if let Some(expr) = expr {
                    visit_expr(body, *expr, offset, best_expr, best_stmt);
                }
            }
            Stmt::Yield { expr, .. } => {
                if let Some(expr) = expr {
                    visit_expr(body, *expr, offset, best_expr, best_stmt);
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                visit_expr(body, *condition, offset, best_expr, best_stmt);
                visit_stmt(body, *then_branch, offset, best_expr, best_stmt);
                if let Some(stmt) = else_branch {
                    visit_stmt(body, *stmt, offset, best_expr, best_stmt);
                }
            }
            Stmt::While {
                condition, body: b, ..
            } => {
                visit_expr(body, *condition, offset, best_expr, best_stmt);
                visit_stmt(body, *b, offset, best_expr, best_stmt);
            }
            Stmt::For {
                init,
                condition,
                update,
                body: b,
                ..
            } => {
                for stmt in init {
                    visit_stmt(body, *stmt, offset, best_expr, best_stmt);
                }
                if let Some(expr) = condition {
                    visit_expr(body, *expr, offset, best_expr, best_stmt);
                }
                for expr in update {
                    visit_expr(body, *expr, offset, best_expr, best_stmt);
                }
                visit_stmt(body, *b, offset, best_expr, best_stmt);
            }
            Stmt::ForEach {
                iterable, body: b, ..
            } => {
                visit_expr(body, *iterable, offset, best_expr, best_stmt);
                visit_stmt(body, *b, offset, best_expr, best_stmt);
            }
            Stmt::Synchronized {
                expr,
                body: sync_body,
                ..
            } => {
                visit_expr(body, *expr, offset, best_expr, best_stmt);
                visit_stmt(body, *sync_body, offset, best_expr, best_stmt);
            }
            Stmt::Switch {
                selector, body: b, ..
            } => {
                visit_expr(body, *selector, offset, best_expr, best_stmt);
                visit_stmt(body, *b, offset, best_expr, best_stmt);
            }
            Stmt::Try {
                body: try_body,
                catches,
                finally,
                ..
            } => {
                visit_stmt(body, *try_body, offset, best_expr, best_stmt);
                for catch in catches {
                    visit_stmt(body, catch.body, offset, best_expr, best_stmt);
                }
                if let Some(stmt) = finally {
                    visit_stmt(body, *stmt, offset, best_expr, best_stmt);
                }
            }
            Stmt::Throw { expr, .. } => visit_expr(body, *expr, offset, best_expr, best_stmt),
            Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Empty { .. } => {}
        }
    }

    let mut best_expr: Option<(usize, hir::ExprId)> = None;
    let mut best_stmt: Option<(usize, hir::StmtId)> = None;
    visit_stmt(body, body.root, offset, &mut best_expr, &mut best_stmt);

    if let Some((_, expr_id)) = best_expr {
        scopes.scope_for_expr(expr_id)
    } else if let Some((_, stmt_id)) = best_stmt {
        scopes.scope_for_stmt(stmt_id)
    } else {
        None
    }
}

fn resolves_to_local_or_param(
    db: &dyn HirDatabase,
    file: &FileAnalysis,
    cache: &mut HashMap<DefWithBodyId, BodyScopeCacheEntry>,
    context_node: &SyntaxNode,
    offset: usize,
    name: &Name,
) -> bool {
    let Some(owner) = def_with_body_for_node(&file.ast_id_map, context_node, file.db_file) else {
        return false;
    };

    let entry = cache
        .entry(owner)
        .or_insert_with(|| build_body_scope_cache_entry(db, file, owner));
    let Some(scope) = expr_scope_for_offset(&entry.body, &entry.scopes, offset) else {
        return false;
    };

    entry.scopes.resolve_name(scope, name).is_some()
}

fn collect_method_call_reference_edits(
    resolver: &Resolver<'_>,
    file: &FileAnalysis,
    db: &dyn HirDatabase,
    target: ItemId,
    new_name: &str,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    let root = file.parse.syntax();
    let mut body_scopes_cache: HashMap<DefWithBodyId, BodyScopeCacheEntry> = HashMap::new();

    for call in root
        .descendants()
        .filter_map(ast::MethodCallExpression::cast)
    {
        let Some(callee) = call.callee() else {
            continue;
        };

        let scope = scope_for_node(&file.scopes, &file.ast_id_map, call.syntax(), file.db_file);

        match callee {
            ast::Expression::NameExpression(expr) => {
                // `NameExpression` in a call position often includes the method name itself, e.g.
                // `Outer.Inner.m()` -> name tokens `Outer`, `Inner`, `m`. The last segment is a
                // value-namespace member, not a type, so exclude it from prefix matching.
                let segments = segments_from_syntax(expr.syntax());
                if segments.len() <= 1 {
                    continue;
                }
                let qualifier = &segments[..segments.len() - 1];
                let Some(first) = qualifier.first() else {
                    continue;
                };
                let name = Name::new(first.text.as_str());
                if resolves_to_local_or_param(
                    db,
                    file,
                    &mut body_scopes_cache,
                    call.syntax(),
                    first.range.start,
                    &name,
                ) {
                    continue;
                }
                match resolver.resolve_name(&file.scopes.scopes, scope, &name) {
                    Some(Resolution::Type(_) | Resolution::Package(_)) | None => {}
                    Some(_) => continue,
                }
                if qualifier.is_empty() {
                    continue;
                }
                record_qualified_type_prefix_matches(
                    resolver,
                    &file.scopes.scopes,
                    scope,
                    qualifier,
                    target,
                    &mut edits,
                    &file.file,
                    new_name,
                );
            }
            ast::Expression::FieldAccessExpression(expr) => {
                // Some call sites use a `FieldAccessExpression` callee (`Foo.m()` as `Foo` receiver
                // + `m` name token). Try resolving the receiver as a qualified type name.
                let Some(recv) = expr.expression() else {
                    continue;
                };
                let ast::Expression::NameExpression(name_expr) = recv else {
                    continue;
                };
                let segments = segments_from_syntax(name_expr.syntax());
                if segments.is_empty() {
                    continue;
                }
                let Some(first) = segments.first() else {
                    continue;
                };
                let name = Name::new(first.text.as_str());
                if resolves_to_local_or_param(
                    db,
                    file,
                    &mut body_scopes_cache,
                    call.syntax(),
                    first.range.start,
                    &name,
                ) {
                    continue;
                }
                match resolver.resolve_name(&file.scopes.scopes, scope, &name) {
                    Some(Resolution::Type(_) | Resolution::Package(_)) | None => {}
                    Some(_) => continue,
                }
                if segments.is_empty() {
                    continue;
                }
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
            _ => {}
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
