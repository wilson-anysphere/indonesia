use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use nova_core::{Name, QualifiedName};
use nova_db::salsa::{Database as SalsaDatabase, NovaHir, NovaIndexing, NovaTypeck};
use nova_db::{FileId as DbFileId, ProjectId};
use nova_hir::hir;
use nova_hir::ids::{ConstructorId, FieldId, ItemId, MethodId};
use nova_hir::item_tree::Modifiers as HirModifiers;
use nova_hir::item_tree::{FieldKind, Item, Member};
use nova_hir::queries::HirDatabase;
use nova_hir::{item_tree, item_tree::ItemTree};
use nova_resolve::{
    BodyOwner, DefMap, LocalRef, NameResolution, ParamOwner, ParamRef, Resolution, Resolver,
    ScopeBuildResult, ScopeId, ScopeKind, StaticMemberResolution, TypeKind, TypeResolution,
    WorkspaceDefMap,
};
use nova_syntax::java as java_syntax;
use nova_syntax::{ast, AstNode};

use crate::edit::{FileId, TextRange};
use crate::semantic::{
    MethodSignature as SemanticMethodSignature, RefactorDatabase, Reference, SymbolDefinition,
    TypeSymbolInfo,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SymbolId(u32);

impl SymbolId {
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    pub(crate) fn as_usize(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JavaSymbolKind {
    Package,
    Type,
    TypeParameter,
    Method,
    Field,
    Local,
    Parameter,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct OverrideMethodSignature {
    name: String,
    param_types: Vec<String>,
}

impl OverrideMethodSignature {
    fn param_count(&self) -> usize {
        self.param_types.len()
    }
}

#[derive(Clone, Debug)]
struct SymbolData {
    def: SymbolDefinition,
    kind: JavaSymbolKind,
    type_info: Option<TypeInfo>,
    method_signature: Option<SemanticMethodSignature>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ResolutionKey {
    Local(LocalRef),
    Param(ParamRef),
    Field(FieldId),
    /// Method-group key (overload set).
    ///
    /// Nova's scope graph stores overloaded methods as `Resolution::Methods(Vec<MethodId>)`.
    /// We intern one symbol per (declaring type, method name) and pick a representative `MethodId`
    /// for the group. During database construction we map all overload `MethodId`s to that same
    /// symbol, so any `MethodId` from the set can be used to recover the group's `SymbolId`.
    Method(MethodId),
    Type(ItemId),
    TypeParam(TypeParamKey),
    Package(DbFileId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TypeParamOwner {
    Type(ItemId),
    Method(MethodId),
    Constructor(ConstructorId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TypeParamKey {
    owner: TypeParamOwner,
    index: usize,
}

#[derive(Debug, Clone)]
struct SymbolCandidate {
    key: ResolutionKey,
    file: FileId,
    name: String,
    name_range: TextRange,
    scope: u32,
    kind: JavaSymbolKind,
    type_info: Option<TypeInfo>,
    method_signature: Option<SemanticMethodSignature>,
}

#[derive(Clone, Debug)]
struct TypeInfo {
    package: Option<String>,
    is_top_level: bool,
    is_public: bool,
}

#[derive(Debug, Clone)]
struct MethodGroupInfo {
    file: FileId,
    representative: MethodId,
    method_ids: Vec<MethodId>,
    decl_ranges: Vec<TextRange>,
}

#[derive(Debug, Clone, Default)]
struct ScopeInterner {
    map: HashMap<(DbFileId, nova_resolve::ScopeId), u32>,
    reverse: Vec<(DbFileId, nova_resolve::ScopeId)>,
}

impl ScopeInterner {
    fn intern(&mut self, file: DbFileId, scope: nova_resolve::ScopeId) -> u32 {
        if let Some(id) = self.map.get(&(file, scope)) {
            return *id;
        }
        let id = self.reverse.len() as u32;
        self.reverse.push((file, scope));
        self.map.insert((file, scope), id);
        id
    }

    fn lookup(&self, id: u32) -> Option<(DbFileId, nova_resolve::ScopeId)> {
        self.reverse.get(id as usize).copied()
    }
}

struct HirAdapter<'a> {
    snap: &'a nova_db::salsa::Snapshot,
    files: &'a HashMap<DbFileId, Arc<str>>,
}

impl HirDatabase for HirAdapter<'_> {
    fn file_text(&self, file: DbFileId) -> Arc<str> {
        self.files
            .get(&file)
            .cloned()
            .unwrap_or_else(|| Arc::<str>::from(""))
    }

    fn hir_item_tree(&self, file: DbFileId) -> Arc<nova_hir::item_tree::ItemTree> {
        self.snap.hir_item_tree(file)
    }

    fn hir_body(&self, method: nova_hir::ids::MethodId) -> Arc<hir::Body> {
        self.snap.hir_body(method)
    }

    fn hir_constructor_body(&self, constructor: nova_hir::ids::ConstructorId) -> Arc<hir::Body> {
        self.snap.hir_constructor_body(constructor)
    }

    fn hir_initializer_body(&self, initializer: nova_hir::ids::InitializerId) -> Arc<hir::Body> {
        self.snap.hir_initializer_body(initializer)
    }
}

/// Salsa-backed semantic database used by Nova refactorings.
///
/// This provides a minimal [`RefactorDatabase`] implementation by lowering Java source through
/// Nova's canonical syntax + HIR + scope graph pipeline (`nova-syntax`, `nova-hir`, `nova-resolve`)
/// and projecting the resulting locals/parameters into the stable `SymbolId` space expected by
/// the semantic refactoring engine (`rename`, `inline_variable`, ...).
pub struct RefactorJavaDatabase {
    files: BTreeMap<FileId, Arc<str>>,
    db_files: BTreeMap<FileId, DbFileId>,
    snap: nova_db::salsa::Snapshot,

    scopes: HashMap<DbFileId, ScopeBuildResult>,
    workspace_def_map: WorkspaceDefMap,
    type_supertypes: HashMap<ItemId, Vec<ItemId>>,
    type_subtypes: HashMap<ItemId, Vec<ItemId>>,
    scope_interner: ScopeInterner,

    symbols: Vec<SymbolData>,
    references: Vec<Vec<Reference>>,
    spans: Vec<(FileId, TextRange, SymbolId)>,
    name_expr_scopes: HashMap<FileId, HashMap<TextRange, ScopeId>>,

    resolution_to_symbol: HashMap<ResolutionKey, SymbolId>,
    top_level_types: HashMap<(Option<String>, String), SymbolId>,
    method_to_symbol: HashMap<MethodId, SymbolId>,
    method_signatures: HashMap<MethodId, OverrideMethodSignature>,
    method_owners: HashMap<MethodId, ItemId>,
}

impl RefactorJavaDatabase {
    pub fn new(files: impl IntoIterator<Item = (FileId, String)>) -> Self {
        Self::new_shared(
            files
                .into_iter()
                .map(|(file, text)| (file, Arc::<str>::from(text))),
        )
    }

    pub fn new_shared(files: impl IntoIterator<Item = (FileId, Arc<str>)>) -> Self {
        let files: BTreeMap<FileId, Arc<str>> = files.into_iter().collect();

        let salsa = SalsaDatabase::new();
        let project = ProjectId::from_raw(0);
        salsa.set_jdk_index(project, Arc::new(nova_jdk::JdkIndex::new()));
        salsa.set_classpath_index(project, None);

        let mut file_ids: BTreeMap<FileId, DbFileId> = BTreeMap::new();
        let mut texts_by_id: HashMap<DbFileId, Arc<str>> = HashMap::new();

        for (idx, (file, text)) in files.iter().enumerate() {
            let id = DbFileId::from_raw(idx as u32);
            file_ids.insert(file.clone(), id);
            texts_by_id.insert(id, text.clone());

            // Set `file_rel_path` before `set_file_text` so `set_file_text` doesn't synthesize and
            // then discard a default `file-123.java` rel-path.
            salsa.set_file_rel_path(id, Arc::new(file.0.clone()));
            salsa.set_file_text(id, text.to_string());
        }

        let project_files: Vec<DbFileId> = file_ids.values().copied().collect();
        salsa.set_project_files(project, Arc::new(project_files));

        let snap = salsa.snapshot();
        let hir = HirAdapter {
            snap: &snap,
            files: &texts_by_id,
        };

        // Build a workspace-wide type index so name resolution can see types declared in other
        // source files. This is required for `rename` to update cross-file references like
        // `new Foo()` when `Foo` is defined elsewhere in the workspace.
        let mut workspace_def_map = WorkspaceDefMap::default();
        let mut item_trees: HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>> = HashMap::new();
        for (_file, file_id) in &file_ids {
            let tree = snap.hir_item_tree(*file_id);
            item_trees.insert(*file_id, tree.clone());
            let def_map = DefMap::from_item_tree(*file_id, &tree);
            workspace_def_map.extend_from_def_map(&def_map);
        }

        // Shared resolver used for both reference collection and hierarchy building.
        let jdk = nova_jdk::JdkIndex::new();
        let resolver = Resolver::new(&jdk)
            .with_classpath(&workspace_def_map)
            .with_workspace(&workspace_def_map);

        let mut scope_interner = ScopeInterner::default();
        let mut scopes: HashMap<DbFileId, ScopeBuildResult> = HashMap::new();
        let mut candidates: Vec<SymbolCandidate> = Vec::new();
        let mut method_groups: Vec<MethodGroupInfo> = Vec::new();
        let mut type_constructor_refs: HashMap<ItemId, Vec<(FileId, TextRange)>> = HashMap::new();
        let mut method_signatures: HashMap<MethodId, OverrideMethodSignature> = HashMap::new();
        let mut method_owners: HashMap<MethodId, ItemId> = HashMap::new();

        // Build per-file scope graphs + symbol definitions.
        for (file, file_id) in &file_ids {
            let scope_result = nova_resolve::build_scopes(&hir, *file_id);
            let tree = item_trees
                .get(file_id)
                .cloned()
                .unwrap_or_else(|| snap.hir_item_tree(*file_id));

            // Package declarations live at the compilation unit level, outside of Nova's scope
            // graph (which primarily models locals/parameters). We still surface them as refactor
            // symbols so upstream layers (e.g. LSP rename) can dispatch to the appropriate
            // refactoring (move_package).
            if let Some(pkg) = tree.package.as_ref() {
                if let Some(text) = files.get(file) {
                    if let Some(name_range) = package_decl_name_range(text.as_ref()) {
                        let scope = scope_interner.intern(*file_id, scope_result.file_scope);
                        candidates.push(SymbolCandidate {
                            key: ResolutionKey::Package(*file_id),
                            file: file.clone(),
                            name: pkg.name.clone(),
                            name_range,
                            scope,
                            kind: JavaSymbolKind::Package,
                            type_info: None,
                            method_signature: None,
                        });
                    }
                }
            }

            let package = tree
                .package
                .as_ref()
                .map(|pkg| pkg.name.as_str())
                .filter(|pkg| !pkg.is_empty());

            // Type/field/method symbols live in item tree (file-level) scopes.
            for item in &tree.items {
                let item_id = item_to_item_id(*item);
                collect_type_candidates(
                    file,
                    *file_id,
                    tree.as_ref(),
                    &scope_result,
                    scope_result.file_scope,
                    item_id,
                    true,
                    package,
                    &mut scope_interner,
                    &mut candidates,
                    &mut method_groups,
                    &mut type_constructor_refs,
                    &mut method_signatures,
                    &mut method_owners,
                );
            }

            // Parameters live in method/constructor scopes.
            let mut method_ids: Vec<_> = scope_result.method_scopes.keys().copied().collect();
            method_ids.sort();
            for method in method_ids {
                let method_scope = scope_result
                    .method_scopes
                    .get(&method)
                    .copied()
                    .expect("method scope map must contain key");

                let scope = scope_interner.intern(*file_id, method_scope);
                let method_data = tree.method(method);
                for (idx, tp) in method_data.type_params.iter().enumerate() {
                    candidates.push(SymbolCandidate {
                        key: ResolutionKey::TypeParam(TypeParamKey {
                            owner: TypeParamOwner::Method(method),
                            index: idx,
                        }),
                        file: file.clone(),
                        name: tp.name.clone(),
                        name_range: TextRange::new(tp.name_range.start, tp.name_range.end),
                        scope,
                        kind: JavaSymbolKind::TypeParameter,
                        type_info: None,
                        method_signature: None,
                    });
                }
                for (idx, param) in method_data.params.iter().enumerate() {
                    candidates.push(SymbolCandidate {
                        key: ResolutionKey::Param(ParamRef {
                            owner: ParamOwner::Method(method),
                            index: idx,
                        }),
                        file: file.clone(),
                        name: param.name.clone(),
                        name_range: TextRange::new(param.name_range.start, param.name_range.end),
                        scope,
                        kind: JavaSymbolKind::Parameter,
                        type_info: None,
                        method_signature: None,
                    });
                }
            }

            let mut ctor_ids: Vec<_> = scope_result.constructor_scopes.keys().copied().collect();
            ctor_ids.sort();
            for ctor in ctor_ids {
                let ctor_scope = scope_result
                    .constructor_scopes
                    .get(&ctor)
                    .copied()
                    .expect("constructor scope map must contain key");
                let scope = scope_interner.intern(*file_id, ctor_scope);
                let ctor_data = tree.constructor(ctor);
                for (idx, tp) in ctor_data.type_params.iter().enumerate() {
                    candidates.push(SymbolCandidate {
                        key: ResolutionKey::TypeParam(TypeParamKey {
                            owner: TypeParamOwner::Constructor(ctor),
                            index: idx,
                        }),
                        file: file.clone(),
                        name: tp.name.clone(),
                        name_range: TextRange::new(tp.name_range.start, tp.name_range.end),
                        scope,
                        kind: JavaSymbolKind::TypeParameter,
                        type_info: None,
                        method_signature: None,
                    });
                }
                for (idx, param) in ctor_data.params.iter().enumerate() {
                    candidates.push(SymbolCandidate {
                        key: ResolutionKey::Param(ParamRef {
                            owner: ParamOwner::Constructor(ctor),
                            index: idx,
                        }),
                        file: file.clone(),
                        name: param.name.clone(),
                        name_range: TextRange::new(param.name_range.start, param.name_range.end),
                        scope,
                        kind: JavaSymbolKind::Parameter,
                        type_info: None,
                        method_signature: None,
                    });
                }
            }

            // Locals live in block scopes. We intern each block scope exactly once (in allocation
            // order) so global scope IDs are deterministic.
            let mut body_cache: HashMap<BodyOwner, Arc<hir::Body>> = HashMap::new();
            for &block_scope in scope_result.block_scopes.iter() {
                let data = scope_result.scopes.scope(block_scope);
                for res in data.values().values() {
                    let Resolution::Local(local_ref) = res else {
                        continue;
                    };

                    let body = body_cache
                        .entry(local_ref.owner)
                        .or_insert_with(|| match local_ref.owner {
                            BodyOwner::Method(m) => snap.hir_body(m),
                            BodyOwner::Constructor(c) => snap.hir_constructor_body(c),
                            BodyOwner::Initializer(i) => snap.hir_initializer_body(i),
                        });
                    let local = &body.locals[local_ref.local];

                    // For locals introduced by `let` statements, `nova-resolve` models Java's
                    // order-sensitive scoping by threading a chain of nested scopes through the
                    // block. This means a later local lives in a *child* scope, and is therefore
                    // not visible when checking the original local's declaration scope. For
                    // refactoring conflict checks (e.g. renaming `foo` to `bar` when `bar` is
                    // declared later in the same block), we want a scope that represents the
                    // full lexical region where the local is visible.
                    //
                    // We approximate this by using the scope at the end of the enclosing block
                    // statement (i.e. the scope of the block's final statement), which will have
                    // later locals in-scope via the parent chain and/or its own entries.
                    let scope_id = refactor_local_scope(&scope_result, body.as_ref(), block_scope);
                    let scope = scope_interner.intern(*file_id, scope_id);

                    candidates.push(SymbolCandidate {
                        key: ResolutionKey::Local(*local_ref),
                        file: file.clone(),
                        name: local.name.clone(),
                        name_range: TextRange::new(local.name_range.start, local.name_range.end),
                        scope,
                        kind: JavaSymbolKind::Local,
                        type_info: None,
                        method_signature: None,
                    });
                }
            }

            // Lambda parameters are modeled as locals in HIR and introduced via `ScopeKind::Lambda`
            // scopes (not block scopes), so we need to walk bodies to index them.
            let mut add_lambda_params = |owner: BodyOwner, body: &hir::Body| {
                walk_hir_body(body, |expr_id| {
                    let hir::Expr::Lambda {
                        params,
                        body: lambda_body,
                        ..
                    } = &body.exprs[expr_id]
                    else {
                        return;
                    };

                    // Pick a scope for rename conflict detection.
                    //
                    // Lambda parameters are declared in the lambda scope itself, but locals
                    // declared in a block-bodied lambda live in nested block scopes. If we use the
                    // lambda scope directly, conflict checking won't see those locals. Prefer a
                    // scope *within* the lambda body so both parameters (via parent scopes) and
                    // locals declared in the body are visible.
                    let scope_id = match lambda_body {
                        hir::LambdaBody::Expr(expr) => {
                            // Expression-bodied lambdas can't declare statement locals.
                            scope_result.expr_scopes.get(&(owner, *expr)).copied()
                        }
                        hir::LambdaBody::Block(stmt) => {
                            // Use the scope of the last statement in the lambda body block so
                            // order-sensitive locals are in scope for conflict checks.
                            let body_scope = scope_result.stmt_scopes.get(&(owner, *stmt)).copied();
                            let Some(body_scope) = body_scope else {
                                return;
                            };

                            match &body.stmts[*stmt] {
                                hir::Stmt::Block { statements, .. } => statements
                                    .last()
                                    .and_then(|last| scope_result.stmt_scopes.get(&(owner, *last)))
                                    .copied()
                                    .or(Some(body_scope)),
                                _ => Some(body_scope),
                            }
                        }
                    };
                    let Some(scope_id) = scope_id else {
                        return;
                    };
                    let scope = scope_interner.intern(*file_id, scope_id);

                    for param in params {
                        let local_id = param.local;
                        let local = &body.locals[local_id];
                        candidates.push(SymbolCandidate {
                            key: ResolutionKey::Local(LocalRef {
                                owner,
                                local: local_id,
                            }),
                            file: file.clone(),
                            name: local.name.clone(),
                            name_range: TextRange::new(
                                local.name_range.start,
                                local.name_range.end,
                            ),
                            scope,
                            kind: JavaSymbolKind::Local,
                            type_info: None,
                            method_signature: None,
                        });
                    }
                });
            };

            let mut method_ids: Vec<_> = scope_result.method_scopes.keys().copied().collect();
            method_ids.sort();
            for method in method_ids {
                let owner = BodyOwner::Method(method);
                let body = body_cache
                    .entry(owner)
                    .or_insert_with(|| snap.hir_body(method));
                add_lambda_params(owner, body.as_ref());
            }

            let mut ctor_ids: Vec<_> = scope_result.constructor_scopes.keys().copied().collect();
            ctor_ids.sort();
            for ctor in ctor_ids {
                let owner = BodyOwner::Constructor(ctor);
                let body = body_cache
                    .entry(owner)
                    .or_insert_with(|| snap.hir_constructor_body(ctor));
                add_lambda_params(owner, body.as_ref());
            }

            let mut init_ids: Vec<_> = scope_result.initializer_scopes.keys().copied().collect();
            init_ids.sort();
            for init in init_ids {
                let owner = BodyOwner::Initializer(init);
                let body = body_cache
                    .entry(owner)
                    .or_insert_with(|| snap.hir_initializer_body(init));
                add_lambda_params(owner, body.as_ref());
            }

            scopes.insert(*file_id, scope_result);
        }

        let (type_supertypes, type_subtypes) =
            build_type_hierarchy(&file_ids, &item_trees, &scopes, &resolver);

        candidates.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.name_range.start.cmp(&b.name_range.start))
                .then_with(|| a.name_range.end.cmp(&b.name_range.end))
                .then_with(|| a.name.cmp(&b.name))
        });

        let mut symbols: Vec<SymbolData> = Vec::new();
        let mut references: Vec<Vec<Reference>> = Vec::new();
        let mut spans: Vec<(FileId, TextRange, SymbolId)> = Vec::new();
        let mut name_expr_scopes: HashMap<FileId, HashMap<TextRange, ScopeId>> = HashMap::new();
        let mut resolution_to_symbol: HashMap<ResolutionKey, SymbolId> = HashMap::new();
        let mut top_level_types: HashMap<(Option<String>, String), SymbolId> = HashMap::new();

        for (idx, candidate) in candidates.into_iter().enumerate() {
            let symbol = SymbolId::new(idx as u32);
            if let Some(info) = &candidate.type_info {
                if info.is_top_level {
                    top_level_types
                        .entry((info.package.clone(), candidate.name.clone()))
                        .or_insert(symbol);
                }
            }
            symbols.push(SymbolData {
                def: SymbolDefinition {
                    file: candidate.file.clone(),
                    name: candidate.name.clone(),
                    name_range: candidate.name_range,
                    scope: candidate.scope,
                },
                kind: candidate.kind,
                type_info: candidate.type_info.clone(),
                method_signature: candidate.method_signature.clone(),
            });
            references.push(Vec::new());
            spans.push((candidate.file, candidate.name_range, symbol));
            resolution_to_symbol.insert(candidate.key, symbol);
        }

        // Populate method-group mappings for overloads and attach additional declaration spans.
        for group in &method_groups {
            let Some(&symbol) =
                resolution_to_symbol.get(&ResolutionKey::Method(group.representative))
            else {
                continue;
            };

            for &method_id in &group.method_ids {
                resolution_to_symbol.insert(ResolutionKey::Method(method_id), symbol);
            }

            for &range in &group.decl_ranges {
                references[symbol.as_usize()].push(Reference {
                    file: group.file.clone(),
                    range,
                });
                spans.push((group.file.clone(), range, symbol));
            }
        }

        let mut method_to_symbol: HashMap<MethodId, SymbolId> = HashMap::new();
        for (key, symbol) in &resolution_to_symbol {
            if let ResolutionKey::Method(method_id) = key {
                method_to_symbol.insert(*method_id, *symbol);
            }
        }

        // Treat constructor declarations as references to their enclosing type so `rename` on a
        // class updates `Foo()` -> `Bar()` as well.
        for (ty, refs) in &type_constructor_refs {
            let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Type(*ty)) else {
                continue;
            };
            for (file, range) in refs {
                references[symbol.as_usize()].push(Reference {
                    file: file.clone(),
                    range: *range,
                });
                spans.push((file.clone(), *range, symbol));
            }
        }

        // Collect reference spans by walking HIR bodies and resolving them via the scope graph.
        // Precompute a best-effort inheritance view and type/member maps used for receiver-aware
        // method resolution (e.g. `super::m`, `this::m`).
        let inheritance = snap.project_indexes(project).inheritance.clone();
        let mut type_by_name: HashMap<String, ItemId> = HashMap::new();
        let mut type_name_by_item: HashMap<ItemId, String> = HashMap::new();
        let mut methods_by_item: HashMap<ItemId, HashMap<String, Vec<MethodId>>> = HashMap::new();
        for (_file, file_id) in &file_ids {
            let tree = snap.hir_item_tree(*file_id);
            for item in &tree.items {
                collect_type_maps(
                    tree.as_ref(),
                    *item,
                    &mut type_by_name,
                    &mut type_name_by_item,
                    &mut methods_by_item,
                );
            }
        }
        for (file, file_id) in &file_ids {
            let Some(scope_result) = scopes.get(file_id) else {
                continue;
            };

            let file_text = files.get(file).map(|t| t.as_ref()).unwrap_or("");

            let tree = item_trees
                .get(file_id)
                .cloned()
                .unwrap_or_else(|| snap.hir_item_tree(*file_id));

            // Type references in imports, signatures, annotations, etc (outside method bodies).
            record_non_body_type_references(
                file,
                file_text,
                tree.as_ref(),
                scope_result,
                &resolver,
                &resolution_to_symbol,
                &mut references,
                &mut spans,
            );

            let mut method_ids: Vec<_> = scope_result.method_scopes.keys().copied().collect();
            method_ids.sort();
            for method in method_ids {
                let body = snap.hir_body(method);
                record_body_references(
                    file,
                    file_text,
                    BodyOwner::Method(method),
                    &body,
                    scope_result,
                    &resolver,
                    &workspace_def_map,
                    &item_trees,
                    tree.as_ref(),
                    &resolution_to_symbol,
                    &type_by_name,
                    &type_name_by_item,
                    &methods_by_item,
                    &inheritance,
                    &mut references,
                    &mut spans,
                    &mut name_expr_scopes,
                );
            }

            let mut ctor_ids: Vec<_> = scope_result.constructor_scopes.keys().copied().collect();
            ctor_ids.sort();
            for ctor in ctor_ids {
                let body = snap.hir_constructor_body(ctor);
                record_body_references(
                    file,
                    file_text,
                    BodyOwner::Constructor(ctor),
                    &body,
                    scope_result,
                    &resolver,
                    &workspace_def_map,
                    &item_trees,
                    tree.as_ref(),
                    &resolution_to_symbol,
                    &type_by_name,
                    &type_name_by_item,
                    &methods_by_item,
                    &inheritance,
                    &mut references,
                    &mut spans,
                    &mut name_expr_scopes,
                );
            }

            let mut init_ids: Vec<_> = scope_result.initializer_scopes.keys().copied().collect();
            init_ids.sort();
            for init in init_ids {
                let body = snap.hir_initializer_body(init);
                record_body_references(
                    file,
                    file_text,
                    BodyOwner::Initializer(init),
                    &body,
                    scope_result,
                    &resolver,
                    &workspace_def_map,
                    &item_trees,
                    tree.as_ref(),
                    &resolution_to_symbol,
                    &type_by_name,
                    &type_name_by_item,
                    &methods_by_item,
                    &inheritance,
                    &mut references,
                    &mut spans,
                    &mut name_expr_scopes,
                );
            }

            // Syntax-only references that are not lowered into `hir::Body`.
            let text = files.get(file).map(|s| s.as_ref()).unwrap_or_default();
            record_syntax_only_references(
                file,
                text,
                tree.as_ref(),
                scope_result,
                &snap,
                &item_trees,
                &resolver,
                &workspace_def_map,
                &resolution_to_symbol,
                &mut references,
                &mut spans,
            );

            // Stable HIR currently drops/omits types in a handful of expression-level type
            // positions (casts/instanceof/array creation/explicit generic invocation type args).
            // Traverse the full-fidelity syntax tree and record type references in these
            // positions so `rename` on a type updates them.
            record_syntax_type_references(
                file,
                text,
                tree.as_ref(),
                scope_result,
                &resolver,
                &resolution_to_symbol,
                &mut references,
                &mut spans,
            );
        }

        // Method references in field initializers (e.g. `Supplier<?> s = super::m;`) are not part
        // of Nova's lowered HIR bodies yet. Scan the full-fidelity syntax tree for those so rename
        // can still update them.
        for (file, file_id) in &file_ids {
            let text = files.get(file).map(|t| t.as_ref()).unwrap_or("");
            let tree = snap.hir_item_tree(*file_id);
            record_syntax_method_reference_references(
                file,
                text,
                tree.as_ref(),
                &type_by_name,
                &type_name_by_item,
                &methods_by_item,
                &inheritance,
                &resolution_to_symbol,
                &mut references,
                &mut spans,
            );
        }

        // Additional syntax-based indexing pass: stable HIR drops qualifiers on `this`/`super`
        // expressions (e.g. `Outer.this` / `Outer.super`). Member accesses through these qualified
        // receivers depend on the qualifier type, so we recover them directly from the full
        // syntax tree.
        for (file, file_id) in &file_ids {
            let Some(scope_result) = scopes.get(file_id) else {
                continue;
            };

            let text = files.get(file).map(|t| t.as_ref()).unwrap_or("");
            let parse = nova_syntax::parse_java(text);
            let root = parse.syntax();

            let tree = item_trees
                .get(file_id)
                .cloned()
                .unwrap_or_else(|| snap.hir_item_tree(*file_id));

            record_qualified_receiver_member_references(
                file,
                &root,
                tree.as_ref(),
                scope_result,
                &resolver,
                &workspace_def_map,
                &type_by_name,
                &type_name_by_item,
                &methods_by_item,
                &inheritance,
                &resolution_to_symbol,
                &mut references,
                &mut spans,
            );
        }

        // Ensure we don't materialize overlapping edits if the same reference was indexed by
        // multiple mechanisms (e.g. both HIR and syntax scans).
        for refs in &mut references {
            refs.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then_with(|| a.range.start.cmp(&b.range.start))
                    .then_with(|| a.range.end.cmp(&b.range.end))
            });
            refs.dedup_by(|a, b| a.file == b.file && a.range == b.range);
        }

        // Collect type-parameter reference spans by walking syntax `Type` nodes.
        for (file, file_id) in &file_ids {
            let Some(text) = files.get(file).map(|t| t.as_ref()) else {
                continue;
            };
            let parse = nova_syntax::parse_java(text);
            let tree = item_trees
                .get(file_id)
                .cloned()
                .unwrap_or_else(|| snap.hir_item_tree(*file_id));

            record_type_param_references(
                file,
                &parse,
                tree.as_ref(),
                &resolution_to_symbol,
                &mut references,
                &mut spans,
            );
        }

        spans.sort_by(|(file_a, range_a, sym_a), (file_b, range_b, sym_b)| {
            file_a
                .cmp(file_b)
                .then_with(|| range_a.start.cmp(&range_b.start))
                .then_with(|| range_a.end.cmp(&range_b.end))
                .then_with(|| sym_a.0.cmp(&sym_b.0))
        });
        spans.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);

        Self {
            files,
            db_files: file_ids,
            snap,
            scopes,
            workspace_def_map,
            type_supertypes,
            type_subtypes,
            scope_interner,
            symbols,
            references,
            spans,
            name_expr_scopes,
            resolution_to_symbol,
            top_level_types,
            method_to_symbol,
            method_signatures,
            method_owners,
        }
    }

    pub fn single_file(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new([(FileId::new(path), text.into())])
    }

    pub fn symbol_at(&self, file: &FileId, offset: usize) -> Option<SymbolId> {
        self.spans.iter().find_map(|(span_file, range, symbol)| {
            if span_file == file && range.start <= offset && offset < range.end {
                Some(*symbol)
            } else {
                None
            }
        })
    }

    pub fn symbol_kind(&self, symbol: SymbolId) -> Option<JavaSymbolKind> {
        self.symbols.get(symbol.as_usize()).map(|s| s.kind)
    }

    fn decode_scope(&self, scope: u32) -> Option<(DbFileId, nova_resolve::ScopeId)> {
        self.scope_interner.lookup(scope)
    }
}

