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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDef {
    pub id: FieldId,
    pub is_static: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodDef {
    pub id: MethodId,
    pub is_static: bool,
}

/// Span-free summary of a type definition derived from `nova_hir::item_tree::ItemTree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDef {
    pub kind: TypeKind,
    pub name: Name,
    pub binary_name: TypeName,
    pub enclosing: Option<ItemId>,
    /// Whether this type declaration is `static`.
    ///
    /// This is only meaningful for member types. Top-level types cannot be declared `static`
    /// in Java.
    pub is_static: bool,

    pub fields: HashMap<Name, FieldDef>,
    pub methods: HashMap<Name, Vec<MethodDef>>,
    pub constructors: Vec<ConstructorId>,
    pub initializers: Vec<InitializerId>,
    pub nested_types: HashMap<Name, ItemId>,
}
