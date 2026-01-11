use std::collections::HashMap;

use nova_core::{Name, TypeName};
use nova_hir::ids::{ConstructorId, FieldId, InitializerId, ItemId, MethodId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

/// Span-free summary of a type definition derived from `nova_hir::item_tree::ItemTree`.
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub kind: TypeKind,
    pub name: Name,
    pub binary_name: TypeName,
    pub enclosing: Option<ItemId>,

    pub fields: HashMap<Name, FieldId>,
    pub methods: HashMap<Name, Vec<MethodId>>,
    pub constructors: Vec<ConstructorId>,
    pub initializers: Vec<InitializerId>,
    pub nested_types: HashMap<Name, ItemId>,
}

