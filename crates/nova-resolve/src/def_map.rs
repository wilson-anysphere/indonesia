use std::collections::hash_map::Entry;
use std::collections::HashMap;

use nova_core::{FileId, Name, PackageName, QualifiedName, TypeName};
use nova_hir::ids::{ConstructorId, FieldId, InitializerId, ItemId, MethodId};
use nova_hir::item_tree::{Import as HirImport, Item, ItemTree, Member};

use crate::types::{TypeDef, TypeKind};

/// A normalized import representation derived from `ItemTree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Import {
    /// `import java.util.List;`
    TypeSingle { ty: QualifiedName },
    /// `import java.util.*;`
    TypeStar { package: PackageName },
    /// `import static java.lang.Math.max;`
    StaticSingle { ty: QualifiedName, member: Name },
    /// `import static java.lang.Math.*;`
    StaticStar { ty: QualifiedName },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefMapError {
    DuplicateTopLevelType {
        name: Name,
        first: ItemId,
        second: ItemId,
    },
    DuplicateNestedType {
        owner: ItemId,
        name: Name,
        first: ItemId,
        second: ItemId,
    },
    DuplicateField {
        owner: ItemId,
        name: Name,
        first: FieldId,
        second: FieldId,
    },
    MalformedStaticImport {
        path: String,
    },
}

/// Span-free, stable-ID definition map for a single Java source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefMap {
    file: FileId,
    package: Option<PackageName>,
    imports: Vec<Import>,

    top_level_types: HashMap<Name, ItemId>,
    types: HashMap<ItemId, TypeDef>,

    field_owners: HashMap<FieldId, ItemId>,
    method_owners: HashMap<MethodId, ItemId>,
    constructor_owners: HashMap<ConstructorId, ItemId>,
    initializer_owners: HashMap<InitializerId, ItemId>,

    errors: Vec<DefMapError>,
}

impl DefMap {
    #[must_use]
    pub fn from_item_tree(file: FileId, tree: &ItemTree) -> Self {
        let package = tree
            .package
            .as_ref()
            .map(|pkg| PackageName::from_dotted(&pkg.name));

        let mut def_map = Self {
            file,
            package,
            imports: Vec::new(),
            top_level_types: HashMap::new(),
            types: HashMap::new(),
            field_owners: HashMap::new(),
            method_owners: HashMap::new(),
            constructor_owners: HashMap::new(),
            initializer_owners: HashMap::new(),
            errors: Vec::new(),
        };

        for import in &tree.imports {
            if let Some(import) = def_map.lower_import(import) {
                def_map.imports.push(import);
            }
        }

        for &item in &tree.items {
            let id = item_to_item_id(item);
            let name = Name::from(item_name(tree, id));

            match def_map.top_level_types.entry(name.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(id);
                }
                Entry::Occupied(entry) => {
                    def_map.errors.push(DefMapError::DuplicateTopLevelType {
                        name,
                        first: *entry.get(),
                        second: id,
                    });
                }
            }

            def_map.collect_type(tree, id, None, None);
        }