fn package_decl_name_range(source: &str) -> Option<TextRange> {
    let parse = nova_syntax::parse_java(source);
    let unit = nova_syntax::CompilationUnit::cast(parse.syntax())
        .expect("root node is a compilation unit");
    let pkg = unit.package()?;
    let name = pkg.name()?;

    // `Name::syntax().text_range()` can include leading trivia in Nova's current tree shapes.
    // For refactorings we want the range of the actual dotted name tokens.
    let mut non_trivia_tokens = name
        .syntax()
        .children_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|tok| !tok.kind().is_trivia());

    let first = non_trivia_tokens.next()?;
    let last = non_trivia_tokens.last().unwrap_or_else(|| first.clone());

    let start = u32::from(first.text_range().start()) as usize;
    let end = u32::from(last.text_range().end()) as usize;
    Some(TextRange::new(start, end))
}

fn refactor_local_scope(
    scope_result: &ScopeBuildResult,
    body: &hir::Body,
    local_scope: nova_resolve::ScopeId,
) -> nova_resolve::ScopeId {
    let (owner, let_stmt_id) = match scope_result.scopes.scope(local_scope).kind() {
        ScopeKind::Block { owner, stmt } if matches!(&body.stmts[*stmt], hir::Stmt::Let { .. }) => {
            (*owner, *stmt)
        }
        _ => {
            return local_scope;
        }
    };

    let mut current = Some(local_scope);
    while let Some(scope_id) = current {
        let data = scope_result.scopes.scope(scope_id);
        let ScopeKind::Block { stmt, .. } = data.kind() else {
            current = data.parent();
            continue;
        };

        match &body.stmts[*stmt] {
            hir::Stmt::For { init, .. } => {
                // `for (int i = 0; ...)` locals are scoped to the `for` statement itself (not the
                // enclosing `{}` block), so don't consider locals declared after the loop.
                if init.iter().any(|stmt_id| *stmt_id == let_stmt_id) {
                    let mut visible_scope = scope_id;
                    for init_stmt in init {
                        if matches!(&body.stmts[*init_stmt], hir::Stmt::Let { .. }) {
                            if let Some(&stmt_scope) =
                                scope_result.stmt_scopes.get(&(owner, *init_stmt))
                            {
                                visible_scope = stmt_scope;
                            }
                        }
                    }
                    return visible_scope;
                }
            }
            hir::Stmt::Block { statements, .. } => {
                if let Some(last_stmt) = statements.last() {
                    return scope_result
                        .stmt_scopes
                        .get(&(owner, *last_stmt))
                        .copied()
                        .unwrap_or(scope_id);
                }
                return scope_id;
            }
            _ => {}
        }

        current = data.parent();
    }

    local_scope
}

fn item_to_item_id(item: Item) -> ItemId {
    match item {
        Item::Class(id) => ItemId::Class(id),
        Item::Interface(id) => ItemId::Interface(id),
        Item::Enum(id) => ItemId::Enum(id),
        Item::Record(id) => ItemId::Record(id),
        Item::Annotation(id) => ItemId::Annotation(id),
    }
}

fn item_name_and_range(tree: &nova_hir::item_tree::ItemTree, item: ItemId) -> (String, TextRange) {
    match item {
        ItemId::Class(id) => {
            let data = tree.class(id);
            (
                data.name.clone(),
                TextRange::new(data.name_range.start, data.name_range.end),
            )
        }
        ItemId::Interface(id) => {
            let data = tree.interface(id);
            (
                data.name.clone(),
                TextRange::new(data.name_range.start, data.name_range.end),
            )
        }
        ItemId::Enum(id) => {
            let data = tree.enum_(id);
            (
                data.name.clone(),
                TextRange::new(data.name_range.start, data.name_range.end),
            )
        }
        ItemId::Record(id) => {
            let data = tree.record(id);
            (
                data.name.clone(),
                TextRange::new(data.name_range.start, data.name_range.end),
            )
        }
        ItemId::Annotation(id) => {
            let data = tree.annotation(id);
            (
                data.name.clone(),
                TextRange::new(data.name_range.start, data.name_range.end),
            )
        }
    }
}

fn item_members<'a>(tree: &'a nova_hir::item_tree::ItemTree, item: ItemId) -> &'a [Member] {
    match item {
        ItemId::Class(id) => &tree.class(id).members,
        ItemId::Interface(id) => &tree.interface(id).members,
        ItemId::Enum(id) => &tree.enum_(id).members,
        ItemId::Record(id) => &tree.record(id).members,
        ItemId::Annotation(id) => &tree.annotation(id).members,
    }
}

fn collect_type_candidates(
    file: &FileId,
    db_file: DbFileId,
    tree: &nova_hir::item_tree::ItemTree,
    scope_result: &ScopeBuildResult,
    decl_scope: nova_resolve::ScopeId,
    item: ItemId,
    is_top_level: bool,
    package: Option<&str>,
    scope_interner: &mut ScopeInterner,
    candidates: &mut Vec<SymbolCandidate>,
    method_groups: &mut Vec<MethodGroupInfo>,
    type_constructor_refs: &mut HashMap<ItemId, Vec<(FileId, TextRange)>>,
    method_signatures: &mut HashMap<MethodId, OverrideMethodSignature>,
    method_owners: &mut HashMap<MethodId, ItemId>,
) {
    // Type declaration.
    let (name, name_range) = item_name_and_range(tree, item);
    let is_public = match item {
        ItemId::Class(id) => tree.class(id).modifiers.raw & HirModifiers::PUBLIC != 0,
        ItemId::Interface(id) => tree.interface(id).modifiers.raw & HirModifiers::PUBLIC != 0,
        ItemId::Enum(id) => tree.enum_(id).modifiers.raw & HirModifiers::PUBLIC != 0,
        ItemId::Record(id) => tree.record(id).modifiers.raw & HirModifiers::PUBLIC != 0,
        ItemId::Annotation(id) => tree.annotation(id).modifiers.raw & HirModifiers::PUBLIC != 0,
    };
    let scope = scope_interner.intern(db_file, decl_scope);
    candidates.push(SymbolCandidate {
        key: ResolutionKey::Type(item),
        file: file.clone(),
        name,
        name_range,
        scope,
        kind: JavaSymbolKind::Type,
        type_info: Some(TypeInfo {
            package: package.map(|p| p.to_string()),
            is_top_level,
            is_public,
        }),
        method_signature: None,
    });

    let Some(&class_scope) = scope_result.class_scopes.get(&item) else {
        return;
    };
    let class_scope_interned = scope_interner.intern(db_file, class_scope);

    // Type parameter declarations.
    let type_params: &[nova_hir::item_tree::TypeParam] = match item {
        ItemId::Class(id) => tree.class(id).type_params.as_slice(),
        ItemId::Interface(id) => tree.interface(id).type_params.as_slice(),
        ItemId::Record(id) => tree.record(id).type_params.as_slice(),
        _ => &[],
    };
    for (idx, tp) in type_params.iter().enumerate() {
        candidates.push(SymbolCandidate {
            key: ResolutionKey::TypeParam(TypeParamKey {
                owner: TypeParamOwner::Type(item),
                index: idx,
            }),
            file: file.clone(),
            name: tp.name.clone(),
            name_range: TextRange::new(tp.name_range.start, tp.name_range.end),
            scope: class_scope_interned,
            kind: JavaSymbolKind::TypeParameter,
            type_info: None,
            method_signature: None,
        });
    }

    // Member declarations.
    let mut methods_by_name: HashMap<String, Vec<(MethodId, TextRange)>> = HashMap::new();

    for member in item_members(tree, item) {
        match member {
            Member::Field(field_id) => {
                let field = tree.field(*field_id);
                candidates.push(SymbolCandidate {
                    key: ResolutionKey::Field(*field_id),
                    file: file.clone(),
                    name: field.name.clone(),
                    name_range: TextRange::new(field.name_range.start, field.name_range.end),
                    scope: class_scope_interned,
                    kind: JavaSymbolKind::Field,
                    type_info: None,
                    method_signature: None,
                });
            }
            Member::Method(method_id) => {
                let method = tree.method(*method_id);
                method_signatures.insert(
                    *method_id,
                    OverrideMethodSignature {
                        name: method.name.clone(),
                        param_types: method
                            .params
                            .iter()
                            .map(|p| p.ty.trim().to_string())
                            .collect(),
                    },
                );
                method_owners.insert(*method_id, item);
                methods_by_name
                    .entry(method.name.clone())
                    .or_default()
                    .push((
                        *method_id,
                        TextRange::new(method.name_range.start, method.name_range.end),
                    ));
            }
            Member::Constructor(ctor_id) => {
                let ctor = tree.constructor(*ctor_id);
                type_constructor_refs.entry(item).or_default().push((
                    file.clone(),
                    TextRange::new(ctor.name_range.start, ctor.name_range.end),
                ));
            }
            Member::Type(child) => {
                let child_id = item_to_item_id(*child);
                collect_type_candidates(
                    file,
                    db_file,
                    tree,
                    scope_result,
                    class_scope,
                    child_id,
                    false,
                    package,
                    scope_interner,
                    candidates,
                    method_groups,
                    type_constructor_refs,
                    method_signatures,
                    method_owners,
                );
            }
            Member::Initializer(_) => {}
        }
    }

    // Method groups (overloads) – one symbol per (containing type, method name).
    for (name, mut methods) in methods_by_name {
        methods.sort_by(|a, b| {
            a.1.start
                .cmp(&b.1.start)
                .then_with(|| a.1.end.cmp(&b.1.end))
        });

        let Some(&(representative, rep_range)) = methods.first() else {
            continue;
        };

        candidates.push(SymbolCandidate {
            key: ResolutionKey::Method(representative),
            file: file.clone(),
            name: name.clone(),
            name_range: rep_range,
            scope: class_scope_interned,
            kind: JavaSymbolKind::Method,
            type_info: None,
            method_signature: Some(SemanticMethodSignature {
                param_types: tree
                    .method(representative)
                    .params
                    .iter()
                    .map(|p| p.ty.trim().to_string())
                    .collect(),
            }),
        });

        method_groups.push(MethodGroupInfo {
            file: file.clone(),
            representative,
            method_ids: methods.iter().map(|(id, _)| *id).collect(),
            decl_ranges: methods.iter().map(|(_, range)| *range).collect(),
        });
    }
}

fn parse_type_name_for_hierarchy(text: &str) -> Option<QualifiedName> {
    let mut s = text.trim();
    if s.is_empty() {
        return None;
    }

    // Skip leading type annotations (`@Foo` / `@foo.Bar(...)`). We do not attempt to parse the
    // full annotation grammar; we just drop the token up to the next whitespace.
    loop {
        let trimmed = s.trim_start();
        if !trimmed.starts_with('@') {
            s = trimmed;
            break;
        }
        let Some(ws) = trimmed.find(|c: char| c.is_whitespace()) else {
            return None;
        };
        s = &trimmed[ws..];
    }

    // Take the first whitespace-delimited token, then strip generics.
    let token = s.split_whitespace().next().unwrap_or("");
    let token = token.split('<').next().unwrap_or("").trim();
    if token.is_empty() || token == "var" {
        return None;
    }

    // Strip array/varargs suffixes.
    let mut token = token;
    while token.ends_with("[]") {
        token = token.strip_suffix("[]").unwrap_or(token);
    }
    while token.ends_with("...") {
        token = token.strip_suffix("...").unwrap_or(token);
    }

    // Replace `$` with `.` so binary nested names resolve as source-like nesting.
    let token = token.replace('$', ".");
    Some(QualifiedName::from_dotted(&token))
}

fn build_type_hierarchy(
    file_ids: &BTreeMap<FileId, DbFileId>,
    item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
    scopes: &HashMap<DbFileId, ScopeBuildResult>,
    resolver: &Resolver<'_>,
) -> (HashMap<ItemId, Vec<ItemId>>, HashMap<ItemId, Vec<ItemId>>) {
    fn collect_item(
        item: ItemId,
        decl_scope: nova_resolve::ScopeId,
        tree: &nova_hir::item_tree::ItemTree,
        scope_result: &ScopeBuildResult,
        resolver: &Resolver<'_>,
        type_supertypes: &mut HashMap<ItemId, Vec<ItemId>>,
        type_subtypes: &mut HashMap<ItemId, Vec<ItemId>>,
    ) {
        let clause_types: Vec<&str> = match item {
            ItemId::Class(id) => {
                let class = tree.class(id);
                class
                    .extends
                    .iter()
                    .chain(class.implements.iter())
                    .map(|s| s.as_str())
                    .collect()
            }
            ItemId::Interface(id) => tree
                .interface(id)
                .extends
                .iter()
                .map(|s| s.as_str())
                .collect(),
            ItemId::Enum(id) => tree
                .enum_(id)
                .implements
                .iter()
                .map(|s| s.as_str())
                .collect(),
            ItemId::Record(id) => tree
                .record(id)
                .implements
                .iter()
                .map(|s| s.as_str())
                .collect(),
            ItemId::Annotation(_) => Vec::new(),
        };

        for super_text in clause_types {
            let Some(path) = parse_type_name_for_hierarchy(super_text) else {
                continue;
            };
            let Some(TypeResolution::Source(super_item)) = resolver
                .resolve_qualified_type_resolution_in_scope(
                    &scope_result.scopes,
                    decl_scope,
                    &path,
                )
            else {
                continue;
            };

            type_supertypes.entry(item).or_default().push(super_item);
            type_subtypes.entry(super_item).or_default().push(item);
        }

        let Some(&class_scope) = scope_result.class_scopes.get(&item) else {
            return;
        };
        for member in item_members(tree, item) {
            let Member::Type(child) = member else {
                continue;
            };
            let child_id = item_to_item_id(*child);
            collect_item(
                child_id,
                class_scope,
                tree,
                scope_result,
                resolver,
                type_supertypes,
                type_subtypes,
            );
        }
    }

    let mut type_supertypes: HashMap<ItemId, Vec<ItemId>> = HashMap::new();
    let mut type_subtypes: HashMap<ItemId, Vec<ItemId>> = HashMap::new();

    for (_file, db_file) in file_ids {
        let Some(tree) = item_trees.get(db_file) else {
            continue;
        };
        let Some(scope_result) = scopes.get(db_file) else {
            continue;
        };

        for item in &tree.items {
            let item_id = item_to_item_id(*item);
            collect_item(
                item_id,
                scope_result.file_scope,
                tree.as_ref(),
                scope_result,
                resolver,
                &mut type_supertypes,
                &mut type_subtypes,
            );
        }
    }

    (type_supertypes, type_subtypes)
}

