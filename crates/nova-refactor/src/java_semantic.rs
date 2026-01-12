use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use nova_core::{Name, QualifiedName};
use nova_db::salsa::{Database as SalsaDatabase, NovaHir};
use nova_db::{FileId as DbFileId, ProjectId};
use nova_hir::hir;
use nova_hir::ids::{FieldId, ItemId, MethodId};
use nova_hir::item_tree::{Item, Member};
use nova_hir::queries::HirDatabase;
use nova_resolve::{
    BodyOwner, DefMap, LocalRef, ParamOwner, ParamRef, Resolution, Resolver, ScopeBuildResult,
    ScopeKind, StaticMemberResolution, TypeResolution, WorkspaceDefMap,
};

use crate::edit::{FileId, TextRange};
use crate::semantic::{RefactorDatabase, Reference, SymbolDefinition};

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
    Type,
    Method,
    Field,
    Local,
    Parameter,
}

#[derive(Clone, Debug)]
struct SymbolData {
    def: SymbolDefinition,
    kind: JavaSymbolKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ResolutionKey {
    Local(LocalRef),
    Param(ParamRef),
    Field(FieldId),
    Method(MethodId),
    Type(ItemId),
}

#[derive(Debug, Clone)]
struct SymbolCandidate {
    key: ResolutionKey,
    file: FileId,
    name: String,
    name_range: TextRange,
    scope: u32,
    kind: JavaSymbolKind,
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

    scopes: HashMap<DbFileId, ScopeBuildResult>,
    scope_interner: ScopeInterner,

    symbols: Vec<SymbolData>,
    references: Vec<Vec<Reference>>,
    spans: Vec<(FileId, TextRange, SymbolId)>,

    resolution_to_symbol: HashMap<ResolutionKey, SymbolId>,
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

            salsa.set_file_text(id, text.to_string());
            salsa.set_file_rel_path(id, Arc::new(file.0.clone()));
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

        let mut scope_interner = ScopeInterner::default();
        let mut scopes: HashMap<DbFileId, ScopeBuildResult> = HashMap::new();
        let mut candidates: Vec<SymbolCandidate> = Vec::new();
        let mut method_groups: Vec<MethodGroupInfo> = Vec::new();
        let mut type_constructor_refs: HashMap<ItemId, Vec<(FileId, TextRange)>> = HashMap::new();