        def_map
    }

    #[must_use]
    pub fn file(&self) -> FileId {
        self.file
    }

    #[must_use]
    pub fn package(&self) -> Option<&PackageName> {
        self.package.as_ref()
    }

    #[must_use]
    pub fn imports(&self) -> &[Import] {
        &self.imports
    }

    #[must_use]
    pub fn errors(&self) -> &[DefMapError] {
        &self.errors
    }

    #[must_use]
    pub fn type_def(&self, id: ItemId) -> Option<&TypeDef> {
        self.types.get(&id)
    }

    /// Iterate all type definitions (top-level and nested) declared in this file.
    ///
    /// The iteration order is unspecified; callers that need determinism should
    /// sort the results by `TypeDef.binary_name` or by `ItemId`.
    pub fn iter_type_defs(&self) -> impl Iterator<Item = (ItemId, &TypeDef)> {
        self.types.iter().map(|(id, def)| (*id, def))
    }

    #[must_use]
    pub fn binary_name(&self, id: ItemId) -> Option<&TypeName> {
        self.types.get(&id).map(|ty| &ty.binary_name)
    }

    #[must_use]
    pub fn lookup_top_level(&self, name: &Name) -> Option<ItemId> {
        self.top_level_types.get(name).copied()
    }

    #[must_use]
    pub fn lookup_nested(&self, owner: ItemId, name: &Name) -> Option<ItemId> {
        self.types
            .get(&owner)
            .and_then(|owner| owner.nested_types.get(name))
            .copied()
    }

    #[must_use]
    pub fn field_owner(&self, field: FieldId) -> Option<ItemId> {
        self.field_owners.get(&field).copied()
    }

    #[must_use]
    pub fn method_owner(&self, method: MethodId) -> Option<ItemId> {
        self.method_owners.get(&method).copied()
    }

    #[must_use]
    pub fn constructor_owner(&self, ctor: ConstructorId) -> Option<ItemId> {
        self.constructor_owners.get(&ctor).copied()
    }

    #[must_use]
    pub fn initializer_owner(&self, init: InitializerId) -> Option<ItemId> {
        self.initializer_owners.get(&init).copied()
    }

    fn lower_import(&mut self, import: &HirImport) -> Option<Import> {
        if !import.is_static {
            if import.is_star {
                return Some(Import::TypeStar {
                    package: PackageName::from_dotted(&import.path),
                });
            }
            return Some(Import::TypeSingle {
                ty: QualifiedName::from_dotted(&import.path),
            });
        }

        if import.is_star {
            return Some(Import::StaticStar {
                ty: QualifiedName::from_dotted(&import.path),
            });
        }

        let mut segments: Vec<&str> = import.path.split('.').collect();
        if segments.len() < 2 {
            self.errors.push(DefMapError::MalformedStaticImport {
                path: import.path.clone(),
            });
            return None;
        }
        let member = segments.pop().expect("len >= 2");
        let owner = segments.join(".");
        Some(Import::StaticSingle {
            ty: QualifiedName::from_dotted(&owner),
            member: Name::from(member),
        })
    }

    fn collect_type(
        &mut self,
        tree: &ItemTree,
        id: ItemId,
        enclosing: Option<ItemId>,
        enclosing_binary_name: Option<&TypeName>,
    ) {
        if self.types.contains_key(&id) {
            return;
        }

        let kind = item_kind(id);
        let name = Name::from(item_name(tree, id));
        let binary_name = match enclosing_binary_name {
            Some(parent) => TypeName::new(format!("{}${}", parent.as_str(), name.as_str())),
            None => binary_name_for_top_level(self.package.as_ref(), &name),
        };

        let mut type_def = TypeDef {
            kind,
            name: name.clone(),
            binary_name: binary_name.clone(),
            enclosing,
            fields: HashMap::new(),
            methods: HashMap::new(),
            constructors: Vec::new(),
            initializers: Vec::new(),
            nested_types: HashMap::new(),
        };

        for member in item_members(tree, id) {
            match member {
                Member::Field(field_id) => {
                    self.field_owners.insert(*field_id, id);

                    let field_name = Name::from(tree.field(*field_id).name.as_str());
                    match type_def.fields.entry(field_name.clone()) {
                        Entry::Vacant(entry) => {
                            entry.insert(*field_id);
                        }
                        Entry::Occupied(entry) => {
                            self.errors.push(DefMapError::DuplicateField {
                                owner: id,
                                name: field_name,
                                first: *entry.get(),
                                second: *field_id,
                            });
                        }
                    }
                }
                Member::Method(method_id) => {
                    self.method_owners.insert(*method_id, id);

                    let method_name = Name::from(tree.method(*method_id).name.as_str());
                    type_def
                        .methods
                        .entry(method_name)
                        .or_default()
                        .push(*method_id);
                }
                Member::Constructor(ctor_id) => {
                    self.constructor_owners.insert(*ctor_id, id);
                    type_def.constructors.push(*ctor_id);
                }
                Member::Initializer(init_id) => {
                    self.initializer_owners.insert(*init_id, id);
                    type_def.initializers.push(*init_id);
                }
                Member::Type(item) => {
                    let nested_id = item_to_item_id(*item);
                    let nested_name = Name::from(item_name(tree, nested_id));

                    match type_def.nested_types.entry(nested_name.clone()) {
                        Entry::Vacant(entry) => {
                            entry.insert(nested_id);
                        }
                        Entry::Occupied(entry) => {
                            self.errors.push(DefMapError::DuplicateNestedType {
                                owner: id,
                                name: nested_name,
                                first: *entry.get(),
                                second: nested_id,
                            });
                        }
                    }

                    self.collect_type(tree, nested_id, Some(id), Some(&binary_name));
                }
            }
        }

        self.types.insert(id, type_def);
    }
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

fn item_kind(id: ItemId) -> TypeKind {
    match id {
        ItemId::Class(_) => TypeKind::Class,
        ItemId::Interface(_) => TypeKind::Interface,
        ItemId::Enum(_) => TypeKind::Enum,
        ItemId::Record(_) => TypeKind::Record,
        ItemId::Annotation(_) => TypeKind::Annotation,
    }
}

fn item_name<'a>(tree: &'a ItemTree, id: ItemId) -> &'a str {
    match id {
        ItemId::Class(id) => tree.class(id).name.as_str(),
        ItemId::Interface(id) => tree.interface(id).name.as_str(),
        ItemId::Enum(id) => tree.enum_(id).name.as_str(),
        ItemId::Record(id) => tree.record(id).name.as_str(),
        ItemId::Annotation(id) => tree.annotation(id).name.as_str(),
    }
}

fn item_members<'a>(tree: &'a ItemTree, id: ItemId) -> &'a [Member] {
    match id {
        ItemId::Class(id) => &tree.class(id).members,
        ItemId::Interface(id) => &tree.interface(id).members,
        ItemId::Enum(id) => &tree.enum_(id).members,
        ItemId::Record(id) => &tree.record(id).members,
        ItemId::Annotation(id) => &tree.annotation(id).members,
    }
}

fn binary_name_for_top_level(package: Option<&PackageName>, name: &Name) -> TypeName {
    match package {
        Some(pkg) if !pkg.segments().is_empty() => {
            TypeName::new(format!("{}.{}", pkg.to_dotted(), name.as_str()))
        }
        _ => TypeName::new(name.as_str()),
    }
}