impl RefactorDatabase for RefactorJavaDatabase {
    fn file_text(&self, file: &FileId) -> Option<&str> {
        self.files.get(file).map(|text| text.as_ref())
    }

    fn all_files(&self) -> Vec<FileId> {
        self.files.keys().cloned().collect()
    }

    fn file_exists(&self, file: &FileId) -> bool {
        self.files.contains_key(file)
    }

    fn symbol_at(&self, file: &FileId, offset: usize) -> Option<SymbolId> {
        RefactorJavaDatabase::symbol_at(self, file, offset)
    }

    fn type_at_offset_display(&self, file: &FileId, offset: usize) -> Option<String> {
        let db_file = *self.db_files.get(file)?;
        self.snap.type_at_offset_display(db_file, offset as u32)
    }

    fn symbol_definition(&self, symbol: SymbolId) -> Option<SymbolDefinition> {
        self.symbols.get(symbol.as_usize()).map(|s| s.def.clone())
    }

    fn symbol_scope(&self, symbol: SymbolId) -> Option<u32> {
        self.symbols.get(symbol.as_usize()).map(|s| s.def.scope)
    }

    fn symbol_kind(&self, symbol: SymbolId) -> Option<JavaSymbolKind> {
        self.symbols.get(symbol.as_usize()).map(|s| s.kind)
    }

    fn type_symbol_info(&self, symbol: SymbolId) -> Option<TypeSymbolInfo> {
        let sym = self.symbols.get(symbol.as_usize())?;
        let info = sym.type_info.as_ref()?;
        Some(TypeSymbolInfo {
            package: info.package.clone(),
            is_top_level: info.is_top_level,
            is_public: info.is_public,
        })
    }

    fn find_top_level_type_in_package(
        &self,
        package: Option<&str>,
        name: &str,
    ) -> Option<SymbolId> {
        let pkg = package.filter(|p| !p.is_empty()).map(|p| p.to_string());
        self.top_level_types.get(&(pkg, name.to_string())).copied()
    }

    fn method_override_chain(&self, symbol: SymbolId) -> Vec<SymbolId> {
        if self.symbol_kind(symbol) != Some(JavaSymbolKind::Method) {
            return vec![symbol];
        }

        // This is a best-effort override chain implementation:
        // - We group overloads by name in `resolution_to_symbol`, so a single symbol may represent
        //   multiple `MethodId`s.
        // - When matching overrides across types, we currently only consider method name +
        //   parameter count (not full type-erased signature).
        fn matching_methods_in_type(
            db: &RefactorJavaDatabase,
            owner: ItemId,
            signature: &OverrideMethodSignature,
        ) -> Vec<MethodId> {
            let Some(def) = db.workspace_def_map.type_def(owner) else {
                return Vec::new();
            };
            let Some(methods) = def.methods.get(&Name::from(signature.name.as_str())) else {
                return Vec::new();
            };

            methods
                .iter()
                .filter_map(|method| {
                    let method_id = method.id;
                    let sig = db.method_signatures.get(&method_id)?;
                    if sig.param_count() == signature.param_count() {
                        Some(method_id)
                    } else {
                        None
                    }
                })
                .collect()
        }

        let mut out: HashSet<SymbolId> = HashSet::new();
        out.insert(symbol);

        // Resolve the set of method IDs covered by this symbol (potentially multiple overloads).
        let method_ids: Vec<MethodId> = self
            .method_to_symbol
            .iter()
            .filter_map(|(method_id, sym)| (*sym == symbol).then_some(*method_id))
            .collect();
        if method_ids.is_empty() {
            return vec![symbol];
        }

        for method_id in method_ids {
            let Some(owner) = self.method_owners.get(&method_id).copied() else {
                continue;
            };
            let Some(signature) = self.method_signatures.get(&method_id).cloned() else {
                continue;
            };

            // Walk up the inheritance chain (overridden methods).
            let mut visited: HashSet<ItemId> = HashSet::new();
            let mut queue: VecDeque<ItemId> = VecDeque::new();
            if let Some(sups) = self.type_supertypes.get(&owner) {
                queue.extend(sups.iter().copied());
            }
            while let Some(ty) = queue.pop_front() {
                if !visited.insert(ty) {
                    continue;
                }

                for method in matching_methods_in_type(self, ty, &signature) {
                    if let Some(sym) = self.method_to_symbol.get(&method).copied() {
                        out.insert(sym);
                    }
                }

                if let Some(sups) = self.type_supertypes.get(&ty) {
                    queue.extend(sups.iter().copied());
                }
            }

            // Walk down the inheritance chain (overriding methods).
            let mut visited: HashSet<ItemId> = HashSet::new();
            let mut queue: VecDeque<ItemId> = VecDeque::new();
            if let Some(subs) = self.type_subtypes.get(&owner) {
                queue.extend(subs.iter().copied());
            }
            while let Some(ty) = queue.pop_front() {
                if !visited.insert(ty) {
                    continue;
                }

                for method in matching_methods_in_type(self, ty, &signature) {
                    if let Some(sym) = self.method_to_symbol.get(&method).copied() {
                        out.insert(sym);
                    }
                }

                if let Some(subs) = self.type_subtypes.get(&ty) {
                    queue.extend(subs.iter().copied());
                }
            }
        }

        let mut out: Vec<_> = out.into_iter().collect();
        out.sort_by_key(|sym| sym.0);
        out
    }

    fn resolve_name_in_scope(&self, scope: u32, name: &str) -> Option<SymbolId> {
        let (file, local_scope) = self.decode_scope(scope)?;
        let scope_result = self.scopes.get(&file)?;
        let data = scope_result.scopes.scope(local_scope);

        let name = Name::from(name);

        if let Some(resolution) = data.values().get(&name) {
            let key = match resolution {
                Resolution::Local(local) => ResolutionKey::Local(*local),
                Resolution::Parameter(param) => ResolutionKey::Param(*param),
                Resolution::Field(field) => ResolutionKey::Field(*field),
                _ => return None,
            };
            return self.resolution_to_symbol.get(&key).copied();
        }

        if let Some(methods) = data.methods().get(&name) {
            let first = methods.first().copied()?;
            return self
                .resolution_to_symbol
                .get(&ResolutionKey::Method(first))
                .copied();
        }

        if let Some(ty) = data.types().get(&name) {
            if let TypeResolution::Source(item) = ty {
                return self
                    .resolution_to_symbol
                    .get(&ResolutionKey::Type(*item))
                    .copied();
            }
        }

        None
    }

    fn resolve_field_in_scope(&self, scope: u32, name: &str) -> Option<SymbolId> {
        let (file, local_scope) = self.decode_scope(scope)?;
        let scope_result = self.scopes.get(&file)?;
        let data = scope_result.scopes.scope(local_scope);

        let name = Name::from(name);
        let Some(resolution) = data.values().get(&name) else {
            return None;
        };
        let Resolution::Field(field_id) = resolution else {
            return None;
        };

        self.resolution_to_symbol
            .get(&ResolutionKey::Field(*field_id))
            .copied()
    }

    fn resolve_methods_in_scope(&self, scope: u32, name: &str) -> Vec<SymbolId> {
        let Some((file, local_scope)) = self.decode_scope(scope) else {
            return Vec::new();
        };
        let Some(scope_result) = self.scopes.get(&file) else {
            return Vec::new();
        };
        let data = scope_result.scopes.scope(local_scope);

        let name = Name::from(name);
        let Some(methods) = data.methods().get(&name) else {
            return Vec::new();
        };

        // Many Nova resolution sites can return multiple overloads; our refactoring symbol space
        // can choose to group overloads (see `method_groups`). Return a stable, deduped list.
        let mut seen: HashSet<SymbolId> = HashSet::new();
        let mut out = Vec::new();
        for method_id in methods {
            let Some(&symbol) = self
                .resolution_to_symbol
                .get(&ResolutionKey::Method(*method_id))
            else {
                continue;
            };
            if seen.insert(symbol) {
                out.push(symbol);
            }
        }
        out
    }

    fn method_signature(&self, symbol: SymbolId) -> Option<SemanticMethodSignature> {
        self.symbols
            .get(symbol.as_usize())
            .and_then(|s| s.method_signature.clone())
    }

    fn would_shadow(&self, scope: u32, name: &str) -> Option<SymbolId> {
        let (file, local_scope) = self.decode_scope(scope)?;
        let scope_result = self.scopes.get(&file)?;

        // Walk parent scopes and report the first symbol that would be shadowed by introducing
        // `name` in the current scope. This is intentionally conservative and includes members and
        // types (not just locals/parameters).
        let name = Name::from(name);
        let mut current = scope_result.scopes.scope(local_scope).parent();
        while let Some(scope_id) = current {
            let data = scope_result.scopes.scope(scope_id);

            if let Some(resolution) = data.values().get(&name) {
                let key = match resolution {
                    Resolution::Local(local) => ResolutionKey::Local(*local),
                    Resolution::Parameter(param) => ResolutionKey::Param(*param),
                    Resolution::Field(field) => ResolutionKey::Field(*field),
                    _ => {
                        current = data.parent();
                        continue;
                    }
                };

                if let Some(symbol) = self.resolution_to_symbol.get(&key).copied() {
                    return Some(symbol);
                }
            }

            if let Some(methods) = data.methods().get(&name) {
                if let Some(&first) = methods.first() {
                    if let Some(symbol) = self
                        .resolution_to_symbol
                        .get(&ResolutionKey::Method(first))
                        .copied()
                    {
                        return Some(symbol);
                    }
                }
            }

            if let Some(ty) = data.types().get(&name) {
                if let TypeResolution::Source(item) = ty {
                    if let Some(symbol) = self
                        .resolution_to_symbol
                        .get(&ResolutionKey::Type(*item))
                        .copied()
                    {
                        return Some(symbol);
                    }
                }
            }

            current = data.parent();
        }

        None
    }

    fn find_references(&self, symbol: SymbolId) -> Vec<Reference> {
        self.references
            .get(symbol.as_usize())
            .cloned()
            .unwrap_or_default()
    }

    fn resolve_name_expr(&self, file: &FileId, range: TextRange) -> Option<SymbolId> {
        let scope = self
            .name_expr_scopes
            .get(file)
            .and_then(|m| m.get(&range))
            .copied()?;

        let db_file = *self.db_files.get(file)?;
        let scope_result = self.scopes.get(&db_file)?;

        let text = self.file_text(file)?;
        if range.end > text.len() {
            return None;
        }
        let name = text.get(range.start..range.end)?;
        if name.is_empty() {
            return None;
        }

        // Re-resolve the identifier in the recorded lexical scope to ensure the
        // range still refers to the intended semantic symbol (important for
        // shadowing-heavy code).
        let jdk = nova_jdk::JdkIndex::new();
        let resolver = Resolver::new(&jdk);
        let resolved = resolver.resolve_name(&scope_result.scopes, scope, &Name::from(name))?;

        match resolved {
            Resolution::Local(local) => self
                .resolution_to_symbol
                .get(&ResolutionKey::Local(local))
                .copied(),
            Resolution::Parameter(param) => self
                .resolution_to_symbol
                .get(&ResolutionKey::Param(param))
                .copied(),
            _ => None,
        }
    }
}

