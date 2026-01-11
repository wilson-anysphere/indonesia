use std::collections::HashMap;

use nova_core::FileId;
use nova_core::{Name, PackageName, TypeName};
use nova_hir::hir;
use nova_hir::ids::{ConstructorId, InitializerId, ItemId, MethodId};
use nova_hir::item_tree::{self, ItemTree, Member};
use nova_hir::queries::{self, HirDatabase};

use crate::import_map::ImportMap;
use crate::resolver::{BodyOwner, LocalRef, ParamOwner, ParamRef, Resolution, TypeResolution};

pub type ScopeId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeGraph {
    scopes: Vec<ScopeData>,
    type_names: HashMap<ItemId, TypeName>,
    items_by_type_name: HashMap<TypeName, ItemId>,
}

impl ScopeGraph {
    #[must_use]
    pub fn scope(&self, id: ScopeId) -> &ScopeData {
        &self.scopes[id]
    }

    #[must_use]
    pub fn type_name(&self, item: ItemId) -> Option<&TypeName> {
        self.type_names.get(&item)
    }

    #[must_use]
    pub fn item_by_type_name(&self, name: &TypeName) -> Option<ItemId> {
        self.items_by_type_name.get(name).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeData {
    pub(crate) parent: Option<ScopeId>,
    pub(crate) kind: ScopeKind,
    pub(crate) values: HashMap<Name, Resolution>,
    pub(crate) types: HashMap<Name, TypeResolution>,
}

impl ScopeData {
    #[must_use]
    pub fn values(&self) -> &HashMap<Name, Resolution> {
        &self.values
    }

    #[must_use]
    pub fn types(&self) -> &HashMap<Name, TypeResolution> {
        &self.types
    }

    #[must_use]
    pub fn parent(&self) -> Option<ScopeId> {
        self.parent
    }

    #[must_use]
    pub fn kind(&self) -> &ScopeKind {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeKind {
    Universe,
    Package {
        package: Option<PackageName>,
    },
    Import {
        imports: ImportMap,
        package: Option<PackageName>,
    },
    File {
        file: FileId,
    },
    Class {
        item: ItemId,
    },
    Method {
        method: MethodId,
    },
    Constructor {
        constructor: ConstructorId,
    },
    Initializer {
        initializer: InitializerId,
    },
    Block {
        owner: BodyOwner,
        stmt: hir::StmtId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeBuildResult {
    pub scopes: ScopeGraph,
    pub file_scope: ScopeId,
    pub class_scopes: HashMap<ItemId, ScopeId>,
    pub method_scopes: HashMap<MethodId, ScopeId>,
    pub constructor_scopes: HashMap<ConstructorId, ScopeId>,
    pub initializer_scopes: HashMap<InitializerId, ScopeId>,
    pub body_scopes: HashMap<BodyOwner, ScopeId>,
    pub block_scopes: Vec<ScopeId>,
    pub stmt_scopes: HashMap<hir::StmtId, ScopeId>,
    pub expr_scopes: HashMap<hir::ExprId, ScopeId>,
}

mod item_tree_scopes;

pub use item_tree_scopes::{build_scopes_for_item_tree, ItemTreeScopeBuildResult};

/// Build a scope graph for a Java file.
///
/// The resulting `ScopeGraph` is derived solely from the file's HIR (and its
/// bodies) and is intended to be used as a query result.
pub fn build_scopes(db: &dyn HirDatabase, file: FileId) -> ScopeBuildResult {
    let tree = queries::item_tree(db, file);
    ScopeBuilder::new(file, &tree).build(db)
}

struct ScopeBuilder<'a> {
    file: FileId,
    tree: &'a ItemTree,
    scopes: Vec<ScopeData>,
    type_names: HashMap<ItemId, TypeName>,
    items_by_type_name: HashMap<TypeName, ItemId>,

    class_scopes: HashMap<ItemId, ScopeId>,
    method_scopes: HashMap<MethodId, ScopeId>,
    constructor_scopes: HashMap<ConstructorId, ScopeId>,
    initializer_scopes: HashMap<InitializerId, ScopeId>,
    body_scopes: HashMap<BodyOwner, ScopeId>,
    block_scopes: Vec<ScopeId>,
    stmt_scopes: HashMap<hir::StmtId, ScopeId>,
    expr_scopes: HashMap<hir::ExprId, ScopeId>,
}

impl<'a> ScopeBuilder<'a> {
    fn new(file: FileId, tree: &'a ItemTree) -> Self {
        Self {
            file,
            tree,
            scopes: Vec::new(),
            type_names: HashMap::new(),
            items_by_type_name: HashMap::new(),
            class_scopes: HashMap::new(),
            method_scopes: HashMap::new(),
            constructor_scopes: HashMap::new(),
            initializer_scopes: HashMap::new(),
            body_scopes: HashMap::new(),
            block_scopes: Vec::new(),
            stmt_scopes: HashMap::new(),
            expr_scopes: HashMap::new(),
        }
    }

    fn build(mut self, db: &dyn HirDatabase) -> ScopeBuildResult {
        let universe = self.alloc_scope(None, ScopeKind::Universe);
        self.populate_universe(universe);

        let package = self.alloc_scope(
            Some(universe),
            ScopeKind::Package {
                package: self.package_name(),
            },
        );

        let import = self.alloc_scope(
            Some(package),
            ScopeKind::Import {
                imports: ImportMap::from_item_tree(self.tree),
                package: self.package_name(),
            },
        );

        let file_scope = self.alloc_scope(Some(import), ScopeKind::File { file: self.file });

        // 1) Declare all top-level types before creating any class scopes to
        // avoid order dependence.
        for item in &self.tree.items {
            let item_id = item_id(*item);
            let name = Name::from(self.item_name(item_id));
            self.scopes[file_scope]
                .types
                .insert(name, TypeResolution::Source(item_id));

            let ty_name = self.top_level_type_name(item_id);
            self.type_names.insert(item_id, ty_name.clone());
            self.items_by_type_name.insert(ty_name, item_id);
        }

        // 2) Build nested scopes.
        for item in &self.tree.items {
            let item_id = item_id(*item);
            self.build_type_scopes(db, file_scope, self.package_name().as_ref(), None, item_id);
        }

        ScopeBuildResult {
            scopes: ScopeGraph {
                scopes: self.scopes,
                type_names: self.type_names,
                items_by_type_name: self.items_by_type_name,
            },
            file_scope,
            class_scopes: self.class_scopes,
            method_scopes: self.method_scopes,
            constructor_scopes: self.constructor_scopes,
            initializer_scopes: self.initializer_scopes,
            body_scopes: self.body_scopes,
            block_scopes: self.block_scopes,
            stmt_scopes: self.stmt_scopes,
            expr_scopes: self.expr_scopes,
        }
    }

    fn package_name(&self) -> Option<PackageName> {
        self.tree
            .package
            .as_ref()
            .map(|pkg| PackageName::from_dotted(&pkg.name))
    }

    fn populate_universe(&mut self, universe: ScopeId) {
        let primitives = [
            "boolean", "byte", "short", "int", "long", "char", "float", "double", "void",
        ];

        for prim in primitives {
            self.scopes[universe].types.insert(
                Name::from(prim),
                TypeResolution::External(TypeName::from(prim)),
            );
        }
    }

    fn top_level_type_name(&self, item: ItemId) -> TypeName {
        let simple = self.item_name(item);
        match self.package_name() {
            Some(pkg) if !pkg.segments().is_empty() => TypeName::new(format!("{}.{}", pkg, simple)),
            _ => TypeName::new(simple),
        }
    }

    fn ensure_type_name(
        &mut self,
        package: Option<&PackageName>,
        enclosing: Option<&TypeName>,
        item: ItemId,
    ) -> TypeName {
        if let Some(existing) = self.type_names.get(&item) {
            return existing.clone();
        }

        let simple = self.item_name(item);
        let name = match enclosing {
            Some(owner) => TypeName::new(format!("{}${simple}", owner.as_str())),
            None => match package {
                Some(pkg) if !pkg.segments().is_empty() => {
                    TypeName::new(format!("{}.{}", pkg.to_dotted(), simple))
                }
                _ => TypeName::new(simple),
            },
        };

        self.type_names.insert(item, name.clone());
        self.items_by_type_name.insert(name.clone(), item);
        name
    }

    fn build_type_scopes(
        &mut self,
        db: &dyn HirDatabase,
        parent: ScopeId,
        package: Option<&PackageName>,
        enclosing: Option<&TypeName>,
        item: ItemId,
    ) -> ScopeId {
        let ty_name = self.ensure_type_name(package, enclosing, item);
        let class_scope = self.alloc_scope(Some(parent), ScopeKind::Class { item });
        self.class_scopes.insert(item, class_scope);

        // Copy the members out so we can mutably borrow `self` while iterating.
        let members: Vec<Member> = self.item_members(item).to_vec();

        // Populate member namespaces.
        for member in &members {
            match member {
                Member::Field(id) => {
                    let field = self.tree.field(*id);
                    self.scopes[class_scope]
                        .values
                        .insert(Name::from(field.name.clone()), Resolution::Field(*id));
                }
                Member::Method(id) => {
                    let method = self.tree.method(*id);
                    let name = Name::from(method.name.clone());
                    match self.scopes[class_scope].values.get_mut(&name) {
                        Some(Resolution::Methods(existing)) => existing.push(*id),
                        _ => {
                            self.scopes[class_scope]
                                .values
                                .insert(name, Resolution::Methods(vec![*id]));
                        }
                    }
                }
                Member::Constructor(_) => {}
                Member::Initializer(_) => {}
                Member::Type(child) => {
                    let child_id = item_id(child.clone());
                    let name = Name::from(self.item_name(child_id));
                    self.scopes[class_scope]
                        .types
                        .insert(name, TypeResolution::Source(child_id));
                }
            }
        }

        // Build nested members (bodies + nested types).
        for member in &members {
            match member {
                Member::Method(id) => {
                    self.build_method_scopes(db, class_scope, *id);
                }
                Member::Constructor(id) => {
                    self.build_constructor_scopes(db, class_scope, *id);
                }
                Member::Initializer(id) => {
                    self.build_initializer_scopes(db, class_scope, *id);
                }
                Member::Type(child) => {
                    let child_id = item_id(child.clone());
                    self.build_type_scopes(db, class_scope, package, Some(&ty_name), child_id);
                }
                Member::Field(_) => {}
            }
        }

        class_scope
    }

    fn build_method_scopes(
        &mut self,
        db: &dyn HirDatabase,
        parent: ScopeId,
        method: MethodId,
    ) -> ScopeId {
        let method_scope = self.alloc_scope(Some(parent), ScopeKind::Method { method });
        self.method_scopes.insert(method, method_scope);

        let method_data = self.tree.method(method);
        for (idx, param) in method_data.params.iter().enumerate() {
            self.scopes[method_scope].values.insert(
                Name::from(param.name.clone()),
                Resolution::Parameter(ParamRef {
                    owner: ParamOwner::Method(method),
                    index: idx,
                }),
            );
        }

        let body = queries::body(db, method);
        let root_block = self.build_body_scopes(method_scope, BodyOwner::Method(method), &body);
        self.body_scopes
            .insert(BodyOwner::Method(method), root_block);

        method_scope
    }

    fn build_constructor_scopes(
        &mut self,
        db: &dyn HirDatabase,
        parent: ScopeId,
        constructor: ConstructorId,
    ) -> ScopeId {
        let ctor_scope = self.alloc_scope(Some(parent), ScopeKind::Constructor { constructor });
        self.constructor_scopes.insert(constructor, ctor_scope);

        let data = self.tree.constructor(constructor);
        for (idx, param) in data.params.iter().enumerate() {
            self.scopes[ctor_scope].values.insert(
                Name::from(param.name.clone()),
                Resolution::Parameter(ParamRef {
                    owner: ParamOwner::Constructor(constructor),
                    index: idx,
                }),
            );
        }

        let body = queries::constructor_body(db, constructor);
        let root_block =
            self.build_body_scopes(ctor_scope, BodyOwner::Constructor(constructor), &body);
        self.body_scopes
            .insert(BodyOwner::Constructor(constructor), root_block);

        ctor_scope
    }

    fn build_initializer_scopes(
        &mut self,
        db: &dyn HirDatabase,
        parent: ScopeId,
        initializer: InitializerId,
    ) -> ScopeId {
        let init_scope = self.alloc_scope(Some(parent), ScopeKind::Initializer { initializer });
        self.initializer_scopes.insert(initializer, init_scope);

        let body = queries::initializer_body(db, initializer);
        let root_block =
            self.build_body_scopes(init_scope, BodyOwner::Initializer(initializer), &body);
        self.body_scopes
            .insert(BodyOwner::Initializer(initializer), root_block);

        init_scope
    }

    fn build_body_scopes(
        &mut self,
        parent: ScopeId,
        owner: BodyOwner,
        body: &hir::Body,
    ) -> ScopeId {
        self.build_stmt_scopes(parent, owner, body, body.root)
    }

    fn build_stmt_scopes(
        &mut self,
        parent: ScopeId,
        owner: BodyOwner,
        body: &hir::Body,
        stmt_id: hir::StmtId,
    ) -> ScopeId {
        match &body.stmts[stmt_id] {
            hir::Stmt::Block { statements, .. } => {
                let block_scope = self.alloc_scope(
                    Some(parent),
                    ScopeKind::Block {
                        owner,
                        stmt: stmt_id,
                    },
                );
                self.block_scopes.push(block_scope);
                self.stmt_scopes.insert(stmt_id, block_scope);

                for stmt in statements {
                    self.build_stmt_scopes(block_scope, owner, body, *stmt);
                }

                block_scope
            }
            hir::Stmt::Let {
                local, initializer, ..
            } => {
                self.stmt_scopes.insert(stmt_id, parent);
                let local_data = &body.locals[*local];
                self.scopes[parent].values.insert(
                    Name::from(local_data.name.clone()),
                    Resolution::Local(LocalRef {
                        owner,
                        local: *local,
                    }),
                );

                if let Some(expr) = initializer {
                    self.record_expr_scopes(parent, body, *expr);
                }

                parent
            }
            hir::Stmt::Expr { expr, .. } => {
                self.stmt_scopes.insert(stmt_id, parent);
                self.record_expr_scopes(parent, body, *expr);
                parent
            }
            hir::Stmt::Return { expr, .. } => {
                self.stmt_scopes.insert(stmt_id, parent);
                if let Some(expr) = expr {
                    self.record_expr_scopes(parent, body, *expr);
                }
                parent
            }
            hir::Stmt::Empty { .. } => {
                self.stmt_scopes.insert(stmt_id, parent);
                parent
            }
        }
    }

    fn record_expr_scopes(&mut self, scope: ScopeId, body: &hir::Body, expr_id: hir::ExprId) {
        self.expr_scopes.insert(expr_id, scope);

        match &body.exprs[expr_id] {
            hir::Expr::Name { .. } | hir::Expr::Literal { .. } | hir::Expr::Missing { .. } => {}
            hir::Expr::FieldAccess { receiver, .. } => {
                self.record_expr_scopes(scope, body, *receiver);
            }
            hir::Expr::Call { callee, args, .. } => {
                self.record_expr_scopes(scope, body, *callee);
                for arg in args {
                    self.record_expr_scopes(scope, body, *arg);
                }
            }
            hir::Expr::Binary { lhs, rhs, .. } => {
                self.record_expr_scopes(scope, body, *lhs);
                self.record_expr_scopes(scope, body, *rhs);
            }
        }
    }

    fn item_name(&self, item: ItemId) -> &str {
        match item {
            ItemId::Class(id) => &self.tree.class(id).name,
            ItemId::Interface(id) => &self.tree.interface(id).name,
            ItemId::Enum(id) => &self.tree.enum_(id).name,
            ItemId::Record(id) => &self.tree.record(id).name,
            ItemId::Annotation(id) => &self.tree.annotation(id).name,
        }
    }

    fn item_members(&self, item: ItemId) -> &[Member] {
        match item {
            ItemId::Class(id) => &self.tree.class(id).members,
            ItemId::Interface(id) => &self.tree.interface(id).members,
            ItemId::Enum(id) => &self.tree.enum_(id).members,
            ItemId::Record(id) => &self.tree.record(id).members,
            ItemId::Annotation(id) => &self.tree.annotation(id).members,
        }
    }

    fn alloc_scope(&mut self, parent: Option<ScopeId>, kind: ScopeKind) -> ScopeId {
        let id = self.scopes.len();
        self.scopes.push(ScopeData {
            parent,
            kind,
            values: HashMap::new(),
            types: HashMap::new(),
        });
        id
    }
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