        // Build per-file scope graphs + symbol definitions.
        for (file, file_id) in &file_ids {
            let scope_result = nova_resolve::build_scopes(&hir, *file_id);
            let tree = item_trees
                .get(file_id)
                .cloned()
                .unwrap_or_else(|| snap.hir_item_tree(*file_id));

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
                    &mut scope_interner,
                    &mut candidates,
                    &mut method_groups,
                    &mut type_constructor_refs,
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
                    });
                }
            }

            scopes.insert(*file_id, scope_result);
        }

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
        let mut resolution_to_symbol: HashMap<ResolutionKey, SymbolId> = HashMap::new();

        for (idx, candidate) in candidates.into_iter().enumerate() {
            let symbol = SymbolId::new(idx as u32);
            symbols.push(SymbolData {
                def: SymbolDefinition {
                    file: candidate.file.clone(),
                    name: candidate.name.clone(),
                    name_range: candidate.name_range,
                    scope: candidate.scope,
                },
                kind: candidate.kind,
            });
            references.push(Vec::new());
            spans.push((candidate.file, candidate.name_range, symbol));
            resolution_to_symbol.insert(candidate.key, symbol);
        }

        // Populate method-group mappings for overloads and attach additional declaration spans.
        for group in &method_groups {
            let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Method(group.representative))
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
        let jdk = nova_jdk::JdkIndex::new();
        let resolver = Resolver::new(&jdk)
            .with_classpath(&workspace_def_map)
            .with_workspace(&workspace_def_map);

        for (file, file_id) in &file_ids {
            let Some(scope_result) = scopes.get(file_id) else {
                continue;
            };

            let tree = item_trees
                .get(file_id)
                .cloned()
                .unwrap_or_else(|| snap.hir_item_tree(*file_id));

            let mut method_ids: Vec<_> = scope_result.method_scopes.keys().copied().collect();
            method_ids.sort();
            for method in method_ids {
                let body = snap.hir_body(method);
                record_body_references(
                    file,
                    BodyOwner::Method(method),
                    &body,
                    scope_result,
                    &resolver,
                    &workspace_def_map,
                    &item_trees,
                    tree.as_ref(),
                    &resolution_to_symbol,
                    &mut references,
                    &mut spans,
                );
            }

            let mut ctor_ids: Vec<_> = scope_result.constructor_scopes.keys().copied().collect();
            ctor_ids.sort();
            for ctor in ctor_ids {
                let body = snap.hir_constructor_body(ctor);
                record_body_references(
                    file,
                    BodyOwner::Constructor(ctor),
                    &body,
                    scope_result,
                    &resolver,
                    &workspace_def_map,
                    &item_trees,
                    tree.as_ref(),
                    &resolution_to_symbol,
                    &mut references,
                    &mut spans,
                );
            }

            let mut init_ids: Vec<_> = scope_result.initializer_scopes.keys().copied().collect();
            init_ids.sort();
            for init in init_ids {
                let body = snap.hir_initializer_body(init);
                record_body_references(
                    file,
                    BodyOwner::Initializer(init),
                    &body,
                    scope_result,
                    &resolver,
                    &workspace_def_map,
                    &item_trees,
                    tree.as_ref(),
                    &resolution_to_symbol,
                    &mut references,
                    &mut spans,
                );
            }
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
            scopes,
            scope_interner,
            symbols,
            references,
            spans,
            resolution_to_symbol,
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

fn refactor_local_scope(
    scope_result: &ScopeBuildResult,
    body: &hir::Body,
    local_scope: nova_resolve::ScopeId,
) -> nova_resolve::ScopeId {
    let (owner, let_stmt_id) = match scope_result.scopes.scope(local_scope).kind() {
        ScopeKind::Block { owner, stmt }
            if matches!(&body.stmts[*stmt], hir::Stmt::Let { .. }) =>
        {
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
    scope_interner: &mut ScopeInterner,
    candidates: &mut Vec<SymbolCandidate>,
    method_groups: &mut Vec<MethodGroupInfo>,
    type_constructor_refs: &mut HashMap<ItemId, Vec<(FileId, TextRange)>>,
) {
    // Type declaration.
    let (name, name_range) = item_name_and_range(tree, item);
    let scope = scope_interner.intern(db_file, decl_scope);
    candidates.push(SymbolCandidate {
        key: ResolutionKey::Type(item),
        file: file.clone(),
        name,
        name_range,
        scope,
        kind: JavaSymbolKind::Type,
    });

    let Some(&class_scope) = scope_result.class_scopes.get(&item) else {
        return;
    };
    let class_scope_interned = scope_interner.intern(db_file, class_scope);

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
                });
            }
            Member::Method(method_id) => {
                let method = tree.method(*method_id);
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
                type_constructor_refs
                    .entry(item)
                    .or_default()
                    .push((
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
                    scope_interner,
                    candidates,
                    method_groups,
                    type_constructor_refs,
                );
            }
            Member::Initializer(_) => {}
        }
    }

    // Method groups (overloads) – one symbol per (containing type, method name).
    for (name, mut methods) in methods_by_name {
        methods.sort_by(|a, b| a.1.start.cmp(&b.1.start).then_with(|| a.1.end.cmp(&b.1.end)));

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
        });

        method_groups.push(MethodGroupInfo {
            file: file.clone(),
            representative,
            method_ids: methods.iter().map(|(id, _)| *id).collect(),
            decl_ranges: methods.iter().map(|(_, range)| *range).collect(),
        });
    }
}

impl RefactorDatabase for RefactorJavaDatabase {
    fn file_text(&self, file: &FileId) -> Option<&str> {
        self.files.get(file).map(|text| text.as_ref())
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
                Resolution::Methods(methods) => ResolutionKey::Method(*methods.first()?),
                _ => return None,
            };
            return self.resolution_to_symbol.get(&key).copied();
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