fn record_non_body_type_references(
    file: &FileId,
    file_text: &str,
    tree: &nova_hir::item_tree::ItemTree,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    // Import clauses are scoped to the compilation unit.
    for import in &tree.imports {
        record_type_references_in_range(
            file,
            file_text,
            TextRange::new(import.range.start, import.range.end),
            scope_result.file_scope,
            &scope_result.scopes,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        );
    }

    // JPMS module declarations live at the compilation unit level as well. We only record
    // directives whose grammar contains type names (`uses` / `provides ... with ...`) to avoid
    // accidentally treating exported package names as type references.
    if let Some(module) = tree.module.as_ref() {
        for directive in &module.directives {
            use nova_hir::item_tree::ModuleDirective;
            let range = match directive {
                ModuleDirective::Uses { range, .. } | ModuleDirective::Provides { range, .. } => {
                    *range
                }
                _ => continue,
            };
            record_type_references_in_range(
                file,
                file_text,
                TextRange::new(range.start, range.end),
                scope_result.file_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }

    for item in &tree.items {
        let item_id = item_to_item_id(*item);
        let Some(&class_scope) = scope_result.class_scopes.get(&item_id) else {
            continue;
        };
        record_non_body_type_references_in_item(
            file,
            file_text,
            tree,
            item_id,
            class_scope,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

fn record_non_body_type_references_in_item(
    file: &FileId,
    file_text: &str,
    tree: &nova_hir::item_tree::ItemTree,
    item: ItemId,
    class_scope: nova_resolve::ScopeId,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    fn record_span(
        file: &FileId,
        file_text: &str,
        span: nova_types::Span,
        scope: nova_resolve::ScopeId,
        scopes: &nova_resolve::ScopeGraph,
        resolver: &Resolver<'_>,
        resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
    ) {
        record_type_references_in_range(
            file,
            file_text,
            TextRange::new(span.start, span.end),
            scope,
            scopes,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        );
    }

    fn record_annotations(
        file: &FileId,
        file_text: &str,
        annotations: &[nova_hir::item_tree::AnnotationUse],
        scope: nova_resolve::ScopeId,
        scopes: &nova_resolve::ScopeGraph,
        resolver: &Resolver<'_>,
        resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
    ) {
        for ann in annotations {
            record_span(
                file,
                file_text,
                ann.range,
                scope,
                scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }

    fn record_type_param_bounds(
        file: &FileId,
        file_text: &str,
        type_params: &[nova_hir::item_tree::TypeParam],
        scope: nova_resolve::ScopeId,
        scopes: &nova_resolve::ScopeGraph,
        resolver: &Resolver<'_>,
        resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
    ) {
        for param in type_params {
            for bound in &param.bounds_ranges {
                record_span(
                    file,
                    file_text,
                    *bound,
                    scope,
                    scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
    }

    match item {
        ItemId::Class(id) => {
            let data = tree.class(id);
            record_annotations(
                file,
                file_text,
                &data.annotations,
                class_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_type_param_bounds(
                file,
                file_text,
                &data.type_params,
                class_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            for span in &data.extends_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            for span in &data.implements_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            for span in &data.permits_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        ItemId::Interface(id) => {
            let data = tree.interface(id);
            record_annotations(
                file,
                file_text,
                &data.annotations,
                class_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_type_param_bounds(
                file,
                file_text,
                &data.type_params,
                class_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            for span in &data.extends_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            for span in &data.permits_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        ItemId::Enum(id) => {
            let data = tree.enum_(id);
            record_annotations(
                file,
                file_text,
                &data.annotations,
                class_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            for span in &data.implements_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            for span in &data.permits_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        ItemId::Record(id) => {
            let data = tree.record(id);
            record_annotations(
                file,
                file_text,
                &data.annotations,
                class_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_type_param_bounds(
                file,
                file_text,
                &data.type_params,
                class_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            for span in &data.implements_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            for span in &data.permits_ranges {
                record_span(
                    file,
                    file_text,
                    *span,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            for component in &data.components {
                record_span(
                    file,
                    file_text,
                    component.ty_range,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        ItemId::Annotation(id) => {
            let data = tree.annotation(id);
            record_annotations(
                file,
                file_text,
                &data.annotations,
                class_scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }

    for member in item_members(tree, item) {
        match member {
            Member::Field(field_id) => {
                let field = tree.field(*field_id);
                record_annotations(
                    file,
                    file_text,
                    &field.annotations,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                record_span(
                    file,
                    file_text,
                    field.ty_range,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            Member::Method(method_id) => {
                let method = tree.method(*method_id);
                record_annotations(
                    file,
                    file_text,
                    &method.annotations,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                record_type_param_bounds(
                    file,
                    file_text,
                    &method.type_params,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                record_span(
                    file,
                    file_text,
                    method.return_ty_range,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                for param in &method.params {
                    record_annotations(
                        file,
                        file_text,
                        &param.annotations,
                        class_scope,
                        &scope_result.scopes,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                    record_span(
                        file,
                        file_text,
                        param.ty_range,
                        class_scope,
                        &scope_result.scopes,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
                for span in &method.throws_ranges {
                    record_span(
                        file,
                        file_text,
                        *span,
                        class_scope,
                        &scope_result.scopes,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
            }
            Member::Constructor(ctor_id) => {
                let ctor = tree.constructor(*ctor_id);

                // Canonical record constructors (including compact constructors) declare the record
                // component names as parameters. When renaming a record component, we want to
                // update the corresponding parameter name tokens as well.
                if let ItemId::Record(record_id) = item {
                    let record = tree.record(record_id);
                    if ctor.params.len() == record.components.len()
                        && ctor
                            .params
                            .iter()
                            .zip(record.components.iter())
                            .all(|(p, c)| p.name == c.name && p.ty == c.ty)
                    {
                        let component_fields: Vec<_> = record
                            .members
                            .iter()
                            .filter_map(|member| match member {
                                Member::Field(field_id) => {
                                    let field = tree.field(*field_id);
                                    matches!(
                                        field.kind,
                                        nova_hir::item_tree::FieldKind::RecordComponent
                                    )
                                    .then_some(*field_id)
                                }
                                _ => None,
                            })
                            .collect();

                        if component_fields.len() == ctor.params.len() {
                            for (idx, param) in ctor.params.iter().enumerate() {
                                let field_id = component_fields[idx];
                                let Some(&symbol) =
                                    resolution_to_symbol.get(&ResolutionKey::Field(field_id))
                                else {
                                    continue;
                                };
                                let range =
                                    TextRange::new(param.name_range.start, param.name_range.end);
                                references[symbol.as_usize()].push(Reference {
                                    file: file.clone(),
                                    range,
                                });
                                spans.push((file.clone(), range, symbol));
                            }
                        }
                    }
                }

                record_annotations(
                    file,
                    file_text,
                    &ctor.annotations,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                record_type_param_bounds(
                    file,
                    file_text,
                    &ctor.type_params,
                    class_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                for param in &ctor.params {
                    record_annotations(
                        file,
                        file_text,
                        &param.annotations,
                        class_scope,
                        &scope_result.scopes,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                    record_span(
                        file,
                        file_text,
                        param.ty_range,
                        class_scope,
                        &scope_result.scopes,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
                for span in &ctor.throws_ranges {
                    record_span(
                        file,
                        file_text,
                        *span,
                        class_scope,
                        &scope_result.scopes,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
            }
            Member::Initializer(_) => {}
            Member::Type(child) => {
                let child_id = item_to_item_id(*child);
                let Some(&child_scope) = scope_result.class_scopes.get(&child_id) else {
                    continue;
                };
                record_non_body_type_references_in_item(
                    file,
                    file_text,
                    tree,
                    child_id,
                    child_scope,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
    }
}

fn record_body_references(
    file: &FileId,
    file_text: &str,
    owner: BodyOwner,
    body: &hir::Body,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    workspace_def_map: &WorkspaceDefMap,
    item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
    tree: &nova_hir::item_tree::ItemTree,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    methods_by_item: &HashMap<ItemId, HashMap<String, Vec<MethodId>>>,
    inheritance: &nova_index::InheritanceIndex,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
    name_expr_scopes: &mut HashMap<FileId, HashMap<TextRange, ScopeId>>,
) {
    fn record(
        file: &FileId,
        symbol: SymbolId,
        range: TextRange,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
    ) {
        references[symbol.as_usize()].push(Reference {
            file: file.clone(),
            range,
        });
        spans.push((file.clone(), range, symbol));
    }

    fn type_resolution_scope(
        scopes: &nova_resolve::ScopeGraph,
        scope: nova_resolve::ScopeId,
    ) -> nova_resolve::ScopeId {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = scopes.scope(id);
            match data.kind() {
                ScopeKind::Block { .. }
                | ScopeKind::Lambda { .. }
                | ScopeKind::Method { .. }
                | ScopeKind::Constructor { .. }
                | ScopeKind::Initializer { .. } => {
                    current = data.parent();
                }
                _ => return id,
            }
        }
        scope
    }

    fn parse_type_name(text: &str) -> Option<QualifiedName> {
        let mut s = text.trim();
        if s.is_empty() {
            return None;
        }

        // Skip leading annotations (`@Foo` / `@foo.Bar(...)`). We do not attempt to parse the full
        // annotation grammar; we just drop the token up to the next whitespace.
        loop {
            let trimmed = s.trim_start();
            if !trimmed.starts_with('@') {
                s = trimmed;
                break;
            }
            let Some(ws) = trimmed.find(|c: char| c.is_whitespace()) else {
                return None;
            };
            s = &trimmed[ws..];
        }

        // Take the first whitespace-delimited token (e.g. strip `final` or multiple bounds).
        let token = s.split_whitespace().next().unwrap_or("");
        let token = token.split('<').next().unwrap_or("").trim();
        if token.is_empty() || token == "var" {
            return None;
        }

        // Strip array/varargs suffixes.
        let mut token = token;
        while token.ends_with("[]") {
            token = token.strip_suffix("[]").unwrap_or(token);
        }
        while token.ends_with("...") {
            token = token.strip_suffix("...").unwrap_or(token);
        }

        Some(QualifiedName::from_dotted(token))
    }

    fn enclosing_class(
        scopes: &nova_resolve::ScopeGraph,
        scope: nova_resolve::ScopeId,
    ) -> Option<ItemId> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let data = scopes.scope(id);
            if let ScopeKind::Class { item } = data.kind() {
                return Some(*item);
            }
            current = data.parent();
        }
        None
    }

    fn resolve_type_text(
        scopes: &nova_resolve::ScopeGraph,
        scope: nova_resolve::ScopeId,
        resolver: &Resolver<'_>,
        text: &str,
    ) -> Option<TypeResolution> {
        let path = parse_type_name(text)?;
        resolver.resolve_qualified_type_resolution_in_scope(scopes, scope, &path)
    }

    fn receiver_type(
        owner: BodyOwner,
        body: &hir::Body,
        expr: hir::ExprId,
        scope_result: &ScopeBuildResult,
        resolver: &Resolver<'_>,
        item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
        tree: &nova_hir::item_tree::ItemTree,
    ) -> Option<TypeResolution> {
        let &scope = scope_result.expr_scopes.get(&(owner, expr))?;
        match &body.exprs[expr] {
            hir::Expr::This { .. } | hir::Expr::Super { .. } => {
                let item = enclosing_class(&scope_result.scopes, scope)?;
                Some(TypeResolution::Source(item))
            }
            hir::Expr::FieldAccess { receiver, name, .. } => {
                // `p.Foo` / `Outer.Inner` in a static member selection context (`p.Foo.bar()`).
                //
                // We do not attempt general type inference for arbitrary field-access receivers,
                // but we can best-effort resolve dotted field-access chains that look like
                // qualified type names.
                let path = qualified_name_for_field_access(body, *receiver, name.as_str())?;
                let root = path.segments().first()?;

                if let Some(root_resolved) =
                    resolver.resolve_name(&scope_result.scopes, scope, root)
                {
                    // If the root segment is a value, this is an expression (`obj.foo`), not a
                    // type/package qualification (`p.Foo`), so we can't safely treat it as a type.
                    if matches!(
                        root_resolved,
                        Resolution::Local(_) | Resolution::Parameter(_) | Resolution::Field(_)
                    ) {
                        return None;
                    }
                }

                let scope = type_resolution_scope(&scope_result.scopes, scope);
                resolver.resolve_qualified_type_resolution_in_scope(
                    &scope_result.scopes,
                    scope,
                    &path,
                )
            }
            hir::Expr::Name { name, .. } => {
                let resolved = resolver.resolve_name(
                    &scope_result.scopes,
                    scope,
                    &Name::from(name.as_str()),
                )?;
                match resolved {
                    Resolution::Type(ty) => Some(ty),
                    Resolution::Local(local_ref) => {
                        let local = &body.locals[local_ref.local];
                        let scope = type_resolution_scope(&scope_result.scopes, scope);
                        resolve_type_text(&scope_result.scopes, scope, resolver, &local.ty_text)
                    }
                    Resolution::Parameter(param_ref) => {
                        let ty_text = match param_ref.owner {
                            ParamOwner::Method(method) => tree
                                .method(method)
                                .params
                                .get(param_ref.index)
                                .map(|p| p.ty.as_str()),
                            ParamOwner::Constructor(ctor) => tree
                                .constructor(ctor)
                                .params
                                .get(param_ref.index)
                                .map(|p| p.ty.as_str()),
                        }?;
                        let scope = type_resolution_scope(&scope_result.scopes, scope);
                        resolve_type_text(&scope_result.scopes, scope, resolver, ty_text)
                    }
                    Resolution::Field(field_id) => {
                        let field_tree = item_trees
                            .get(&field_id.file)
                            .map(|t| t.as_ref())
                            .unwrap_or(tree);
                        let ty_text = field_tree.field(field_id).ty.as_str();
                        let scope = type_resolution_scope(&scope_result.scopes, scope);
                        resolve_type_text(&scope_result.scopes, scope, resolver, ty_text)
                    }
                    _ => None,
                }
            }
            hir::Expr::New { class, .. } => {
                let scope = type_resolution_scope(&scope_result.scopes, scope);
                resolve_type_text(&scope_result.scopes, scope, resolver, class)
            }
            _ => None,
        }
    }

    fn qualified_name_for_field_access(
        body: &hir::Body,
        receiver: hir::ExprId,
        name: &str,
    ) -> Option<QualifiedName> {
        fn collect(body: &hir::Body, expr: hir::ExprId, out: &mut Vec<String>) -> bool {
            match &body.exprs[expr] {
                hir::Expr::Name { name, .. } => {
                    out.push(name.clone());
                    true
                }
                hir::Expr::FieldAccess { receiver, name, .. } => {
                    if !collect(body, *receiver, out) {
                        return false;
                    }
                    out.push(name.clone());
                    true
                }
                _ => false,
            }
        }

        let mut segments = Vec::new();
        if !collect(body, receiver, &mut segments) {
            return None;
        }
        segments.push(name.to_string());

        let mut dotted = String::new();
        for (idx, seg) in segments.iter().enumerate() {
            if idx > 0 {
                dotted.push('.');
            }
            dotted.push_str(seg);
        }

        Some(QualifiedName::from_dotted(&dotted))
    }

    // Stable HIR drops qualifiers on `this`/`super` expressions (`Outer.this`, `Outer.super`).
    // Avoid indexing member accesses through these receivers here, since the qualifier affects
    // which member is being referenced. A later syntax-based pass can recover the qualifier and
    // resolve the correct member symbol.
    fn is_qualified_this_or_super(file_text: &str, body: &hir::Body, expr: hir::ExprId) -> bool {
        let (start, end) = match &body.exprs[expr] {
            hir::Expr::This { range } | hir::Expr::Super { range } => (range.start, range.end),
            _ => return false,
        };
        if start >= end || end > file_text.len() {
            return false;
        }
        file_text[start..end].contains('.')
    }

    // Track call callee expressions so we can treat `obj.method()` as a method reference span for
    // the `method` token (rather than a field access span).
    let mut call_callees: HashSet<hir::ExprId> = HashSet::new();
    walk_hir_body(body, |expr_id| {
        if let hir::Expr::Call { callee, .. } = &body.exprs[expr_id] {
            call_callees.insert(*callee);
        }
    });

    walk_hir_body(body, |expr_id| {
        let Some(&scope) = scope_result.expr_scopes.get(&(owner, expr_id)) else {
            return;
        };

        match &body.exprs[expr_id] {
            hir::Expr::Name { name, range } => {
                if call_callees.contains(&expr_id) {
                    return;
                }
                let name_text = name.as_str();
                let name = Name::from(name_text);

                // NOTE: `nova_resolve::ScopeGraph` does not contain inherited fields. If this name
                // is an inherited field reference, `resolve_name` will likely return `Unresolved`.
                // We fall back to an explicit inheritance walk below.
                let resolved_name =
                    resolver.resolve_name_detailed(&scope_result.scopes, scope, &name);
                let resolved = match &resolved_name {
                    NameResolution::Resolved(res) => Some(res.clone()),
                    NameResolution::Ambiguous(_) => return,
                    NameResolution::Unresolved => None,
                }
                .or_else(|| resolver.resolve_method_name(&scope_result.scopes, scope, &name));

                if let Some(resolved) = resolved {
                    let key = match resolved {
                        Resolution::Local(local) => ResolutionKey::Local(local),
                        Resolution::Parameter(param) => {
                            // In record canonical constructors (including compact constructors),
                            // component names are available as constructor parameters. Nova models
                            // these as regular constructor params (so name resolution yields
                            // `Resolution::Parameter`), but for refactoring purposes we want
                            // renaming a record component to also rename uses of the corresponding
                            // constructor parameter.
                            //
                            // We detect this case by checking whether the resolved constructor param
                            // is part of the record's canonical constructor parameter list.
                            let record_component_field = (|| {
                                let ParamOwner::Constructor(ctor) = param.owner else {
                                    return None;
                                };
                                let enclosing_item = enclosing_class(&scope_result.scopes, scope)?;
                                let ItemId::Record(record_id) = enclosing_item else {
                                    return None;
                                };

                                // Only treat canonical constructor parameters (including compact
                                // constructors) as record component references.
                                let record_data = tree.record(record_id);
                                let ctor_data = tree.constructor(ctor);
                                if ctor_data.params.len() != record_data.components.len() {
                                    return None;
                                }
                                if !ctor_data
                                    .params
                                    .iter()
                                    .zip(record_data.components.iter())
                                    .all(|(p, c)| p.name == c.name && p.ty == c.ty)
                                {
                                    return None;
                                }

                                let def = workspace_def_map.type_def(enclosing_item)?;
                                if !matches!(def.kind, TypeKind::Record) {
                                    return None;
                                }
                                let field = def.fields.get(&name).map(|f| f.id)?;
                                let field_tree = item_trees
                                    .get(&field.file)
                                    .map(|t| t.as_ref())
                                    .unwrap_or(tree);
                                if !matches!(
                                    field_tree.field(field).kind,
                                    nova_hir::item_tree::FieldKind::RecordComponent
                                ) {
                                    return None;
                                }

                                Some(field)
                            })();

                            if let Some(field) = record_component_field {
                                ResolutionKey::Field(field)
                            } else {
                                ResolutionKey::Param(param)
                            }
                        }
                        Resolution::Field(field) => ResolutionKey::Field(field),
                        Resolution::Type(TypeResolution::Source(item)) => ResolutionKey::Type(item),
                        Resolution::Type(TypeResolution::External(_)) => return,
                        Resolution::StaticMember(StaticMemberResolution::SourceField(field)) => {
                            ResolutionKey::Field(field)
                        }
                        Resolution::StaticMember(StaticMemberResolution::SourceMethod(method)) => {
                            ResolutionKey::Method(method)
                        }
                        Resolution::StaticMember(StaticMemberResolution::External(_))
                        | Resolution::Methods(_)
                        | Resolution::Constructors(_)
                        | Resolution::Package(_) => return,
                    };
                    let Some(&symbol) = resolution_to_symbol.get(&key) else {
                        return;
                    };
                    let range = TextRange::new(range.start, range.end);
                    // Track the scope where this name expression should be resolved so refactorings
                    // can later validate that a reference range still resolves to the expected
                    // symbol (important for shadowing-heavy code).
                    name_expr_scopes
                        .entry(file.clone())
                        .or_default()
                        .insert(range, scope);
                    record(file, symbol, range, references, spans);
                    return;
                }

                // Best-effort: if this looks like an unqualified inherited field reference, resolve
                // it against the enclosing class + supertypes and record it as a reference to the
                // resolved field symbol.
                if !matches!(resolved_name, NameResolution::Unresolved) {
                    return;
                }
                let Some(enclosing_item) = enclosing_class(&scope_result.scopes, scope) else {
                    return;
                };
                let Some(field) = resolve_field_in_type_or_supertypes(
                    enclosing_item,
                    name_text,
                    workspace_def_map,
                    type_by_name,
                    type_name_by_item,
                    inheritance,
                ) else {
                    return;
                };
                let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Field(field)) else {
                    return;
                };
                let range = TextRange::new(range.start, range.end);
                name_expr_scopes
                    .entry(file.clone())
                    .or_default()
                    .insert(range, scope);
                record(file, symbol, range, references, spans);
            }
            hir::Expr::FieldAccess {
                receiver,
                name,
                name_range,
                ..
            } => {
                if call_callees.contains(&expr_id) {
                    return;
                }

                if is_qualified_this_or_super(file_text, body, *receiver) {
                    return;
                }

                // First, try interpreting the full `receiver.name` chain as a qualified type name.
                // This catches nested/qualified type usages in expression position like:
                //   - `Outer.Inner.m()`
                //   - `com.example.Foo.staticM()`
                if let Some(path) = qualified_name_for_field_access(body, *receiver, name.as_str())
                {
                    if let Some(resolved) = resolver.resolve_qualified_type_resolution_in_scope(
                        &scope_result.scopes,
                        scope,
                        &path,
                    ) {
                        match resolved {
                            TypeResolution::Source(item) => {
                                if let Some(&symbol) =
                                    resolution_to_symbol.get(&ResolutionKey::Type(item))
                                {
                                    let range = TextRange::new(name_range.start, name_range.end);
                                    record(file, symbol, range, references, spans);
                                }
                                return;
                            }
                            TypeResolution::External(_) => {
                                // Treat external types as types too, but only record workspace
                                // symbols for refactorings.
                                return;
                            }
                        }
                    }
                }

                // `super.x` and `this.x` need special casing: `WorkspaceDefMap::type_def` does not
                // include inherited members, and `super` should bind to the direct superclass even
                // if the subclass defines a shadowing field.
                let receiver_kind = match &body.exprs[*receiver] {
                    hir::Expr::This { .. } => Some(ReceiverKind::This),
                    hir::Expr::Super { .. } => Some(ReceiverKind::Super),
                    _ => None,
                };
                if let Some(receiver_kind) = receiver_kind {
                    let Some(enclosing_item) = enclosing_class(&scope_result.scopes, scope) else {
                        return;
                    };
                    if let Some(field) = resolve_receiver_field(
                        enclosing_item,
                        receiver_kind,
                        name.as_str(),
                        workspace_def_map,
                        type_by_name,
                        type_name_by_item,
                        inheritance,
                    ) {
                        if let Some(&symbol) =
                            resolution_to_symbol.get(&ResolutionKey::Field(field))
                        {
                            let range = TextRange::new(name_range.start, name_range.end);
                            record(file, symbol, range, references, spans);
                        }
                    }
                    // Do not fall back to normal receiver-type lookup: `super.x` must not bind to a
                    // shadowing field in the current class.
                    return;
                }

                let Some(receiver_ty) = receiver_type(
                    owner,
                    body,
                    *receiver,
                    scope_result,
                    resolver,
                    item_trees,
                    tree,
                ) else {
                    return;
                };
                let TypeResolution::Source(item) = receiver_ty else {
                    return;
                };
                let Some(field) = resolve_field_in_type_or_supertypes(
                    item,
                    name.as_str(),
                    workspace_def_map,
                    type_by_name,
                    type_name_by_item,
                    inheritance,
                ) else {
                    return;
                };
                let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Field(field)) else {
                    return;
                };
                let range = TextRange::new(name_range.start, name_range.end);
                record(file, symbol, range, references, spans);
            }
            hir::Expr::Call { callee, args, .. } => match &body.exprs[*callee] {
                hir::Expr::Name { name, range } => {
                    let Some(&callee_scope) = scope_result.expr_scopes.get(&(owner, *callee))
                    else {
                        return;
                    };
                    let name = Name::from(name.as_str());
                    let resolved =
                        resolver.resolve_method_name(&scope_result.scopes, callee_scope, &name);

                    // If call-context resolution fails, fall back to treating `foo()` as an
                    // implicit `this.foo()` call and resolve against the enclosing class. This is a
                    // best-effort workaround for incomplete method resolution.
                    let method = match resolved {
                        Some(Resolution::Methods(methods)) => methods.first().copied(),
                        Some(Resolution::StaticMember(StaticMemberResolution::SourceMethod(m))) => {
                            Some(m)
                        }
                        Some(_) => None,
                        None => {
                            let Some(item) = enclosing_class(&scope_result.scopes, callee_scope)
                            else {
                                return;
                            };
                            let Some(def) = workspace_def_map.type_def(item) else {
                                return;
                            };
                            let Some(methods) = def.methods.get(&name) else {
                                // Record component accessors are synthesized by Java, but Nova does
                                // not currently model them as methods. Treat `x()` inside a record
                                // as a reference to the record component field `x`.
                                if !args.is_empty() {
                                    return;
                                }
                                if !matches!(def.kind, TypeKind::Record) {
                                    return;
                                }
                                let Some(field) = def.fields.get(&name).map(|f| f.id) else {
                                    return;
                                };
                                let field_tree = item_trees
                                    .get(&field.file)
                                    .map(|t| t.as_ref())
                                    .unwrap_or(tree);
                                if !matches!(
                                    field_tree.field(field).kind,
                                    nova_hir::item_tree::FieldKind::RecordComponent
                                ) {
                                    return;
                                }
                                let Some(&symbol) =
                                    resolution_to_symbol.get(&ResolutionKey::Field(field))
                                else {
                                    return;
                                };
                                let range = TextRange::new(range.start, range.end);
                                record(file, symbol, range, references, spans);
                                return;
                            };
                            methods.first().map(|m| m.id)
                        }
                    };
                    let Some(method) = method else {
                        return;
                    };
                    let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Method(method))
                    else {
                        return;
                    };
                    let range = TextRange::new(range.start, range.end);
                    record(file, symbol, range, references, spans);
                }
                hir::Expr::FieldAccess {
                    receiver,
                    name,
                    name_range,
                    ..
                } => {
                    if is_qualified_this_or_super(file_text, body, *receiver) {
                        return;
                    }

                    // `WorkspaceDefMap::type_def` only contains methods declared directly on the
                    // type, so member resolution for `this.m()` / `super.m()` needs an explicit
                    // inheritance walk to find inherited methods.
                    let receiver_kind = match &body.exprs[*receiver] {
                        hir::Expr::This { .. } => Some(ReceiverKind::This),
                        hir::Expr::Super { .. } => Some(ReceiverKind::Super),
                        _ => None,
                    };
                    if let Some(receiver_kind) = receiver_kind {
                        if let Some(enclosing_item) = enclosing_class(&scope_result.scopes, scope) {
                            if let Some(method) = resolve_receiver_method(
                                enclosing_item,
                                receiver_kind,
                                name.as_str(),
                                type_by_name,
                                type_name_by_item,
                                methods_by_item,
                                inheritance,
                            ) {
                                if let Some(&symbol) =
                                    resolution_to_symbol.get(&ResolutionKey::Method(method))
                                {
                                    let range = TextRange::new(name_range.start, name_range.end);
                                    record(file, symbol, range, references, spans);
                                    return;
                                }
                            }
                        }
                    }

                    let Some(receiver_ty) = receiver_type(
                        owner,
                        body,
                        *receiver,
                        scope_result,
                        resolver,
                        item_trees,
                        tree,
                    ) else {
                        return;
                    };
                    let TypeResolution::Source(item) = receiver_ty else {
                        return;
                    };
                    let Some(def) = workspace_def_map.type_def(item) else {
                        return;
                    };

                    let method_name = Name::from(name.as_str());
                    if let Some(methods) = def.methods.get(&method_name) {
                        if let Some(method) = methods.first().map(|method| method.id) {
                            if let Some(&symbol) =
                                resolution_to_symbol.get(&ResolutionKey::Method(method))
                            {
                                let range = TextRange::new(name_range.start, name_range.end);
                                record(file, symbol, range, references, spans);
                            }
                        }
                        return;
                    }

                    // Record component accessors are synthesized by Java, but Nova does not
                    // currently model them as methods. Treat `p.x()` as a reference to the record
                    // component field `x`.
                    if !args.is_empty() {
                        return;
                    }
                    if !matches!(def.kind, TypeKind::Record) {
                        return;
                    }
                    let Some(field) = def.fields.get(&method_name).map(|f| f.id) else {
                        return;
                    };
                    let field_tree = item_trees
                        .get(&field.file)
                        .map(|t| t.as_ref())
                        .unwrap_or(tree);
                    if !matches!(
                        field_tree.field(field).kind,
                        nova_hir::item_tree::FieldKind::RecordComponent
                    ) {
                        return;
                    }
                    let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Field(field))
                    else {
                        return;
                    };
                    let range = TextRange::new(name_range.start, name_range.end);
                    record(file, symbol, range, references, spans);
                }
                _ => {}
            },
            hir::Expr::MethodReference {
                receiver,
                name,
                name_range,
                ..
            } => {
                if is_qualified_this_or_super(file_text, body, *receiver) {
                    return;
                }

                // `WorkspaceDefMap::type_def` does not include inherited methods, so resolve
                // `this::m` / `super::m` via an explicit inheritance walk.
                let receiver_kind = match &body.exprs[*receiver] {
                    hir::Expr::This { .. } => Some(ReceiverKind::This),
                    hir::Expr::Super { .. } => Some(ReceiverKind::Super),
                    _ => None,
                };
                if let Some(receiver_kind) = receiver_kind {
                    if let Some(enclosing_item) = enclosing_class(&scope_result.scopes, scope) {
                        if let Some(method) = resolve_receiver_method(
                            enclosing_item,
                            receiver_kind,
                            name.as_str(),
                            type_by_name,
                            type_name_by_item,
                            methods_by_item,
                            inheritance,
                        ) {
                            if let Some(&symbol) =
                                resolution_to_symbol.get(&ResolutionKey::Method(method))
                            {
                                let range = TextRange::new(name_range.start, name_range.end);
                                record(file, symbol, range, references, spans);
                                return;
                            }
                        }
                    }
                }

                let Some(receiver_ty) = receiver_type(
                    owner,
                    body,
                    *receiver,
                    scope_result,
                    resolver,
                    item_trees,
                    tree,
                ) else {
                    return;
                };
                let TypeResolution::Source(item) = receiver_ty else {
                    return;
                };
                let Some(def) = workspace_def_map.type_def(item) else {
                    return;
                };
                let method_name = Name::from(name.as_str());
                if let Some(methods) = def.methods.get(&method_name) {
                    if let Some(method) = methods.first().map(|method| method.id) {
                        if let Some(&symbol) =
                            resolution_to_symbol.get(&ResolutionKey::Method(method))
                        {
                            let range = TextRange::new(name_range.start, name_range.end);
                            record(file, symbol, range, references, spans);
                        }
                    }
                    return;
                }

                // Same record-component accessor fallback as method calls: `P::x` should be treated
                // as a reference to the record component `x`.
                if !matches!(def.kind, TypeKind::Record) {
                    return;
                }
                let Some(field) = def.fields.get(&method_name).map(|f| f.id) else {
                    return;
                };
                let field_tree = item_trees
                    .get(&field.file)
                    .map(|t| t.as_ref())
                    .unwrap_or(tree);
                if !matches!(
                    field_tree.field(field).kind,
                    nova_hir::item_tree::FieldKind::RecordComponent
                ) {
                    return;
                }
                let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Field(field)) else {
                    return;
                };
                let range = TextRange::new(name_range.start, name_range.end);
                record(file, symbol, range, references, spans);
            }
            hir::Expr::New { class_range, .. } => {
                let type_scope = type_resolution_scope(&scope_result.scopes, scope);
                record_type_references_in_range(
                    file,
                    file_text,
                    TextRange::new(class_range.start, class_range.end),
                    type_scope,
                    &scope_result.scopes,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            _ => {}
        }
    });

    // Type usage in local variable declarations (`TypeName x = ...`).
    fn walk_stmt(body: &hir::Body, stmt: hir::StmtId, f: &mut impl FnMut(hir::StmtId)) {
        f(stmt);
        match &body.stmts[stmt] {
            hir::Stmt::Block { statements, .. } => {
                for stmt in statements {
                    walk_stmt(body, *stmt, f);
                }
            }
            hir::Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                walk_stmt(body, *then_branch, f);
                if let Some(stmt) = else_branch {
                    walk_stmt(body, *stmt, f);
                }
            }
            hir::Stmt::While { body: inner, .. } => walk_stmt(body, *inner, f),
            hir::Stmt::For {
                init, body: inner, ..
            } => {
                for stmt in init {
                    walk_stmt(body, *stmt, f);
                }
                walk_stmt(body, *inner, f);
            }
            hir::Stmt::ForEach { body: inner, .. } => walk_stmt(body, *inner, f),
            hir::Stmt::Switch { body: inner, .. } => walk_stmt(body, *inner, f),
            hir::Stmt::Try {
                body: inner,
                catches,
                finally,
                ..
            } => {
                walk_stmt(body, *inner, f);
                for catch in catches {
                    walk_stmt(body, catch.body, f);
                }
                if let Some(stmt) = finally {
                    walk_stmt(body, *stmt, f);
                }
            }
            _ => {}
        }
    }

    walk_stmt(body, body.root, &mut |stmt_id| {
        let (local_id, stmt_scope, use_parent_scope) = match &body.stmts[stmt_id] {
            hir::Stmt::Let { local, .. } => (
                *local,
                scope_result.stmt_scopes.get(&(owner, stmt_id)).copied(),
                true,
            ),
            hir::Stmt::ForEach { local, .. } => (
                *local,
                scope_result.stmt_scopes.get(&(owner, stmt_id)).copied(),
                false,
            ),
            _ => return,
        };

        let Some(stmt_scope) = stmt_scope else {
            return;
        };
        let type_scope = if use_parent_scope {
            scope_result
                .scopes
                .scope(stmt_scope)
                .parent()
                .unwrap_or(stmt_scope)
        } else {
            stmt_scope
        };
        let type_scope = type_resolution_scope(&scope_result.scopes, type_scope);

        let local = &body.locals[local_id];
        record_type_references_in_range(
            file,
            file_text,
            TextRange::new(local.ty_range.start, local.ty_range.end),
            type_scope,
            &scope_result.scopes,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        );
    });
}

fn record_syntax_type_references(
    file: &FileId,
    file_text: &str,
    tree: &nova_hir::item_tree::ItemTree,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    if file_text.is_empty() {
        return;
    }

    let parse = nova_syntax::parse_java(file_text);
    let root = parse.syntax();

    let mut type_bodies: Vec<(ItemId, nova_types::Span)> = Vec::new();
    for item in &tree.items {
        collect_type_body_ranges_in_item(tree, item_to_item_id(*item), &mut type_bodies);
    }

    for node in root.descendants() {
        if let Some(ty) = ast::Type::cast(node.clone()) {
            let range = syntax_node_text_range(ty.syntax());
            let scope = scope_for_syntax_offset(range.start, &type_bodies, scope_result);
            record_type_references_in_range(
                file,
                file_text,
                range,
                scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }

        if let Some(type_args) = ast::TypeArguments::cast(node) {
            let range = syntax_node_text_range(type_args.syntax());
            let scope = scope_for_syntax_offset(range.start, &type_bodies, scope_result);
            record_type_references_in_range(
                file,
                file_text,
                range,
                scope,
                &scope_result.scopes,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }
}

fn syntax_node_text_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn scope_for_syntax_offset(
    offset: usize,
    type_bodies: &[(ItemId, nova_types::Span)],
    scope_result: &ScopeBuildResult,
) -> nova_resolve::ScopeId {
    let mut best: Option<(ItemId, usize)> = None;
    for (item, body_range) in type_bodies {
        if body_range.start <= offset && offset < body_range.end {
            let len = body_range.end.saturating_sub(body_range.start);
            match best {
                None => best = Some((*item, len)),
                Some((_, best_len)) if len < best_len => best = Some((*item, len)),
                _ => {}
            }
        }
    }

    match best {
        Some((item, _)) => scope_result
            .class_scopes
            .get(&item)
            .copied()
            .unwrap_or(scope_result.file_scope),
        None => scope_result.file_scope,
    }
}

fn collect_type_body_ranges_in_item(
    tree: &nova_hir::item_tree::ItemTree,
    item: ItemId,
    out: &mut Vec<(ItemId, nova_types::Span)>,
) {
    out.push((item, item_body_range(tree, item)));

    for member in item_members(tree, item) {
        let Member::Type(child) = member else {
            continue;
        };
        collect_type_body_ranges_in_item(tree, item_to_item_id(*child), out);
    }
}

fn item_body_range(tree: &nova_hir::item_tree::ItemTree, item: ItemId) -> nova_types::Span {
    match item {
        ItemId::Class(id) => tree.class(id).body_range,
        ItemId::Interface(id) => tree.interface(id).body_range,
        ItemId::Enum(id) => tree.enum_(id).body_range,
        ItemId::Record(id) => tree.record(id).body_range,
        ItemId::Annotation(id) => tree.annotation(id).body_range,
    }
}

fn record_type_references_in_range(
    file: &FileId,
    file_text: &str,
    range: TextRange,
    scope: nova_resolve::ScopeId,
    scopes: &nova_resolve::ScopeGraph,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    if range.start >= range.end || range.end > file_text.len() {
        return;
    }
    let Some(slice) = file_text.get(range.start..range.end) else {
        return;
    };
    let bytes = slice.as_bytes();
    let mut i = 0usize;

    while i < slice.len() {
        // Skip comments/strings so we don't accidentally record identifiers that appear in trivia
        // (e.g. `@Named("Foo")` should not rename `"Foo"`).
        let b = bytes[i];
        match b {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Line comment.
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Block comment.
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
            b'"' => {
                // String literal or text block (`""" ... """`).
                if i + 2 < bytes.len() && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    i += 3;
                    while i + 2 < bytes.len() {
                        if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                            i += 3;
                            break;
                        }
                        i += 1;
                    }
                } else {
                    i += 1;
                    while i < bytes.len() {
                        match bytes[i] {
                            b'\\' => i = (i + 2).min(bytes.len()),
                            b'"' => {
                                i += 1;
                                break;
                            }
                            _ => i += 1,
                        }
                    }
                }
                continue;
            }
            b'\'' => {
                // Char literal.
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i = (i + 2).min(bytes.len()),
                        b'\'' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                continue;
            }
            _ => {}
        }

        let ch = slice[i..].chars().next().unwrap();
        if !is_ident_start_char(ch) {
            i += ch.len_utf8();
            continue;
        }

        let token_start = i;
        i += ch.len_utf8();

        // Parse a dotted identifier path (`foo.bar.Baz`).
        while i < slice.len() {
            let ch = slice[i..].chars().next().unwrap();
            if is_ident_continue_char(ch) {
                i += ch.len_utf8();
                continue;
            }

            if ch == '.' || ch == '$' {
                let sep_len = ch.len_utf8();
                let after = i + sep_len;
                if after < slice.len() {
                    let next = slice[after..].chars().next().unwrap();
                    if is_ident_start_char(next) {
                        // Include the separator and keep scanning.
                        i = after + next.len_utf8();
                        continue;
                    }
                }
            }

            break;
        }

        let token_end = i;
        if token_end <= token_start {
            continue;
        }
        let token = &slice[token_start..token_end];
        if token == "var" {
            continue;
        }

        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut seg_start = 0usize;
        for (idx, ch) in token.char_indices() {
            if ch == '.' || ch == '$' {
                if seg_start < idx {
                    segments.push((seg_start, idx));
                }
                seg_start = idx + ch.len_utf8();
            }
        }
        if seg_start < token.len() {
            segments.push((seg_start, token.len()));
        }
        if segments.is_empty() {
            continue;
        }

        // Resolve each prefix so we can record references to nested/outer types. For a token like
        // `Outer.Inner`, both `Outer` and `Inner` are type references.
        for prefix_len in 1..=segments.len() {
            let mut prefix = String::new();
            for (idx, (start, end)) in segments.iter().copied().take(prefix_len).enumerate() {
                if idx > 0 {
                    prefix.push('.');
                }
                prefix.push_str(&token[start..end]);
            }

            let path = QualifiedName::from_dotted(&prefix);
            let Some(TypeResolution::Source(item)) =
                resolver.resolve_qualified_type_resolution_in_scope(scopes, scope, &path)
            else {
                continue;
            };

            let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Type(item)) else {
                continue;
            };

            let (start, end) = segments[prefix_len - 1];
            let abs_range = TextRange::new(
                range.start + token_start + start,
                range.start + token_start + end,
            );
            references[symbol.as_usize()].push(Reference {
                file: file.clone(),
                range: abs_range,
            });
            spans.push((file.clone(), abs_range, symbol));
        }
    }
}

fn record_type_param_references(
    file: &FileId,
    parse: &nova_syntax::JavaParseResult,
    tree: &nova_hir::item_tree::ItemTree,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    fn record(
        file: &FileId,
        symbol: SymbolId,
        range: TextRange,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
    ) {
        references[symbol.as_usize()].push(Reference {
            file: file.clone(),
            range,
        });
        spans.push((file.clone(), range, symbol));
    }

    fn record_type_usages_in_range(
        file: &FileId,
        parse: &nova_syntax::JavaParseResult,
        owner_range: TextRange,
        type_param_name: &str,
        symbol: SymbolId,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
    ) {
        for ty in parse.syntax().descendants().filter_map(ast::Type::cast) {
            let node_range = ty.syntax().text_range();
            let start = u32::from(node_range.start()) as usize;
            let end = u32::from(node_range.end()) as usize;
            if start < owner_range.start || end > owner_range.end {
                continue;
            }

            let Some(named) = ty.named() else {
                continue;
            };

            // Named types are represented as identifier-like tokens and `.` separators directly
            // under the `NamedType` node (not as an `ast::Name` node). A type-parameter reference
            // is always a simple (unqualified) identifier.
            if named
                .syntax()
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|tok| tok.kind() == nova_syntax::SyntaxKind::Dot)
            {
                continue;
            }

            let mut idents = named
                .syntax()
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|tok| tok.kind().is_identifier_like())
                .collect::<Vec<_>>();

            if idents.len() != 1 {
                continue;
            }

            let ident = idents.pop().expect("idents.len() == 1");
            if ident.text() != type_param_name {
                continue;
            }

            let range = token_text_range(&ident);
            record(file, symbol, range, references, spans);
        }
    }

    fn item_range(tree: &nova_hir::item_tree::ItemTree, item: ItemId) -> Option<nova_types::Span> {
        Some(match item {
            ItemId::Class(id) => tree.class(id).range,
            ItemId::Interface(id) => tree.interface(id).range,
            ItemId::Enum(id) => tree.enum_(id).range,
            ItemId::Record(id) => tree.record(id).range,
            ItemId::Annotation(id) => tree.annotation(id).range,
        })
    }

    fn record_item(
        file: &FileId,
        parse: &nova_syntax::JavaParseResult,
        tree: &nova_hir::item_tree::ItemTree,
        item: ItemId,
        resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
    ) {
        let Some(owner_span) = item_range(tree, item) else {
            return;
        };
        let owner_range = TextRange::new(owner_span.start, owner_span.end);

        let type_params: &[nova_hir::item_tree::TypeParam] = match item {
            ItemId::Class(id) => tree.class(id).type_params.as_slice(),
            ItemId::Interface(id) => tree.interface(id).type_params.as_slice(),
            ItemId::Record(id) => tree.record(id).type_params.as_slice(),
            _ => &[],
        };
        for (idx, tp) in type_params.iter().enumerate() {
            let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::TypeParam(TypeParamKey {
                owner: TypeParamOwner::Type(item),
                index: idx,
            })) else {
                continue;
            };
            record_type_usages_in_range(
                file,
                parse,
                owner_range,
                &tp.name,
                symbol,
                references,
                spans,
            );
        }

        for member in item_members(tree, item) {
            match member {
                Member::Method(method_id) => {
                    let method = tree.method(*method_id);
                    let range = TextRange::new(method.range.start, method.range.end);
                    for (idx, tp) in method.type_params.iter().enumerate() {
                        let Some(&symbol) =
                            resolution_to_symbol.get(&ResolutionKey::TypeParam(TypeParamKey {
                                owner: TypeParamOwner::Method(*method_id),
                                index: idx,
                            }))
                        else {
                            continue;
                        };
                        record_type_usages_in_range(
                            file, parse, range, &tp.name, symbol, references, spans,
                        );
                    }
                }
                Member::Constructor(ctor_id) => {
                    let ctor = tree.constructor(*ctor_id);
                    let range = TextRange::new(ctor.range.start, ctor.range.end);
                    for (idx, tp) in ctor.type_params.iter().enumerate() {
                        let Some(&symbol) =
                            resolution_to_symbol.get(&ResolutionKey::TypeParam(TypeParamKey {
                                owner: TypeParamOwner::Constructor(*ctor_id),
                                index: idx,
                            }))
                        else {
                            continue;
                        };
                        record_type_usages_in_range(
                            file, parse, range, &tp.name, symbol, references, spans,
                        );
                    }
                }
                Member::Type(child) => {
                    let child_id = item_to_item_id(*child);
                    record_item(
                        file,
                        parse,
                        tree,
                        child_id,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
                Member::Field(_) | Member::Initializer(_) => {}
            }
        }
    }

    for item in &tree.items {
        let item_id = item_to_item_id(*item);
        record_item(
            file,
            parse,
            tree,
            item_id,
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

fn token_text_range(token: &nova_syntax::SyntaxToken) -> TextRange {
    let range = token.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn is_ident_start_char(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_' || ch == '$'
}

fn is_ident_continue_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverKind {
    This,
    Super,
}

fn resolve_receiver_method(
    enclosing_item: ItemId,
    receiver_kind: ReceiverKind,
    name: &str,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    methods_by_item: &HashMap<ItemId, HashMap<String, Vec<MethodId>>>,
    inheritance: &nova_index::InheritanceIndex,
) -> Option<MethodId> {
    match receiver_kind {
        ReceiverKind::This => resolve_method_in_type_or_supertypes(
            enclosing_item,
            name,
            type_by_name,
            type_name_by_item,
            methods_by_item,
            inheritance,
        ),
        ReceiverKind::Super => {
            let super_item =
                direct_super_item(enclosing_item, type_by_name, type_name_by_item, inheritance)?;
            resolve_method_in_type_or_supertypes(
                super_item,
                name,
                type_by_name,
                type_name_by_item,
                methods_by_item,
                inheritance,
            )
        }
    }
}

fn resolve_receiver_field(
    enclosing_item: ItemId,
    receiver_kind: ReceiverKind,
    name: &str,
    workspace: &WorkspaceDefMap,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    inheritance: &nova_index::InheritanceIndex,
) -> Option<FieldId> {
    match receiver_kind {
        ReceiverKind::This => resolve_field_in_type_or_supertypes(
            enclosing_item,
            name,
            workspace,
            type_by_name,
            type_name_by_item,
            inheritance,
        ),
        ReceiverKind::Super => {
            let super_item =
                direct_super_item(enclosing_item, type_by_name, type_name_by_item, inheritance)?;
            resolve_field_in_type_or_supertypes(
                super_item,
                name,
                workspace,
                type_by_name,
                type_name_by_item,
                inheritance,
            )
        }
    }
}

fn direct_super_item(
    subtype: ItemId,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    inheritance: &nova_index::InheritanceIndex,
) -> Option<ItemId> {
    let subtype_name = type_name_by_item.get(&subtype)?;
    let supertypes = inheritance.supertypes.get(subtype_name)?;

    // Best-effort: prefer a class supertype (so `super` does not bind to an implemented interface).
    for super_name in supertypes {
        if let Some(super_item) = type_by_name.get(super_name) {
            if matches!(super_item, ItemId::Class(_)) {
                return Some(*super_item);
            }
        }
    }

    for super_name in supertypes {
        if let Some(super_item) = type_by_name.get(super_name) {
            return Some(*super_item);
        }
    }

    None
}

fn resolve_field_in_type_or_supertypes(
    ty: ItemId,
    name: &str,
    workspace: &WorkspaceDefMap,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    inheritance: &nova_index::InheritanceIndex,
) -> Option<FieldId> {
    let mut visited = HashSet::<ItemId>::new();
    resolve_field_in_type_or_supertypes_impl(
        ty,
        name,
        workspace,
        type_by_name,
        type_name_by_item,
        inheritance,
        &mut visited,
    )
}

fn resolve_field_in_type_or_supertypes_impl(
    ty: ItemId,
    name: &str,
    workspace: &WorkspaceDefMap,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    inheritance: &nova_index::InheritanceIndex,
    visited: &mut HashSet<ItemId>,
) -> Option<FieldId> {
    if !visited.insert(ty) {
        return None;
    }

    if let Some(def) = workspace.type_def(ty) {
        if let Some(field) = def.fields.get(&Name::from(name)) {
            return Some(field.id);
        }
    }

    let ty_name = type_name_by_item.get(&ty)?;
    let Some(supertypes) = inheritance.supertypes.get(ty_name) else {
        return None;
    };

    for super_name in supertypes {
        let Some(super_item) = type_by_name.get(super_name).copied() else {
            continue;
        };
        if let Some(found) = resolve_field_in_type_or_supertypes_impl(
            super_item,
            name,
            workspace,
            type_by_name,
            type_name_by_item,
            inheritance,
            visited,
        ) {
            return Some(found);
        }
    }

    None
}

fn resolve_method_in_type_or_supertypes(
    ty: ItemId,
    name: &str,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    methods_by_item: &HashMap<ItemId, HashMap<String, Vec<MethodId>>>,
    inheritance: &nova_index::InheritanceIndex,
) -> Option<MethodId> {
    let mut visited = HashSet::<ItemId>::new();
    resolve_method_in_type_or_supertypes_impl(
        ty,
        name,
        type_by_name,
        type_name_by_item,
        methods_by_item,
        inheritance,
        &mut visited,
    )
}

fn resolve_method_in_type_or_supertypes_impl(
    ty: ItemId,
    name: &str,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    methods_by_item: &HashMap<ItemId, HashMap<String, Vec<MethodId>>>,
    inheritance: &nova_index::InheritanceIndex,
    visited: &mut HashSet<ItemId>,
) -> Option<MethodId> {
    if !visited.insert(ty) {
        return None;
    }

    if let Some(methods) = methods_by_item.get(&ty).and_then(|m| m.get(name)) {
        if let Some(method) = methods.first().copied() {
            return Some(method);
        }
    }

    let ty_name = type_name_by_item.get(&ty)?;
    let Some(supertypes) = inheritance.supertypes.get(ty_name) else {
        return None;
    };

    for super_name in supertypes {
        let Some(super_item) = type_by_name.get(super_name).copied() else {
            continue;
        };
        if let Some(found) = resolve_method_in_type_or_supertypes_impl(
            super_item,
            name,
            type_by_name,
            type_name_by_item,
            methods_by_item,
            inheritance,
            visited,
        ) {
            return Some(found);
        }
    }

    None
}

fn collect_type_maps(
    tree: &ItemTree,
    item: item_tree::Item,
    type_by_name: &mut HashMap<String, ItemId>,
    type_name_by_item: &mut HashMap<ItemId, String>,
    methods_by_item: &mut HashMap<ItemId, HashMap<String, Vec<MethodId>>>,
) {
    let item_id = item_id(item);
    let (name, members) = item_name_and_members(tree, item_id);

    type_by_name.entry(name.to_string()).or_insert(item_id);
    type_name_by_item.insert(item_id, name.to_string());

    let mut method_map: HashMap<String, Vec<MethodId>> = HashMap::new();
    for member in members {
        match member {
            item_tree::Member::Method(method_id) => {
                let method = tree.method(*method_id);
                method_map
                    .entry(method.name.clone())
                    .or_default()
                    .push(*method_id);
            }
            item_tree::Member::Type(child) => {
                collect_type_maps(
                    tree,
                    *child,
                    type_by_name,
                    type_name_by_item,
                    methods_by_item,
                );
            }
            _ => {}
        }
    }
    methods_by_item.insert(item_id, method_map);
}

fn record_syntax_method_reference_references(
    file: &FileId,
    text: &str,
    tree: &ItemTree,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    methods_by_item: &HashMap<ItemId, HashMap<String, Vec<MethodId>>>,
    inheritance: &nova_index::InheritanceIndex,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    let parse = nova_syntax::parse_java(text);
    let root = parse.syntax();

    for mr in root
        .descendants()
        .filter_map(|node| ast::MethodReferenceExpression::cast(node))
    {
        let Some(name_tok) = mr.name_token() else {
            continue;
        };
        let name = name_tok.text().to_string();
        let name_range = syntax_token_range(&name_tok);

        let Some(receiver) = mr.expression() else {
            continue;
        };

        // Only handle the unqualified `this::m` / `super::m` forms (mirrors `hir::Expr::{This,Super}`).
        let receiver_kind = match receiver {
            ast::Expression::ThisExpression(this_expr) => this_expr
                .qualifier()
                .is_none()
                .then_some(ReceiverKind::This),
            ast::Expression::SuperExpression(super_expr) => super_expr
                .qualifier()
                .is_none()
                .then_some(ReceiverKind::Super),
            _ => None,
        };
        let Some(receiver_kind) = receiver_kind else {
            continue;
        };

        let Some(enclosing_item) = enclosing_item_at_offset(tree, name_range.start) else {
            continue;
        };

        let Some(method) = resolve_receiver_method(
            enclosing_item,
            receiver_kind,
            &name,
            type_by_name,
            type_name_by_item,
            methods_by_item,
            inheritance,
        ) else {
            continue;
        };

        record_reference(
            file,
            name_range,
            ResolutionKey::Method(method),
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn record_qualified_receiver_member_references(
    file: &FileId,
    root: &nova_syntax::SyntaxNode,
    tree: &ItemTree,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    workspace_def_map: &WorkspaceDefMap,
    type_by_name: &HashMap<String, ItemId>,
    type_name_by_item: &HashMap<ItemId, String>,
    methods_by_item: &HashMap<ItemId, HashMap<String, Vec<MethodId>>>,
    inheritance: &nova_index::InheritanceIndex,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    fn syntax_node_text_no_trivia(node: &nova_syntax::SyntaxNode) -> String {
        let mut out = String::new();
        for tok in node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| !tok.kind().is_trivia())
        {
            out.push_str(tok.text());
        }
        out
    }

    fn expr_to_qualified_name(expr: ast::Expression) -> Option<QualifiedName> {
        match expr {
            ast::Expression::NameExpression(name_expr) => {
                let text = ast::support::child::<ast::Name>(name_expr.syntax())
                    .map(|name| name.text())
                    .unwrap_or_else(|| syntax_node_text_no_trivia(name_expr.syntax()));
                (!text.is_empty()).then_some(QualifiedName::from_dotted(&text))
            }
            ast::Expression::FieldAccessExpression(field_access) => {
                let prefix = expr_to_qualified_name(field_access.expression()?)?;
                let seg = field_access.name_token()?.text().to_string();
                Some(QualifiedName::from_dotted(&format!(
                    "{}.{seg}",
                    prefix.to_dotted()
                )))
            }
            _ => None,
        }
    }

    fn qualified_receiver(expr: ast::Expression) -> Option<(ReceiverKind, QualifiedName)> {
        match expr {
            ast::Expression::ThisExpression(this_expr) => Some((
                ReceiverKind::This,
                expr_to_qualified_name(this_expr.qualifier()?)?,
            )),
            ast::Expression::SuperExpression(super_expr) => Some((
                ReceiverKind::Super,
                expr_to_qualified_name(super_expr.qualifier()?)?,
            )),
            ast::Expression::ParenthesizedExpression(parenthesized) => {
                qualified_receiver(parenthesized.expression()?)
            }
            _ => None,
        }
    }

    // `Outer.this.m(...)` / `Outer.super.m(...)` method calls.
    for call in root
        .descendants()
        .filter_map(ast::MethodCallExpression::cast)
    {
        let Some(ast::Expression::FieldAccessExpression(field_access)) = call.callee() else {
            continue;
        };

        let Some(receiver) = field_access.expression() else {
            continue;
        };
        let Some((receiver_kind, qual_path)) = qualified_receiver(receiver) else {
            continue;
        };

        let Some(name_tok) = field_access.name_token() else {
            continue;
        };
        let method_name = name_tok.text();
        let name_range = syntax_token_range(&name_tok);

        let start_scope = enclosing_item_at_offset(tree, name_range.start)
            .and_then(|item| scope_result.class_scopes.get(&item).copied())
            .unwrap_or(scope_result.file_scope);

        let Some(TypeResolution::Source(item)) = resolver
            .resolve_qualified_type_resolution_in_scope(
                &scope_result.scopes,
                start_scope,
                &qual_path,
            )
        else {
            continue;
        };

        let Some(method) = resolve_receiver_method(
            item,
            receiver_kind,
            method_name,
            type_by_name,
            type_name_by_item,
            methods_by_item,
            inheritance,
        ) else {
            continue;
        };

        record_reference(
            file,
            name_range,
            ResolutionKey::Method(method),
            resolution_to_symbol,
            references,
            spans,
        );
    }

    // `Outer.this::m` / `Outer.super::m` method references.
    for method_ref in root
        .descendants()
        .filter_map(ast::MethodReferenceExpression::cast)
    {
        let Some(receiver) = method_ref.expression() else {
            continue;
        };
        let Some((receiver_kind, qual_path)) = qualified_receiver(receiver) else {
            continue;
        };

        let Some(name_tok) = method_ref.name_token() else {
            continue;
        };
        let method_name = name_tok.text();
        let name_range = syntax_token_range(&name_tok);

        let start_scope = enclosing_item_at_offset(tree, name_range.start)
            .and_then(|item| scope_result.class_scopes.get(&item).copied())
            .unwrap_or(scope_result.file_scope);

        let Some(TypeResolution::Source(item)) = resolver
            .resolve_qualified_type_resolution_in_scope(
                &scope_result.scopes,
                start_scope,
                &qual_path,
            )
        else {
            continue;
        };

        let Some(method) = resolve_receiver_method(
            item,
            receiver_kind,
            method_name,
            type_by_name,
            type_name_by_item,
            methods_by_item,
            inheritance,
        ) else {
            continue;
        };

        record_reference(
            file,
            name_range,
            ResolutionKey::Method(method),
            resolution_to_symbol,
            references,
            spans,
        );
    }

    // `Outer.this.foo` / `Outer.super.foo` field accesses.
    for field_access in root
        .descendants()
        .filter_map(ast::FieldAccessExpression::cast)
    {
        // Skip method-call callees (`Outer.this.m()`), handled above.
        if field_access
            .syntax()
            .parent()
            .and_then(ast::MethodCallExpression::cast)
            .is_some()
        {
            continue;
        }

        let Some(receiver) = field_access.expression() else {
            continue;
        };
        let Some((receiver_kind, qual_path)) = qualified_receiver(receiver) else {
            continue;
        };

        let Some(name_tok) = field_access.name_token() else {
            continue;
        };
        let field_name = name_tok.text();
        let name_range = syntax_token_range(&name_tok);

        let start_scope = enclosing_item_at_offset(tree, name_range.start)
            .and_then(|item| scope_result.class_scopes.get(&item).copied())
            .unwrap_or(scope_result.file_scope);

        let Some(TypeResolution::Source(item)) = resolver
            .resolve_qualified_type_resolution_in_scope(
                &scope_result.scopes,
                start_scope,
                &qual_path,
            )
        else {
            continue;
        };

        let Some(field) = resolve_receiver_field(
            item,
            receiver_kind,
            field_name,
            workspace_def_map,
            type_by_name,
            type_name_by_item,
            inheritance,
        ) else {
            continue;
        };

        record_reference(
            file,
            name_range,
            ResolutionKey::Field(field),
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

fn enclosing_item_at_offset(tree: &ItemTree, offset: usize) -> Option<ItemId> {
    fn walk(tree: &ItemTree, item: ItemId, offset: usize, best: &mut Option<(usize, ItemId)>) {
        let (body_range, members) = item_body_range_and_members(tree, item);
        if !(body_range.start <= offset && offset < body_range.end) {
            return;
        }

        let len = body_range.end.saturating_sub(body_range.start);
        match best {
            Some((best_len, _)) if *best_len <= len => {}
            _ => {
                *best = Some((len, item));
            }
        }

        for member in members {
            if let item_tree::Member::Type(child) = member {
                walk(tree, item_id(*child), offset, best);
            }
        }
    }

    let mut best: Option<(usize, ItemId)> = None;
    for item in &tree.items {
        walk(tree, item_id(*item), offset, &mut best);
    }
    best.map(|(_, item)| item)
}

fn item_id(item: item_tree::Item) -> ItemId {
    match item {
        item_tree::Item::Class(id) => ItemId::Class(id),
        item_tree::Item::Interface(id) => ItemId::Interface(id),
        item_tree::Item::Enum(id) => ItemId::Enum(id),
        item_tree::Item::Record(id) => ItemId::Record(id),
        item_tree::Item::Annotation(id) => ItemId::Annotation(id),
    }
}

fn item_name_and_members<'a>(
    tree: &'a ItemTree,
    item: ItemId,
) -> (&'a str, &'a [item_tree::Member]) {
    match item {
        ItemId::Class(id) => {
            let data = tree.class(id);
            (data.name.as_str(), data.members.as_slice())
        }
        ItemId::Interface(id) => {
            let data = tree.interface(id);
            (data.name.as_str(), data.members.as_slice())
        }
        ItemId::Enum(id) => {
            let data = tree.enum_(id);
            (data.name.as_str(), data.members.as_slice())
        }
        ItemId::Record(id) => {
            let data = tree.record(id);
            (data.name.as_str(), data.members.as_slice())
        }
        ItemId::Annotation(id) => {
            let data = tree.annotation(id);
            (data.name.as_str(), data.members.as_slice())
        }
    }
}

fn item_body_range_and_members<'a>(
    tree: &'a ItemTree,
    item: ItemId,
) -> (nova_types::Span, &'a [item_tree::Member]) {
    match item {
        ItemId::Class(id) => {
            let data = tree.class(id);
            (data.body_range, data.members.as_slice())
        }
        ItemId::Interface(id) => {
            let data = tree.interface(id);
            (data.body_range, data.members.as_slice())
        }
        ItemId::Enum(id) => {
            let data = tree.enum_(id);
            (data.body_range, data.members.as_slice())
        }
        ItemId::Record(id) => {
            let data = tree.record(id);
            (data.body_range, data.members.as_slice())
        }
        ItemId::Annotation(id) => {
            let data = tree.annotation(id);
            (data.body_range, data.members.as_slice())
        }
    }
}

fn walk_hir_body(body: &hir::Body, mut f: impl FnMut(hir::ExprId)) {
    fn walk_stmt(body: &hir::Body, stmt: hir::StmtId, f: &mut impl FnMut(hir::ExprId)) {
        match &body.stmts[stmt] {
            hir::Stmt::Block { statements, .. } => {
                for stmt in statements {
                    walk_stmt(body, *stmt, f);
                }
            }
            hir::Stmt::Let { initializer, .. } => {
                if let Some(expr) = initializer {
                    walk_expr(body, *expr, f);
                }
            }
            hir::Stmt::Expr { expr, .. } => walk_expr(body, *expr, f),
            hir::Stmt::Assert {
                condition, message, ..
            } => {
                walk_expr(body, *condition, f);
                if let Some(expr) = message {
                    walk_expr(body, *expr, f);
                }
            }
            hir::Stmt::Return { expr, .. } => {
                if let Some(expr) = expr {
                    walk_expr(body, *expr, f);
                }
            }
            hir::Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                walk_expr(body, *condition, f);
                walk_stmt(body, *then_branch, f);
                if let Some(stmt) = else_branch {
                    walk_stmt(body, *stmt, f);
                }
            }
            hir::Stmt::While {
                condition,
                body: inner,
                ..
            } => {
                walk_expr(body, *condition, f);
                walk_stmt(body, *inner, f);
            }
            hir::Stmt::For {
                init,
                condition,
                update,
                body: inner,
                ..
            } => {
                for stmt in init {
                    walk_stmt(body, *stmt, f);
                }
                if let Some(expr) = condition {
                    walk_expr(body, *expr, f);
                }
                for expr in update {
                    walk_expr(body, *expr, f);
                }
                walk_stmt(body, *inner, f);
            }
            hir::Stmt::ForEach {
                iterable,
                body: inner,
                ..
            } => {
                walk_expr(body, *iterable, f);
                walk_stmt(body, *inner, f);
            }
            hir::Stmt::Synchronized {
                expr, body: inner, ..
            } => {
                walk_expr(body, *expr, f);
                walk_stmt(body, *inner, f);
            }
            hir::Stmt::Switch {
                selector,
                body: inner,
                ..
            } => {
                walk_expr(body, *selector, f);
                walk_stmt(body, *inner, f);
            }
            hir::Stmt::Try {
                body: inner,
                catches,
                finally,
                ..
            } => {
                walk_stmt(body, *inner, f);
                for catch in catches {
                    walk_stmt(body, catch.body, f);
                }
                if let Some(finally) = finally {
                    walk_stmt(body, *finally, f);
                }
            }
            hir::Stmt::Throw { expr, .. } => walk_expr(body, *expr, f),
            hir::Stmt::Break { .. } | hir::Stmt::Continue { .. } | hir::Stmt::Empty { .. } => {}
        }
    }

    fn walk_expr(body: &hir::Body, expr: hir::ExprId, f: &mut impl FnMut(hir::ExprId)) {
        f(expr);
        match &body.exprs[expr] {
            hir::Expr::Name { .. }
            | hir::Expr::Literal { .. }
            | hir::Expr::Null { .. }
            | hir::Expr::This { .. }
            | hir::Expr::Super { .. }
            | hir::Expr::Missing { .. } => {}
            hir::Expr::FieldAccess { receiver, .. }
            | hir::Expr::MethodReference { receiver, .. }
            | hir::Expr::ConstructorReference { receiver, .. } => walk_expr(body, *receiver, f),
            hir::Expr::ArrayAccess { array, index, .. } => {
                walk_expr(body, *array, f);
                walk_expr(body, *index, f);
            }
            hir::Expr::ClassLiteral { ty, .. } => walk_expr(body, *ty, f),
            hir::Expr::Call { callee, args, .. } => {
                walk_expr(body, *callee, f);
                for arg in args {
                    walk_expr(body, *arg, f);
                }
            }
            hir::Expr::New { args, .. } => {
                for arg in args {
                    walk_expr(body, *arg, f);
                }
            }
            hir::Expr::ArrayCreation { dim_exprs, .. } => {
                for dim in dim_exprs {
                    walk_expr(body, *dim, f);
                }
            }
            hir::Expr::Unary { expr, .. } => walk_expr(body, *expr, f),
            hir::Expr::Cast { expr, .. } => walk_expr(body, *expr, f),
            hir::Expr::Binary { lhs, rhs, .. } | hir::Expr::Assign { lhs, rhs, .. } => {
                walk_expr(body, *lhs, f);
                walk_expr(body, *rhs, f);
            }
            hir::Expr::Instanceof { expr, .. } => walk_expr(body, *expr, f),
            hir::Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                walk_expr(body, *condition, f);
                walk_expr(body, *then_expr, f);
                walk_expr(body, *else_expr, f);
            }
            hir::Expr::Lambda {
                body: lambda_body, ..
            } => match lambda_body {
                hir::LambdaBody::Expr(expr) => walk_expr(body, *expr, f),
                hir::LambdaBody::Block(stmt) => walk_stmt(body, *stmt, f),
            },
            hir::Expr::Invalid { children, .. } => {
                for child in children {
                    walk_expr(body, *child, f);
                }
            }
        }
    }

    walk_stmt(body, body.root, &mut f);
}

fn syntax_token_range(token: &nova_syntax::SyntaxToken) -> TextRange {
    let range = token.text_range();
    TextRange::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn collect_ident_segments(node: &nova_syntax::SyntaxNode) -> Vec<(String, TextRange)> {
    node.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| tok.kind().is_identifier_like())
        .map(|tok| (tok.text().to_string(), syntax_token_range(&tok)))
        .collect()
}

/// Collect the identifier segments that make up a named type reference.
///
/// `NamedType` is token-based in the AST schema. We intentionally only look at its *direct*
/// identifier-like tokens so we do not accidentally include tokens from nested `TypeArguments`
/// (`List<Foo>` → `List`, `Foo` is handled by its own `ast::Type` node).
fn collect_named_type_segments(named: &ast::NamedType) -> Vec<(String, TextRange)> {
    named
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| tok.kind().is_identifier_like())
        .map(|tok| (tok.text().to_string(), syntax_token_range(&tok)))
        .collect()
}

fn resolution_to_key(res: Resolution, accept_methods: bool) -> Option<ResolutionKey> {
    match res {
        Resolution::Field(field) => Some(ResolutionKey::Field(field)),
        Resolution::Type(TypeResolution::Source(item)) => Some(ResolutionKey::Type(item)),
        Resolution::StaticMember(StaticMemberResolution::SourceField(field)) => {
            Some(ResolutionKey::Field(field))
        }
        Resolution::Methods(methods) if accept_methods => {
            methods.first().copied().map(ResolutionKey::Method)
        }
        Resolution::StaticMember(StaticMemberResolution::SourceMethod(method))
            if accept_methods =>
        {
            Some(ResolutionKey::Method(method))
        }
        _ => None,
    }
}

fn record_reference(
    file: &FileId,
    range: TextRange,
    key: ResolutionKey,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    let Some(&symbol) = resolution_to_symbol.get(&key) else {
        return;
    };
    references[symbol.as_usize()].push(Reference {
        file: file.clone(),
        range,
    });
    spans.push((file.clone(), range, symbol));
}

fn resolve_member_in_type(
    workspace: &WorkspaceDefMap,
    owner: ItemId,
    member: &str,
    accept_methods: bool,
) -> Option<ResolutionKey> {
    let name = Name::from(member);
    let ty = workspace.type_def(owner)?;
    if let Some(field) = ty.fields.get(&name) {
        return Some(ResolutionKey::Field(field.id));
    }
    if accept_methods {
        if let Some(methods) = ty.methods.get(&name) {
            return methods
                .first()
                .map(|method| ResolutionKey::Method(method.id));
        }
    }
    None
}

fn resolve_static_import_member_in_type(
    workspace: &WorkspaceDefMap,
    owner: ItemId,
    member: &str,
) -> Option<ResolutionKey> {
    let name = Name::from(member);
    let ty = workspace.type_def(owner)?;

    // Static imports can import member types as well (`import static java.util.Map.Entry;`).
    // If a nested type shares the same name, treat the import as ambiguous so we don't
    // accidentally rewrite an import that must continue importing other members/types.
    if ty.nested_types.contains_key(&name) {
        return None;
    }

    let field = ty.fields.get(&name).filter(|f| f.is_static).map(|f| f.id);

    let mut methods: Vec<MethodId> = ty
        .methods
        .get(&name)
        .map(|methods| {
            methods
                .iter()
                .filter(|m| m.is_static)
                .map(|m| m.id)
                .collect()
        })
        .unwrap_or_default();
    methods.sort();

    match (field, methods.as_slice()) {
        (Some(field), []) => Some(ResolutionKey::Field(field)),
        (None, [method]) => Some(ResolutionKey::Method(*method)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameExprContext {
    Value,
    Type,
    MethodCallee,
}

fn name_expr_context(expr: &nova_syntax::SyntaxNode) -> NameExprContext {
    let Some(parent) = expr.parent() else {
        return NameExprContext::Value;
    };
    if ast::MethodCallExpression::cast(parent.clone()).is_some() {
        return NameExprContext::MethodCallee;
    }
    if ast::ClassLiteralExpression::cast(parent).is_some() {
        return NameExprContext::Type;
    }
    NameExprContext::Value
}

fn resolve_type_from_segments(
    resolver: &Resolver<'_>,
    scopes: &nova_resolve::ScopeGraph,
    scope: nova_resolve::ScopeId,
    segments: &[(String, TextRange)],
) -> Option<TypeResolution> {
    let dotted = segments
        .iter()
        .map(|(s, _)| s.as_str())
        .collect::<Vec<_>>()
        .join(".");
    let qn = QualifiedName::from_dotted(&dotted);
    resolver.resolve_qualified_type_resolution_in_scope(scopes, scope, &qn)
}

/// Record references for each prefix of a qualified name that resolves to a source type.
///
/// This is required so `Outer.Inner` counts as a reference to both `Outer` and `Inner`.
fn record_type_prefix_references(
    file: &FileId,
    scope: nova_resolve::ScopeId,
    segments: &[(String, TextRange)],
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    for idx in 0..segments.len() {
        let prefix = &segments[..=idx];
        let Some(TypeResolution::Source(item)) =
            resolve_type_from_segments(resolver, &scope_result.scopes, scope, prefix)
        else {
            continue;
        };
        let range = prefix[idx].1;
        record_reference(
            file,
            range,
            ResolutionKey::Type(item),
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

fn process_name_expression(
    file: &FileId,
    scope: nova_resolve::ScopeId,
    scopes: &nova_resolve::ScopeGraph,
    resolver: &Resolver<'_>,
    workspace: &WorkspaceDefMap,
    name_expr: ast::NameExpression,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    let segments = collect_ident_segments(name_expr.syntax());
    let Some((last_name, last_range)) = segments.last().cloned() else {
        return;
    };

    match name_expr_context(name_expr.syntax()) {
        NameExprContext::Type => {
            let Some(TypeResolution::Source(item)) =
                resolve_type_from_segments(resolver, scopes, scope, &segments)
            else {
                return;
            };
            record_reference(
                file,
                last_range,
                ResolutionKey::Type(item),
                resolution_to_symbol,
                references,
                spans,
            );
        }
        NameExprContext::MethodCallee => {
            // `foo()` or `Type.foo()`.
            if segments.len() == 1 {
                let Some(resolved) =
                    resolver.resolve_method_name(scopes, scope, &Name::from(last_name.as_str()))
                else {
                    return;
                };
                let method = match resolved {
                    Resolution::Methods(methods) => methods.first().copied(),
                    Resolution::StaticMember(StaticMemberResolution::SourceMethod(method)) => {
                        Some(method)
                    }
                    _ => None,
                };
                let Some(method) = method else {
                    return;
                };
                record_reference(
                    file,
                    last_range,
                    ResolutionKey::Method(method),
                    resolution_to_symbol,
                    references,
                    spans,
                );
                return;
            }

            let owner_segments = &segments[..segments.len() - 1];
            let Some(TypeResolution::Source(owner)) =
                resolve_type_from_segments(resolver, scopes, scope, owner_segments)
            else {
                return;
            };

            if let Some(owner_last_range) = owner_segments.last().map(|(_, r)| *r) {
                record_reference(
                    file,
                    owner_last_range,
                    ResolutionKey::Type(owner),
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }

            let Some(key) = resolve_member_in_type(workspace, owner, &last_name, true) else {
                return;
            };
            if matches!(key, ResolutionKey::Method(_)) {
                record_reference(
                    file,
                    last_range,
                    key,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        NameExprContext::Value => {
            if segments.len() == 1 {
                let Some(res) = resolver.resolve_name(scopes, scope, &Name::from(last_name)) else {
                    return;
                };
                let Some(key) = resolution_to_key(res, /*accept_methods*/ false) else {
                    return;
                };
                record_reference(
                    file,
                    last_range,
                    key,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                return;
            }

            // Prefer type resolution (e.g. `pkg.Foo`) before treating as `Type.FIELD`.
            if let Some(TypeResolution::Source(item)) =
                resolve_type_from_segments(resolver, scopes, scope, &segments)
            {
                record_reference(
                    file,
                    last_range,
                    ResolutionKey::Type(item),
                    resolution_to_symbol,
                    references,
                    spans,
                );
                return;
            }

            let owner_segments = &segments[..segments.len() - 1];
            let Some(TypeResolution::Source(owner)) =
                resolve_type_from_segments(resolver, scopes, scope, owner_segments)
            else {
                return;
            };

            if let Some(owner_last_range) = owner_segments.last().map(|(_, r)| *r) {
                record_reference(
                    file,
                    owner_last_range,
                    ResolutionKey::Type(owner),
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }

            let Some(key) = resolve_member_in_type(workspace, owner, &last_name, false) else {
                return;
            };
            record_reference(
                file,
                last_range,
                key,
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }
}

fn resolve_receiver_type(
    resolver: &Resolver<'_>,
    scopes: &nova_resolve::ScopeGraph,
    scope: nova_resolve::ScopeId,
    receiver: ast::Expression,
) -> Option<TypeResolution> {
    match receiver {
        ast::Expression::NameExpression(ne) => {
            let segments = collect_ident_segments(ne.syntax());
            resolve_type_from_segments(resolver, scopes, scope, &segments)
        }
        _ => None,
    }
}

fn process_field_access_expression(
    file: &FileId,
    scope: nova_resolve::ScopeId,
    scopes: &nova_resolve::ScopeGraph,
    resolver: &Resolver<'_>,
    workspace: &WorkspaceDefMap,
    field_access: ast::FieldAccessExpression,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    let Some(name_tok) = field_access.name_token() else {
        return;
    };
    let name = name_tok.text().to_string();
    let name_range = syntax_token_range(&name_tok);

    let is_callee = field_access
        .syntax()
        .parent()
        .and_then(ast::MethodCallExpression::cast)
        .is_some();

    if let Some(receiver) = field_access.expression() {
        if let Some(TypeResolution::Source(owner)) =
            resolve_receiver_type(resolver, scopes, scope, receiver)
        {
            if let Some(key) = resolve_member_in_type(workspace, owner, &name, is_callee) {
                // In callee position (`Type.foo()`), only record the method, not a field.
                if !is_callee || matches!(key, ResolutionKey::Method(_)) {
                    record_reference(
                        file,
                        name_range,
                        key,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
                return;
            }
        }
    }

    // Fallback: attempt unqualified resolution for `this.foo` / `super.foo`-like expressions.
    let Some(res) = resolver.resolve_name(scopes, scope, &Name::from(name)) else {
        return;
    };
    let Some(key) = resolution_to_key(res, is_callee) else {
        return;
    };
    if !is_callee || matches!(key, ResolutionKey::Method(_)) {
        record_reference(
            file,
            name_range,
            key,
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

fn record_expression_references(
    file: &FileId,
    file_text: &str,
    expr: ast::Expression,
    scope: nova_resolve::ScopeId,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    workspace: &WorkspaceDefMap,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    // Process root.
    match expr.clone() {
        ast::Expression::NameExpression(ne) => process_name_expression(
            file,
            scope,
            &scope_result.scopes,
            resolver,
            workspace,
            ne,
            resolution_to_symbol,
            references,
            spans,
        ),
        ast::Expression::FieldAccessExpression(fa) => process_field_access_expression(
            file,
            scope,
            &scope_result.scopes,
            resolver,
            workspace,
            fa,
            resolution_to_symbol,
            references,
            spans,
        ),
        _ => {}
    }

    // Process descendants.
    for node in expr.syntax().descendants() {
        let Some(expr) = ast::Expression::cast(node) else {
            continue;
        };
        match expr {
            ast::Expression::NameExpression(ne) => process_name_expression(
                file,
                scope,
                &scope_result.scopes,
                resolver,
                workspace,
                ne,
                resolution_to_symbol,
                references,
                spans,
            ),
            ast::Expression::FieldAccessExpression(fa) => process_field_access_expression(
                file,
                scope,
                &scope_result.scopes,
                resolver,
                workspace,
                fa,
                resolution_to_symbol,
                references,
                spans,
            ),
            ast::Expression::MethodReferenceExpression(mr) => {
                let Some(name_tok) = mr.name_token() else {
                    continue;
                };
                let name = name_tok.text().to_string();
                let name_range = syntax_token_range(&name_tok);
                let Some(receiver) = mr.expression() else {
                    continue;
                };
                let Some(TypeResolution::Source(owner)) =
                    resolve_receiver_type(resolver, &scope_result.scopes, scope, receiver)
                else {
                    continue;
                };
                let Some(key) = resolve_member_in_type(workspace, owner, &name, true) else {
                    continue;
                };
                if matches!(key, ResolutionKey::Method(_)) {
                    record_reference(
                        file,
                        name_range,
                        key,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
            }
            _ => {}
        }
    }

    // Also record any type references that occur in nested `Type` nodes (e.g. `new Foo()`,
    // casts, `instanceof`, array creation, lambda parameter types). These types are not modeled
    // in HIR bodies for syntax-only contexts like enum constant argument lists.
    for node in expr.syntax().descendants() {
        let Some(ty) = ast::Type::cast(node) else {
            continue;
        };
        let range = ty.syntax().text_range();
        let start = u32::from(range.start()) as usize;
        let end = u32::from(range.end()) as usize;
        record_type_references_in_range(
            file,
            file_text,
            TextRange::new(start, end),
            scope,
            &scope_result.scopes,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct SwitchContext {
    scope: ScopeId,
    selector_enum: Option<ItemId>,
}

fn collect_switch_contexts(
    body: &hir::Body,
    owner: BodyOwner,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
    out: &mut HashMap<usize, SwitchContext>,
) {
    fn walk_stmt(
        body: &hir::Body,
        stmt: hir::StmtId,
        owner: BodyOwner,
        scope_result: &ScopeBuildResult,
        resolver: &Resolver<'_>,
        item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
        out: &mut HashMap<usize, SwitchContext>,
    ) {
        match &body.stmts[stmt] {
            hir::Stmt::Block { statements, .. } => {
                for stmt in statements {
                    walk_stmt(body, *stmt, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::Let { initializer, .. } => {
                if let Some(expr) = initializer {
                    walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::Expr { expr, .. } => {
                walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
            }
            hir::Stmt::Assert {
                condition, message, ..
            } => {
                walk_expr(
                    body,
                    *condition,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                if let Some(expr) = message {
                    walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::Return { expr, .. } => {
                if let Some(expr) = expr {
                    walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::Assert {
                condition,
                message,
                ..
            } => {
                walk_expr(body, *condition, owner, scope_result, resolver, item_trees, out);
                if let Some(expr) = message {
                    walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                walk_expr(
                    body,
                    *condition,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                walk_stmt(
                    body,
                    *then_branch,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                if let Some(stmt) = else_branch {
                    walk_stmt(body, *stmt, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::While {
                condition,
                body: inner,
                ..
            } => {
                walk_expr(
                    body,
                    *condition,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                walk_stmt(body, *inner, owner, scope_result, resolver, item_trees, out);
            }
            hir::Stmt::For {
                init,
                condition,
                update,
                body: inner,
                ..
            } => {
                for stmt in init {
                    walk_stmt(body, *stmt, owner, scope_result, resolver, item_trees, out);
                }
                if let Some(expr) = condition {
                    walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
                }
                for expr in update {
                    walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
                }
                walk_stmt(body, *inner, owner, scope_result, resolver, item_trees, out);
            }
            hir::Stmt::ForEach {
                iterable,
                body: inner,
                ..
            } => {
                walk_expr(
                    body,
                    *iterable,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                walk_stmt(body, *inner, owner, scope_result, resolver, item_trees, out);
            }
            hir::Stmt::Synchronized {
                expr, body: inner, ..
            } => {
                walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
                walk_stmt(body, *inner, owner, scope_result, resolver, item_trees, out);
            }
            hir::Stmt::Switch {
                selector,
                body: inner,
                range,
                ..
            } => {
                let Some(&scope) = scope_result.expr_scopes.get(&(owner, *selector)) else {
                    walk_stmt(body, *inner, owner, scope_result, resolver, item_trees, out);
                    return;
                };

                let selector_enum = infer_switch_selector_enum_type(
                    body,
                    selector,
                    scope,
                    scope_result,
                    resolver,
                    item_trees,
                );

                out.entry(range.start).or_insert(SwitchContext {
                    scope,
                    selector_enum,
                });
                walk_stmt(body, *inner, owner, scope_result, resolver, item_trees, out);
            }
            hir::Stmt::Try {
                body: inner,
                catches,
                finally,
                ..
            } => {
                walk_stmt(body, *inner, owner, scope_result, resolver, item_trees, out);
                for catch in catches {
                    walk_stmt(
                        body,
                        catch.body,
                        owner,
                        scope_result,
                        resolver,
                        item_trees,
                        out,
                    );
                }
                if let Some(finally) = finally {
                    walk_stmt(
                        body,
                        *finally,
                        owner,
                        scope_result,
                        resolver,
                        item_trees,
                        out,
                    );
                }
            }
            hir::Stmt::Assert {
                condition, message, ..
            } => {
                walk_expr(
                    body,
                    *condition,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                if let Some(expr) = message {
                    walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::Assert {
                condition, message, ..
            } => {
                walk_expr(
                    body,
                    *condition,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                if let Some(message) = message {
                    walk_expr(
                        body,
                        *message,
                        owner,
                        scope_result,
                        resolver,
                        item_trees,
                        out,
                    );
                }
            }
            hir::Stmt::Assert {
                condition, message, ..
            } => {
                walk_expr(body, *condition, owner, scope_result, resolver, item_trees, out);
                if let Some(message) = message {
                    walk_expr(body, *message, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::Assert {
                condition,
                message,
                ..
            } => {
                walk_expr(body, *condition, owner, scope_result, resolver, item_trees, out);
                if let Some(message) = message {
                    walk_expr(body, *message, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::Throw { expr, .. } => {
                walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
            }
            hir::Stmt::Assert {
                condition, message, ..
            } => {
                walk_expr(body, *condition, owner, scope_result, resolver, item_trees, out);
                if let Some(message) = message {
                    walk_expr(body, *message, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Stmt::Break { .. } | hir::Stmt::Continue { .. } | hir::Stmt::Empty { .. } => {}
        }
    }

    fn walk_expr(
        body: &hir::Body,
        expr: hir::ExprId,
        owner: BodyOwner,
        scope_result: &ScopeBuildResult,
        resolver: &Resolver<'_>,
        item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
        out: &mut HashMap<usize, SwitchContext>,
    ) {
        match &body.exprs[expr] {
            hir::Expr::FieldAccess { receiver, .. }
            | hir::Expr::MethodReference { receiver, .. }
            | hir::Expr::ConstructorReference { receiver, .. } => {
                walk_expr(
                    body,
                    *receiver,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
            }
            hir::Expr::ArrayAccess { array, index, .. } => {
                walk_expr(body, *array, owner, scope_result, resolver, item_trees, out);
                walk_expr(body, *index, owner, scope_result, resolver, item_trees, out);
            }
            hir::Expr::ClassLiteral { ty, .. } => {
                walk_expr(body, *ty, owner, scope_result, resolver, item_trees, out);
            }
            hir::Expr::Call { callee, args, .. } => {
                walk_expr(
                    body,
                    *callee,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                for arg in args {
                    walk_expr(body, *arg, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Expr::New { args, .. } => {
                for arg in args {
                    walk_expr(body, *arg, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Expr::ArrayCreation { dim_exprs, .. } => {
                for dim in dim_exprs {
                    walk_expr(body, *dim, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Expr::Unary { expr, .. }
            | hir::Expr::Instanceof { expr, .. }
            | hir::Expr::Cast { expr, .. } => {
                walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out);
            }
            hir::Expr::Binary { lhs, rhs, .. } | hir::Expr::Assign { lhs, rhs, .. } => {
                walk_expr(body, *lhs, owner, scope_result, resolver, item_trees, out);
                walk_expr(body, *rhs, owner, scope_result, resolver, item_trees, out);
            }
            hir::Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                walk_expr(
                    body,
                    *condition,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                walk_expr(
                    body,
                    *then_expr,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
                walk_expr(
                    body,
                    *else_expr,
                    owner,
                    scope_result,
                    resolver,
                    item_trees,
                    out,
                );
            }
            hir::Expr::Lambda {
                body: lambda_body, ..
            } => match lambda_body {
                hir::LambdaBody::Expr(expr) => {
                    walk_expr(body, *expr, owner, scope_result, resolver, item_trees, out)
                }
                hir::LambdaBody::Block(stmt) => {
                    walk_stmt(body, *stmt, owner, scope_result, resolver, item_trees, out)
                }
            },
            hir::Expr::Invalid { children, .. } => {
                for child in children {
                    walk_expr(body, *child, owner, scope_result, resolver, item_trees, out);
                }
            }
            hir::Expr::Name { .. }
            | hir::Expr::Literal { .. }
            | hir::Expr::Null { .. }
            | hir::Expr::This { .. }
            | hir::Expr::Super { .. }
            | hir::Expr::Missing { .. } => {}
        }
    }

    walk_stmt(
        body,
        body.root,
        owner,
        scope_result,
        resolver,
        item_trees,
        out,
    );
}

fn infer_switch_selector_enum_type(
    body: &hir::Body,
    selector: &hir::ExprId,
    scope: ScopeId,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
) -> Option<ItemId> {
    let hir::Expr::Name { name, .. } = &body.exprs[*selector] else {
        return None;
    };

    let resolved =
        resolver.resolve_name(&scope_result.scopes, scope, &Name::from(name.as_str()))?;
    let ty_text = match resolved {
        Resolution::Local(local_ref) => body.locals[local_ref.local].ty_text.clone(),
        Resolution::Parameter(param_ref) => match param_ref.owner {
            ParamOwner::Method(m) => {
                let tree = item_trees.get(&m.file)?;
                tree.method(m).params.get(param_ref.index)?.ty.clone()
            }
            ParamOwner::Constructor(c) => {
                let tree = item_trees.get(&c.file)?;
                tree.constructor(c).params.get(param_ref.index)?.ty.clone()
            }
        },
        Resolution::Field(field) => {
            let tree = item_trees.get(&field.file)?;
            tree.field(field).ty.clone()
        }
        _ => return None,
    };

    let Some(type_name) = type_name_from_ref_text(&ty_text) else {
        return None;
    };
    let resolved = resolver.resolve_qualified_type_resolution_in_scope(
        &scope_result.scopes,
        scope,
        &QualifiedName::from_dotted(&type_name),
    )?;
    match resolved {
        TypeResolution::Source(item @ ItemId::Enum(_)) => Some(item),
        _ => None,
    }
}

fn record_syntax_only_references(
    file: &FileId,
    text: &str,
    tree: &nova_hir::item_tree::ItemTree,
    scope_result: &ScopeBuildResult,
    snap: &nova_db::salsa::Snapshot,
    item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
    resolver: &Resolver<'_>,
    workspace: &WorkspaceDefMap,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    let parse = nova_syntax::parse_java(text);
    let Some(unit) = ast::CompilationUnit::cast(parse.syntax()) else {
        return;
    };

    // Map type body ranges to their class scopes so we can pick an appropriate resolution scope
    // for annotations and enum constant arguments.
    let mut type_scopes: Vec<(TextRange, nova_resolve::ScopeId)> = Vec::new();
    for (&item, &class_scope) in &scope_result.class_scopes {
        let body_span = item_body_range(tree, item);
        if body_span.start >= body_span.end {
            continue;
        }
        type_scopes.push((TextRange::new(body_span.start, body_span.end), class_scope));
    }

    let type_scope_at = |start: usize| -> Option<ScopeId> {
        let mut best: Option<(usize, ScopeId)> = None;
        for (body_range, class_scope) in &type_scopes {
            if body_range.start <= start && start < body_range.end {
                let len = body_range.len();
                if best.map(|(best_len, _)| len < best_len).unwrap_or(true) {
                    best = Some((len, *class_scope));
                }
            }
        }
        best.map(|(_, scope)| scope)
    };

    // Collect switch statement scopes from HIR (selector is in HIR, labels are not).
    let mut switch_contexts: HashMap<usize, SwitchContext> = HashMap::new();
    let mut method_ids: Vec<_> = scope_result.method_scopes.keys().copied().collect();
    method_ids.sort();
    for method in method_ids {
        let body = snap.hir_body(method);
        collect_switch_contexts(
            body.as_ref(),
            BodyOwner::Method(method),
            scope_result,
            resolver,
            item_trees,
            &mut switch_contexts,
        );
    }
    let mut ctor_ids: Vec<_> = scope_result.constructor_scopes.keys().copied().collect();
    ctor_ids.sort();
    for ctor in ctor_ids {
        let body = snap.hir_constructor_body(ctor);
        collect_switch_contexts(
            body.as_ref(),
            BodyOwner::Constructor(ctor),
            scope_result,
            resolver,
            item_trees,
            &mut switch_contexts,
        );
    }
    let mut init_ids: Vec<_> = scope_result.initializer_scopes.keys().copied().collect();
    init_ids.sort();
    for init in init_ids {
        let body = snap.hir_initializer_body(init);
        collect_switch_contexts(
            body.as_ref(),
            BodyOwner::Initializer(init),
            scope_result,
            resolver,
            item_trees,
            &mut switch_contexts,
        );
    }

    // Type references across the full syntax tree.
    //
    // We intentionally walk all `ast::Type` nodes instead of relying solely on HIR/type-ref string
    // ranges: Nova's lowering currently drops several type-bearing constructs (casts/instanceof,
    // `throws`, `catch` unions, ...), so semantic rename needs an AST-level pass to be complete.
    for node in unit.syntax().descendants() {
        let Some(ty) = ast::Type::cast(node) else {
            continue;
        };
        let Some(named) = ty.named() else {
            continue;
        };

        let segments = collect_named_type_segments(&named);
        if segments.is_empty() {
            continue;
        }

        let range = ty.syntax().text_range();
        let start = u32::from(range.start()) as usize;

        // Prefer the innermost enclosing type body scope (so nested types resolve correctly).
        let mut scope = scope_result.file_scope;
        let mut best: Option<(usize, nova_resolve::ScopeId)> = None;
        for (body_range, class_scope) in &type_scopes {
            if body_range.start <= start && start < body_range.end {
                let len = body_range.len();
                if best.map(|(best_len, _)| len < best_len).unwrap_or(true) {
                    best = Some((len, *class_scope));
                }
            }
        }
        if let Some((_, class_scope)) = best {
            scope = class_scope;
        }

        // Resolve each prefix (`Outer`, `Outer.Inner`, ...) so renames can target both outer and
        // inner identifiers within a qualified type reference.
        let mut prefix = String::new();
        for (idx, (seg, seg_range)) in segments.iter().enumerate() {
            if idx > 0 {
                prefix.push('.');
            }
            prefix.push_str(seg);

            let qn = QualifiedName::from_dotted(&prefix);
            let Some(TypeResolution::Source(item)) = resolver
                .resolve_qualified_type_resolution_in_scope(&scope_result.scopes, scope, &qn)
            else {
                continue;
            };

            record_reference(
                file,
                *seg_range,
                ResolutionKey::Type(item),
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }

    // Static/type import references.
    for import in unit.imports() {
        if import.is_wildcard() {
            continue;
        }
        let Some(name) = import.name() else {
            continue;
        };

        let segments = collect_ident_segments(name.syntax());
        if segments.is_empty() {
            continue;
        }

        if import.is_static() {
            if segments.len() < 2 {
                continue;
            }
            let (owner_segments, member) = segments.split_at(segments.len() - 1);
            let member_name = member[0].0.as_str();
            let member_range = member[0].1;

            // Record type references in the owner prefix (`import static p.Outer.Inner.MEMBER;`).
            record_type_prefix_references(
                file,
                scope_result.file_scope,
                owner_segments,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );

            let Some(TypeResolution::Source(owner)) = resolve_type_from_segments(
                resolver,
                &scope_result.scopes,
                scope_result.file_scope,
                owner_segments,
            ) else {
                continue;
            };

            if let Some(key) = resolve_static_import_member_in_type(workspace, owner, member_name) {
                record_reference(
                    file,
                    member_range,
                    key,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        } else {
            // Record type references for each resolvable prefix so `Outer.Inner` counts as a
            // reference to both `Outer` and `Inner`.
            record_type_prefix_references(
                file,
                scope_result.file_scope,
                &segments,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }

    // Type references in signatures / local variable declarations / `new` expressions. These are
    // not lowered into `hir::Expr::Name` and therefore need a syntax-level walk.
    record_lightweight_type_references(
        file,
        text,
        &type_scopes,
        scope_result,
        resolver,
        resolution_to_symbol,
        references,
        spans,
    );

    // Walk all annotation argument expressions (including nested annotations).
    let mut seen_annotations: HashSet<(usize, usize)> = HashSet::new();

    fn visit_value(
        file: &FileId,
        file_text: &str,
        tree: &nova_hir::item_tree::ItemTree,
        value: ast::AnnotationElementValue,
        scope: nova_resolve::ScopeId,
        scope_result: &ScopeBuildResult,
        resolver: &Resolver<'_>,
        workspace: &WorkspaceDefMap,
        resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
        seen: &mut HashSet<(usize, usize)>,
    ) {
        if let Some(expr) = value.expression() {
            record_expression_references(
                file,
                file_text,
                expr,
                scope,
                scope_result,
                resolver,
                workspace,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        if let Some(nested) = value.annotation() {
            visit_annotation(
                file,
                file_text,
                tree,
                nested,
                scope,
                scope_result,
                resolver,
                workspace,
                resolution_to_symbol,
                references,
                spans,
                seen,
            );
        }
        if let Some(array) = value.array_initializer() {
            for v in array.values() {
                visit_value(
                    file,
                    file_text,
                    tree,
                    v,
                    scope,
                    scope_result,
                    resolver,
                    workspace,
                    resolution_to_symbol,
                    references,
                    spans,
                    seen,
                );
            }
        }
    }

    fn visit_annotation(
        file: &FileId,
        file_text: &str,
        tree: &nova_hir::item_tree::ItemTree,
        annotation: ast::Annotation,
        scope: nova_resolve::ScopeId,
        scope_result: &ScopeBuildResult,
        resolver: &Resolver<'_>,
        workspace: &WorkspaceDefMap,
        resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
        references: &mut [Vec<Reference>],
        spans: &mut Vec<(FileId, TextRange, SymbolId)>,
        seen: &mut HashSet<(usize, usize)>,
    ) {
        let range = annotation.syntax().text_range();
        let start = u32::from(range.start()) as usize;
        let end = u32::from(range.end()) as usize;
        if !seen.insert((start, end)) {
            return;
        }

        let Some(args) = annotation.arguments() else {
            return;
        };

        let anno_def = annotation
            .name()
            .and_then(|name| {
                let qn = QualifiedName::from_dotted(&name.text());
                resolver.resolve_qualified_type_resolution_in_scope(
                    &scope_result.scopes,
                    scope,
                    &qn,
                )
            })
            .and_then(|resolved| match resolved {
                TypeResolution::Source(item) => workspace.type_def(item),
                _ => None,
            })
            .filter(|def| def.kind == TypeKind::Annotation);

        if let Some(value) = args.value() {
            visit_value(
                file,
                file_text,
                tree,
                value,
                scope,
                scope_result,
                resolver,
                workspace,
                resolution_to_symbol,
                references,
                spans,
                seen,
            );
        }
        for pair in args.pairs() {
            if let (Some(anno_def), Some(name_tok)) = (anno_def, pair.name_token()) {
                let name_range = syntax_token_range(&name_tok);
                let element_name = Name::from(name_tok.text());
                if let Some(methods) = anno_def.methods.get(&element_name) {
                    if let Some(method) =
                        methods.iter().find(|m| tree.method(m.id).params.is_empty())
                    {
                        record_reference(
                            file,
                            name_range,
                            ResolutionKey::Method(method.id),
                            resolution_to_symbol,
                            references,
                            spans,
                        );
                    }
                }
            }

            let Some(value) = pair.value() else {
                continue;
            };
            visit_value(
                file,
                file_text,
                tree,
                value,
                scope,
                scope_result,
                resolver,
                workspace,
                resolution_to_symbol,
                references,
                spans,
                seen,
            );
        }
    }

    for node in unit.syntax().descendants() {
        let Some(annotation) = ast::Annotation::cast(node) else {
            continue;
        };

        let anno_range = annotation.syntax().text_range();
        let start = u32::from(anno_range.start()) as usize;

        // Package-level annotations use file/import scope.
        let mut scope = scope_result.file_scope;
        if annotation
            .syntax()
            .ancestors()
            .any(|n| n.kind() == nova_syntax::SyntaxKind::PackageDeclaration)
        {
            scope = scope_result.file_scope;
        } else {
            // Member/type-use annotations: use the innermost enclosing type body scope if present.
            let mut best: Option<(usize, nova_resolve::ScopeId)> = None;
            for (body_range, class_scope) in &type_scopes {
                if body_range.start <= start && start < body_range.end {
                    let len = body_range.len();
                    if best.map(|(best_len, _)| len < best_len).unwrap_or(true) {
                        best = Some((len, *class_scope));
                    }
                }
            }
            if let Some((_, class_scope)) = best {
                scope = class_scope;
            }
        }

        // Record the annotation name itself as a type reference (`@Foo`, `@p.Foo`).
        if let Some(name) = annotation.name() {
            let segments = collect_ident_segments(name.syntax());
            record_type_prefix_references(
                file,
                scope,
                &segments,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }

        visit_annotation(
            file,
            text,
            tree,
            annotation,
            scope,
            scope_result,
            resolver,
            workspace,
            resolution_to_symbol,
            references,
            spans,
            &mut seen_annotations,
        );
    }

    // Annotation element default values (`@interface A { Foo v() default Foo.BAR; }`).
    //
    // These default expressions are not part of any stable `hir::Body` and therefore need
    // a syntax-only reference pass so renames update them.
    for node in unit.syntax().descendants() {
        let Some(method) = ast::MethodDeclaration::cast(node) else {
            continue;
        };
        let Some(default_value) = method.default_value() else {
            continue;
        };
        let Some(value) = default_value.value() else {
            continue;
        };

        let range = method.syntax().text_range();
        let start = u32::from(range.start()) as usize;

        // Resolve in the innermost enclosing type body scope (so nested types resolve correctly).
        let mut scope = scope_result.file_scope;
        let mut best: Option<(usize, nova_resolve::ScopeId)> = None;
        for (body_range, class_scope) in &type_scopes {
            if body_range.start <= start && start < body_range.end {
                let len = body_range.len();
                if best.map(|(best_len, _)| len < best_len).unwrap_or(true) {
                    best = Some((len, *class_scope));
                }
            }
        }
        if let Some((_, class_scope)) = best {
            scope = class_scope;
        }

        visit_value(
            file,
            text,
            tree,
            value,
            scope,
            scope_result,
            resolver,
            workspace,
            resolution_to_symbol,
            references,
            spans,
            &mut seen_annotations,
        );
    }

    // Enum constant argument expressions.
    for node in unit.syntax().descendants() {
        let Some(constant) = ast::EnumConstant::cast(node) else {
            continue;
        };

        let Some(args) = constant.arguments() else {
            continue;
        };

        let range = constant.syntax().text_range();
        let start = u32::from(range.start()) as usize;

        let mut scope = scope_result.file_scope;
        let mut best: Option<(usize, nova_resolve::ScopeId)> = None;
        for (body_range, class_scope) in &type_scopes {
            if body_range.start <= start && start < body_range.end {
                let len = body_range.len();
                if best.map(|(best_len, _)| len < best_len).unwrap_or(true) {
                    best = Some((len, *class_scope));
                }
            }
        }
        if let Some((_, class_scope)) = best {
            scope = class_scope;
        }

        for expr in args.arguments() {
            record_expression_references(
                file,
                text,
                expr,
                scope,
                scope_result,
                resolver,
                workspace,
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }

    // Field initializer expressions (`int x = ...;`) are not lowered into stable HIR.
    for node in unit.syntax().descendants() {
        let Some(field_decl) = ast::FieldDeclaration::cast(node) else {
            continue;
        };
        let start = u32::from(field_decl.syntax().text_range().start()) as usize;
        let Some(scope) = type_scope_at(start) else {
            continue;
        };

        let Some(list) = field_decl.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            let Some(init) = decl.initializer() else {
                continue;
            };
            record_expression_references(
                file,
                text,
                init,
                scope,
                scope_result,
                resolver,
                workspace,
                resolution_to_symbol,
                references,
                spans,
            );
        }
    }

    // Switch label expressions (`case FOO:`) are not lowered into stable HIR.
    for node in unit.syntax().descendants() {
        let Some(switch_stmt) = ast::SwitchStatement::cast(node) else {
            continue;
        };

        let start = u32::from(switch_stmt.syntax().text_range().start()) as usize;
        let (scope, selector_enum) = switch_contexts
            .get(&start)
            .map(|ctx| (ctx.scope, ctx.selector_enum))
            .unwrap_or_else(|| {
                (
                    type_scope_at(start).unwrap_or(scope_result.file_scope),
                    None,
                )
            });

        for label in switch_stmt.labels() {
            for expr in label.expressions() {
                record_expression_references(
                    file,
                    text,
                    expr.clone(),
                    scope,
                    scope_result,
                    resolver,
                    workspace,
                    resolution_to_symbol,
                    references,
                    spans,
                );

                // `case FOO:` inside `switch(enum)` implicitly refers to the enum constant.
                let Some(enum_item) = selector_enum else {
                    continue;
                };
                let ast::Expression::NameExpression(name_expr) = expr else {
                    continue;
                };
                let segments = collect_ident_segments(name_expr.syntax());
                if segments.len() != 1 {
                    continue;
                }
                let (name, range) = segments[0].clone();

                // If it resolves in the normal scope, it's not an implicit enum constant label.
                if resolver
                    .resolve_name(&scope_result.scopes, scope, &Name::from(name.as_str()))
                    .is_some()
                {
                    continue;
                }

                let Some(ty) = workspace.type_def(enum_item) else {
                    continue;
                };
                let Some(field) = ty.fields.get(&Name::from(name.as_str())) else {
                    continue;
                };
                let field_id = field.id;
                let Some(tree) = item_trees.get(&field_id.file) else {
                    continue;
                };
                if tree.field(field_id).kind != FieldKind::EnumConstant {
                    continue;
                }
                record_reference(
                    file,
                    range,
                    ResolutionKey::Field(field_id),
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
    }

    // Best-effort: switch expressions are not modeled in stable HIR, so we cannot recover the
    // precise resolution scope. Fall back to the innermost enclosing type scope (or file scope).
    for node in unit.syntax().descendants() {
        let Some(switch_expr) = ast::SwitchExpression::cast(node) else {
            continue;
        };
        let start = u32::from(switch_expr.syntax().text_range().start()) as usize;
        let scope = type_scope_at(start).unwrap_or(scope_result.file_scope);
        for label in switch_expr.labels() {
            for expr in label.expressions() {
                record_expression_references(
                    file,
                    text,
                    expr,
                    scope,
                    scope_result,
                    resolver,
                    workspace,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
    }
}

fn type_name_from_ref_text(text: &str) -> Option<String> {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
            out.push(ch);
        } else {
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn record_lightweight_type_references(
    file: &FileId,
    text: &str,
    type_scopes: &[(TextRange, nova_resolve::ScopeId)],
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    let parsed = java_syntax::parse(text);
    let unit = parsed.compilation_unit();

    for ty in &unit.types {
        record_lightweight_type_decl(
            file,
            text,
            ty,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

fn record_lightweight_type_decl(
    file: &FileId,
    text: &str,
    ty: &java_syntax::ast::TypeDecl,
    type_scopes: &[(TextRange, nova_resolve::ScopeId)],
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    use java_syntax::ast::{MemberDecl, TypeDecl};

    let members = match ty {
        TypeDecl::Class(decl) => &decl.members,
        TypeDecl::Interface(decl) => &decl.members,
        TypeDecl::Enum(decl) => &decl.members,
        TypeDecl::Record(decl) => &decl.members,
        TypeDecl::Annotation(decl) => &decl.members,
    };

    for member in members {
        match member {
            MemberDecl::Field(field) => {
                record_type_names_in_range(
                    file,
                    text,
                    TextRange::new(field.ty.range.start, field.ty.range.end),
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            MemberDecl::Method(method) => {
                record_type_names_in_range(
                    file,
                    text,
                    TextRange::new(method.return_ty.range.start, method.return_ty.range.end),
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                for param in &method.params {
                    record_type_names_in_range(
                        file,
                        text,
                        TextRange::new(param.ty.range.start, param.ty.range.end),
                        type_scopes,
                        scope_result,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
                if let Some(body) = &method.body {
                    record_lightweight_block(
                        file,
                        text,
                        body,
                        type_scopes,
                        scope_result,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
            }
            MemberDecl::Constructor(ctor) => {
                for param in &ctor.params {
                    record_type_names_in_range(
                        file,
                        text,
                        TextRange::new(param.ty.range.start, param.ty.range.end),
                        type_scopes,
                        scope_result,
                        resolver,
                        resolution_to_symbol,
                        references,
                        spans,
                    );
                }
                record_lightweight_block(
                    file,
                    text,
                    &ctor.body,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            MemberDecl::Initializer(init) => record_lightweight_block(
                file,
                text,
                &init.body,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            ),
            MemberDecl::Type(child) => record_lightweight_type_decl(
                file,
                text,
                child,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            ),
        }
    }
}

fn record_lightweight_block(
    file: &FileId,
    text: &str,
    block: &java_syntax::ast::Block,
    type_scopes: &[(TextRange, nova_resolve::ScopeId)],
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    for stmt in &block.statements {
        record_lightweight_stmt(
            file,
            text,
            stmt,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

fn record_lightweight_stmt(
    file: &FileId,
    text: &str,
    stmt: &java_syntax::ast::Stmt,
    type_scopes: &[(TextRange, nova_resolve::ScopeId)],
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    use java_syntax::ast::Stmt;

    match stmt {
        Stmt::LocalVar(local) => {
            record_type_names_in_range(
                file,
                text,
                TextRange::new(local.ty.range.start, local.ty.range.end),
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            if let Some(init) = &local.initializer {
                record_lightweight_expr(
                    file,
                    text,
                    init,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Stmt::Assert(assert) => {
            record_lightweight_expr(
                file,
                text,
                &assert.condition,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            if let Some(message) = &assert.message {
                record_lightweight_expr(
                    file,
                    text,
                    message,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Stmt::Expr(expr) => record_lightweight_expr(
            file,
            text,
            &expr.expr,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        ),
        Stmt::Return(ret) => {
            if let Some(expr) = &ret.expr {
                record_lightweight_expr(
                    file,
                    text,
                    expr,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Stmt::Block(block) => record_lightweight_block(
            file,
            text,
            block,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        ),
        Stmt::If(stmt) => {
            record_lightweight_expr(
                file,
                text,
                &stmt.condition,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_stmt(
                file,
                text,
                &stmt.then_branch,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            if let Some(else_branch) = &stmt.else_branch {
                record_lightweight_stmt(
                    file,
                    text,
                    else_branch,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Stmt::While(stmt) => {
            record_lightweight_expr(
                file,
                text,
                &stmt.condition,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_stmt(
                file,
                text,
                &stmt.body,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Stmt::For(stmt) => {
            for init in &stmt.init {
                record_lightweight_stmt(
                    file,
                    text,
                    init,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            if let Some(cond) = &stmt.condition {
                record_lightweight_expr(
                    file,
                    text,
                    cond,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            for update in &stmt.update {
                record_lightweight_expr(
                    file,
                    text,
                    update,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            record_lightweight_stmt(
                file,
                text,
                &stmt.body,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Stmt::ForEach(stmt) => {
            let var = &stmt.var;
            record_type_names_in_range(
                file,
                text,
                TextRange::new(var.ty.range.start, var.ty.range.end),
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            if let Some(init) = &var.initializer {
                record_lightweight_expr(
                    file,
                    text,
                    init,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            record_lightweight_expr(
                file,
                text,
                &stmt.iterable,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_stmt(
                file,
                text,
                &stmt.body,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Stmt::Synchronized(stmt) => {
            record_lightweight_expr(
                file,
                text,
                &stmt.expr,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_block(
                file,
                text,
                &stmt.body,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Stmt::Switch(stmt) => {
            record_lightweight_expr(
                file,
                text,
                &stmt.selector,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_block(
                file,
                text,
                &stmt.body,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Stmt::Try(stmt) => {
            record_lightweight_block(
                file,
                text,
                &stmt.body,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            for catch in &stmt.catches {
                let param = &catch.param;
                record_type_names_in_range(
                    file,
                    text,
                    TextRange::new(param.ty.range.start, param.ty.range.end),
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
                record_lightweight_block(
                    file,
                    text,
                    &catch.body,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
            if let Some(finally) = &stmt.finally {
                record_lightweight_block(
                    file,
                    text,
                    finally,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Stmt::Throw(stmt) => record_lightweight_expr(
            file,
            text,
            &stmt.expr,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        ),
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty(_) => {}
    }
}

fn record_lightweight_expr(
    file: &FileId,
    text: &str,
    expr: &java_syntax::ast::Expr,
    type_scopes: &[(TextRange, nova_resolve::ScopeId)],
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    use java_syntax::ast::Expr;

    match expr {
        Expr::New(new_expr) => {
            record_type_names_in_range(
                file,
                text,
                TextRange::new(new_expr.class.range.start, new_expr.class.range.end),
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            for arg in &new_expr.args {
                record_lightweight_expr(
                    file,
                    text,
                    arg,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Expr::ArrayCreation(expr) => {
            record_type_names_in_range(
                file,
                text,
                TextRange::new(expr.elem_ty.range.start, expr.elem_ty.range.end),
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            for dim in &expr.dim_exprs {
                record_lightweight_expr(
                    file,
                    text,
                    dim,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Expr::Cast(expr) => {
            record_type_names_in_range(
                file,
                text,
                TextRange::new(expr.ty.range.start, expr.ty.range.end),
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_expr(
                file,
                text,
                &expr.expr,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Expr::Call(call) => {
            record_lightweight_expr(
                file,
                text,
                &call.callee,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            for arg in &call.args {
                record_lightweight_expr(
                    file,
                    text,
                    arg,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Expr::FieldAccess(access) => record_lightweight_expr(
            file,
            text,
            &access.receiver,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        ),
        Expr::ArrayAccess(access) => {
            record_lightweight_expr(
                file,
                text,
                &access.array,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_expr(
                file,
                text,
                &access.index,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Expr::Unary(expr) => record_lightweight_expr(
            file,
            text,
            &expr.expr,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        ),
        Expr::Binary(expr) => {
            record_lightweight_expr(
                file,
                text,
                &expr.lhs,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_expr(
                file,
                text,
                &expr.rhs,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Expr::Instanceof(expr) => {
            record_lightweight_expr(
                file,
                text,
                &expr.expr,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_type_names_in_range(
                file,
                text,
                TextRange::new(expr.ty.range.start, expr.ty.range.end),
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Expr::Assign(expr) => {
            record_lightweight_expr(
                file,
                text,
                &expr.lhs,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_expr(
                file,
                text,
                &expr.rhs,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Expr::Conditional(expr) => {
            record_lightweight_expr(
                file,
                text,
                &expr.condition,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_expr(
                file,
                text,
                &expr.then_expr,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_expr(
                file,
                text,
                &expr.else_expr,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Expr::Lambda(expr) => match &expr.body {
            java_syntax::ast::LambdaBody::Expr(expr) => record_lightweight_expr(
                file,
                text,
                expr,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            ),
            java_syntax::ast::LambdaBody::Block(block) => record_lightweight_block(
                file,
                text,
                block,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            ),
        },
        Expr::Cast(expr) => {
            record_type_names_in_range(
                file,
                text,
                TextRange::new(expr.ty.range.start, expr.ty.range.end),
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_expr(
                file,
                text,
                &expr.expr,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Expr::MethodReference(expr) => record_lightweight_expr(
            file,
            text,
            &expr.receiver,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        ),
        Expr::ConstructorReference(expr) => record_lightweight_expr(
            file,
            text,
            &expr.receiver,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        ),
        Expr::Cast(expr) => {
            record_type_names_in_range(
                file,
                text,
                TextRange::new(expr.ty.range.start, expr.ty.range.end),
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
            record_lightweight_expr(
                file,
                text,
                &expr.expr,
                type_scopes,
                scope_result,
                resolver,
                resolution_to_symbol,
                references,
                spans,
            );
        }
        Expr::ClassLiteral(expr) => record_lightweight_expr(
            file,
            text,
            &expr.ty,
            type_scopes,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        ),
        Expr::Invalid { children, .. } => {
            for child in children {
                record_lightweight_expr(
                    file,
                    text,
                    child,
                    type_scopes,
                    scope_result,
                    resolver,
                    resolution_to_symbol,
                    references,
                    spans,
                );
            }
        }
        Expr::Name(_)
        | Expr::IntLiteral(_)
        | Expr::LongLiteral(_)
        | Expr::FloatLiteral(_)
        | Expr::DoubleLiteral(_)
        | Expr::CharLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::TextBlock(_)
        | Expr::BoolLiteral(_)
        | Expr::NullLiteral(_)
        | Expr::This(_)
        | Expr::Super(_)
        | Expr::Missing(_) => {}
    }
}
fn record_type_names_in_range(
    file: &FileId,
    text: &str,
    range: TextRange,
    type_scopes: &[(TextRange, nova_resolve::ScopeId)],
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    resolution_to_symbol: &HashMap<ResolutionKey, SymbolId>,
    references: &mut [Vec<Reference>],
    spans: &mut Vec<(FileId, TextRange, SymbolId)>,
) {
    let scope = scope_for_offset_in_type_scopes(type_scopes, scope_result.file_scope, range.start);
    for segments in scan_qualified_name_occurrences(text, range) {
        record_type_prefix_references(
            file,
            scope,
            &segments,
            scope_result,
            resolver,
            resolution_to_symbol,
            references,
            spans,
        );
    }
}

fn scope_for_offset_in_type_scopes(
    type_scopes: &[(TextRange, nova_resolve::ScopeId)],
    default_scope: nova_resolve::ScopeId,
    offset: usize,
) -> nova_resolve::ScopeId {
    let mut best: Option<(usize, nova_resolve::ScopeId)> = None;
    for (body_range, class_scope) in type_scopes {
        if body_range.start <= offset && offset < body_range.end {
            let len = body_range.len();
            if best.map(|(best_len, _)| len < best_len).unwrap_or(true) {
                best = Some((len, *class_scope));
            }
        }
    }
    best.map(|(_, scope)| scope).unwrap_or(default_scope)
}

fn scan_qualified_name_occurrences(text: &str, range: TextRange) -> Vec<Vec<(String, TextRange)>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();

    let mut i = range.start.min(bytes.len());
    let end = range.end.min(bytes.len());

    while i < end {
        i = skip_ws_and_comments(text, i, end);
        if i >= end {
            break;
        }

        // Skip literals defensively.
        if bytes[i] == b'"' {
            i = skip_string_literal(text, i, end);
            continue;
        }
        if bytes[i] == b'\'' {
            i = skip_char_literal(text, i, end);
            continue;
        }

        if !is_ident_start(bytes[i]) {
            i += 1;
            continue;
        }

        let (first, first_range, next) = match read_ident(text, i, end) {
            Some(v) => v,
            None => {
                i += 1;
                continue;
            }
        };
        let mut segments = vec![(first.to_string(), first_range)];
        i = next;

        loop {
            let after = skip_ws_and_comments(text, i, end);
            if after >= end || bytes[after] != b'.' {
                break;
            }
            let mut j = after + 1;
            j = skip_ws_and_comments(text, j, end);
            if j >= end || !is_ident_start(bytes[j]) {
                break;
            }
            let (seg, seg_range, next2) = match read_ident(text, j, end) {
                Some(v) => v,
                None => break,
            };
            segments.push((seg.to_string(), seg_range));
            i = next2;
        }

        out.push(segments);
    }

    out
}

fn is_ident_start(b: u8) -> bool {
    (b as char).is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || (b as char).is_ascii_digit()
}

fn skip_ws_and_comments(text: &str, mut i: usize, end: usize) -> usize {
    let bytes = text.as_bytes();
    while i < end {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if i + 1 < end && bytes[i + 1] == b'/' => {
                i += 2;
                while i < end && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < end && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < end {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => break,
        }
    }
    i
}

fn read_ident<'a>(text: &'a str, i: usize, end: usize) -> Option<(&'a str, TextRange, usize)> {
    let bytes = text.as_bytes();
    if i >= end || !is_ident_start(bytes[i]) {
        return None;
    }
    let start = i;
    let mut j = i + 1;
    while j < end && is_ident_continue(bytes[j]) {
        j += 1;
    }
    Some((&text[start..j], TextRange::new(start, j), j))
}

fn skip_string_literal(text: &str, mut i: usize, end: usize) -> usize {
    let bytes = text.as_bytes();
    if i >= end || bytes[i] != b'"' {
        return i;
    }
    i += 1;
    while i < end {
        let b = bytes[i];
        if b == b'\\' {
            i = (i + 2).min(end);
            continue;
        }
        i += 1;
        if b == b'"' {
            break;
        }
    }
    i
}

fn skip_char_literal(text: &str, mut i: usize, end: usize) -> usize {
    let bytes = text.as_bytes();
    if i >= end || bytes[i] != b'\'' {
        return i;
    }
    i += 1;
    while i < end {
        let b = bytes[i];
        if b == b'\\' {
            i = (i + 2).min(end);
            continue;
        }
        i += 1;
        if b == b'\'' {
            break;
        }
    }
    i
}
