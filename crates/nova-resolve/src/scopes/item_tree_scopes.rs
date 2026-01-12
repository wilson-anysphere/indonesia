use std::collections::HashMap;

use nova_core::{FileId, Name, PackageName, TypeName};
use nova_hir::ids::{ConstructorId, InitializerId, ItemId, MethodId};
use nova_hir::item_tree::{ItemTree, Member};

use crate::import_map::ImportMap;
use crate::resolver::{ParamOwner, ParamRef, Resolution, TypeResolution};

use super::{ScopeData, ScopeGraph, ScopeId, ScopeKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemTreeScopeBuildResult {
    pub scopes: ScopeGraph,
    pub file_scope: ScopeId,
    pub class_scopes: HashMap<ItemId, ScopeId>,
    pub method_scopes: HashMap<MethodId, ScopeId>,
    pub constructor_scopes: HashMap<ConstructorId, ScopeId>,
    pub initializer_scopes: HashMap<InitializerId, ScopeId>,
}

/// Build a [`ScopeGraph`] from an [`ItemTree`] only (no method/constructor bodies).
///
/// This is useful for early type-name resolution in file-structural contexts where the
/// caller already has an `ItemTree` and wants scope ids for types/methods/ctors.
pub fn build_scopes_for_item_tree(file: FileId, tree: &ItemTree) -> ItemTreeScopeBuildResult {
    ItemTreeScopeBuilder::new(file, tree).build()
}

struct ItemTreeScopeBuilder<'a> {
    file: FileId,
    tree: &'a ItemTree,
    scopes: Vec<ScopeData>,
    type_names: HashMap<ItemId, TypeName>,
    items_by_type_name: HashMap<TypeName, ItemId>,

    class_scopes: HashMap<ItemId, ScopeId>,
    method_scopes: HashMap<MethodId, ScopeId>,
    constructor_scopes: HashMap<ConstructorId, ScopeId>,
    initializer_scopes: HashMap<InitializerId, ScopeId>,
}

impl<'a> ItemTreeScopeBuilder<'a> {
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
        }
    }

    fn build(mut self) -> ItemTreeScopeBuildResult {
        let universe = self.alloc_scope(None, ScopeKind::Universe);
        self.populate_universe(universe);

        let package = self.package_name();
        let package_scope = self.alloc_scope(
            Some(universe),
            ScopeKind::Package {
                package: package.clone(),
            },
        );
        let import_scope = self.alloc_scope(
            Some(package_scope),
            ScopeKind::Import {
                imports: ImportMap::from_item_tree(self.tree),
                package: package.clone(),
            },
        );
        let file_scope = self.alloc_scope(Some(import_scope), ScopeKind::File { file: self.file });

        // Predeclare all top-level types to avoid order dependence.
        for item in &self.tree.items {
            let item_id = super::item_id(*item);
            let name = Name::from(self.item_name(item_id));
            self.scopes[file_scope]
                .types
                .insert(name, TypeResolution::Source(item_id));

            let ty_name = self.top_level_type_name(item_id);
            self.type_names.insert(item_id, ty_name.clone());
            self.items_by_type_name.insert(ty_name, item_id);
        }

        for item in &self.tree.items {
            let item_id = super::item_id(*item);
            self.build_type_scopes(file_scope, package.as_ref(), None, item_id);
        }

        ItemTreeScopeBuildResult {
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
        }
    }

    fn package_name(&self) -> Option<PackageName> {
        Some(
            self.tree
                .package
                .as_ref()
                .map(|pkg| PackageName::from_dotted(&pkg.name))
                .unwrap_or_else(PackageName::root),
        )
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
        parent: ScopeId,
        package: Option<&PackageName>,
        enclosing: Option<&TypeName>,
        item: ItemId,
    ) -> ScopeId {
        let ty_name = self.ensure_type_name(package, enclosing, item);
        let class_scope = self.alloc_scope(Some(parent), ScopeKind::Class { item });
        self.class_scopes.insert(item, class_scope);

        let members: Vec<Member> = self.item_members(item).to_vec();

        // Populate member namespaces.
        for member in &members {
            match *member {
                Member::Field(id) => {
                    let field = self.tree.field(id);
                    self.scopes[class_scope]
                        .values
                        .insert(Name::from(field.name.clone()), Resolution::Field(id));
                }
                Member::Method(id) => {
                    let method = self.tree.method(id);
                    let name = Name::from(method.name.clone());
                    match self.scopes[class_scope].values.get_mut(&name) {
                        Some(Resolution::Methods(existing)) => existing.push(id),
                        _ => {
                            self.scopes[class_scope]
                                .values
                                .insert(name, Resolution::Methods(vec![id]));
                        }
                    }
                }
                Member::Constructor(_) => {}
                Member::Initializer(_) => {}
                Member::Type(child) => {
                    let child_id = super::item_id(child);
                    let name = Name::from(self.item_name(child_id));
                    self.scopes[class_scope]
                        .types
                        .insert(name, TypeResolution::Source(child_id));
                }
            }
        }

        for member in &members {
            match *member {
                Member::Method(id) => {
                    self.build_method_scopes(class_scope, id);
                }
                Member::Constructor(id) => {
                    self.build_constructor_scopes(class_scope, id);
                }
                Member::Initializer(id) => {
                    self.build_initializer_scopes(class_scope, id);
                }
                Member::Type(child) => {
                    let child_id = super::item_id(child);
                    self.build_type_scopes(class_scope, package, Some(&ty_name), child_id);
                }
                Member::Field(_) => {}
            }
        }

        class_scope
    }

    fn build_method_scopes(&mut self, parent: ScopeId, method: MethodId) -> ScopeId {
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

        method_scope
    }

    fn build_constructor_scopes(&mut self, parent: ScopeId, constructor: ConstructorId) -> ScopeId {
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

        ctor_scope
    }

    fn build_initializer_scopes(&mut self, parent: ScopeId, initializer: InitializerId) -> ScopeId {
        let init_scope = self.alloc_scope(Some(parent), ScopeKind::Initializer { initializer });
        self.initializer_scopes.insert(initializer, init_scope);
        init_scope
    }

    fn item_name(&self, item: ItemId) -> &str {
        match item {
            ItemId::Class(id) => self.tree.class(id).name.as_str(),
            ItemId::Interface(id) => self.tree.interface(id).name.as_str(),
            ItemId::Enum(id) => self.tree.enum_(id).name.as_str(),
            ItemId::Record(id) => self.tree.record(id).name.as_str(),
            ItemId::Annotation(id) => self.tree.annotation(id).name.as_str(),
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