    fn would_shadow(&self, scope: u32, name: &str) -> Option<SymbolId> {
        let (file, local_scope) = self.decode_scope(scope)?;
        let scope_result = self.scopes.get(&file)?;

        let name = Name::from(name);

        let mut current = scope_result.scopes.scope(local_scope).parent();
        while let Some(scope_id) = current {
            let data = scope_result.scopes.scope(scope_id);

            if let Some(resolution) = data.values().get(&name) {
                let key = match resolution {
                    Resolution::Local(local) => ResolutionKey::Local(*local),
                    Resolution::Parameter(param) => ResolutionKey::Param(*param),
                    Resolution::Field(field) => ResolutionKey::Field(*field),
                    Resolution::Methods(methods) => ResolutionKey::Method(*methods.first()?),
                    _ => {
                        current = data.parent();
                        continue;
                    }
                };

                if let Some(symbol) = self.resolution_to_symbol.get(&key).copied() {
                    return Some(symbol);
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
}

fn record_body_references(
    file: &FileId,
    owner: BodyOwner,
    body: &hir::Body,
    scope_result: &ScopeBuildResult,
    resolver: &Resolver<'_>,
    workspace_def_map: &WorkspaceDefMap,
    item_trees: &HashMap<DbFileId, Arc<nova_hir::item_tree::ItemTree>>,
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
            hir::Expr::Name { name, .. } => {
                let resolved =
                    resolver.resolve_name(&scope_result.scopes, scope, &Name::from(name.as_str()))?;
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
                let name = Name::from(name.as_str());
                let resolved = resolver
                    .resolve_name(&scope_result.scopes, scope, &name)
                    .or_else(|| resolver.resolve_method_name(&scope_result.scopes, scope, &name));
                let Some(resolved) = resolved else {
                    return;
                };

                let key = match resolved {
                    Resolution::Local(local) => ResolutionKey::Local(local),
                    Resolution::Parameter(param) => ResolutionKey::Param(param),
                    Resolution::Field(field) => ResolutionKey::Field(field),
                    Resolution::Type(TypeResolution::Source(item)) => ResolutionKey::Type(item),
                    Resolution::StaticMember(StaticMemberResolution::SourceField(field)) => {
                        ResolutionKey::Field(field)
                    }
                    Resolution::StaticMember(StaticMemberResolution::SourceMethod(method)) => {
                        ResolutionKey::Method(method)
                    }
                    _ => return,
                };
                let Some(&symbol) = resolution_to_symbol.get(&key) else {
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
                if call_callees.contains(&expr_id) {
                    return;
                }

                let Some(receiver_ty) =
                    receiver_type(owner, body, *receiver, scope_result, resolver, item_trees, tree)
                else {
                    return;
                };
                let TypeResolution::Source(item) = receiver_ty else {
                    return;
                };
                let Some(def) = workspace_def_map.type_def(item) else {
                    return;
                };
                let Some(field) = def.fields.get(&Name::from(name.as_str())).map(|f| f.id) else {
                    return;
                };

                let Some(&symbol) =
                    resolution_to_symbol.get(&ResolutionKey::Field(field))
                else {
                    return;
                };
                let range = TextRange::new(name_range.start, name_range.end);
                record(file, symbol, range, references, spans);
            }
            hir::Expr::Call { callee, .. } => match &body.exprs[*callee] {
                hir::Expr::Name { name, range } => {
                    let Some(&callee_scope) = scope_result.expr_scopes.get(&(owner, *callee))
                    else {
                        return;
                    };
                    let Some(resolved) = resolver.resolve_method_name(
                        &scope_result.scopes,
                        callee_scope,
                        &Name::from(name.as_str()),
                    ) else {
                        return;
                    };

                    let method = match resolved {
                        Resolution::Methods(methods) => methods.first().copied(),
                        Resolution::StaticMember(StaticMemberResolution::SourceMethod(m)) => Some(m),
                        _ => None,
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

                    let Some(methods) = def.methods.get(&Name::from(name.as_str())) else {
                        return;
                    };
                    let Some(method) = methods.first().map(|m| m.id) else {
                        return;
                    };
                    let Some(&symbol) =
                        resolution_to_symbol.get(&ResolutionKey::Method(method))
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
                let Some(receiver_ty) =
                    receiver_type(owner, body, *receiver, scope_result, resolver, item_trees, tree)
                else {
                    return;
                };
                let TypeResolution::Source(item) = receiver_ty else {
                    return;
                };
                let Some(def) = workspace_def_map.type_def(item) else {
                    return;
                };
                let Some(methods) = def.methods.get(&Name::from(name.as_str())) else {
                    return;
                };
                let Some(method) = methods.first().map(|m| m.id) else {
                    return;
                };
                let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Method(method)) else {
                    return;
                };
                let range = TextRange::new(name_range.start, name_range.end);
                record(file, symbol, range, references, spans);
            }
            hir::Expr::New {
                class, class_range, ..
            } => {
                let scope = type_resolution_scope(&scope_result.scopes, scope);
                let Some(resolved) = resolve_type_text(&scope_result.scopes, scope, resolver, class)
                else {
                    return;
                };
                let TypeResolution::Source(item) = resolved else {
                    return;
                };
                let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Type(item)) else {
                    return;
                };
                let range = TextRange::new(class_range.start, class_range.end);
                record(file, symbol, range, references, spans);
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
            hir::Stmt::For { init, body: inner, .. } => {
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
        let Some(resolved) =
            resolve_type_text(&scope_result.scopes, type_scope, resolver, &local.ty_text)
        else {
            return;
        };
        let TypeResolution::Source(item) = resolved else {
            return;
        };
        let Some(&symbol) = resolution_to_symbol.get(&ResolutionKey::Type(item)) else {
            return;
        };
        let range = TextRange::new(local.ty_range.start, local.ty_range.end);
        record(file, symbol, range, references, spans);
    });
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
            hir::Expr::Unary { expr, .. } => walk_expr(body, *expr, f),
            hir::Expr::Binary { lhs, rhs, .. } | hir::Expr::Assign { lhs, rhs, .. } => {
                walk_expr(body, *lhs, f);
                walk_expr(body, *rhs, f);
            }
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
        }
    }

    walk_stmt(body, body.root, &mut f);
}
