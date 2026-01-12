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

impl TypeDef {
    /// Best-effort estimate of heap memory usage of this type definition in bytes.
    ///
    /// This is intended for cheap, deterministic memory accounting (e.g. Nova's
    /// `nova-memory` budgets). The heuristic is not exact; it intentionally
    /// prioritizes stability over precision.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        fn name_bytes(name: &Name) -> u64 {
            name.as_str().len() as u64
        }

        let mut bytes = size_of::<TypeDef>() as u64;

        bytes = bytes.saturating_add(name_bytes(&self.name));
        bytes = bytes.saturating_add(self.binary_name.as_str().len() as u64);

        bytes = bytes.saturating_add((self.fields.capacity() as u64).saturating_mul(
            size_of::<(Name, FieldDef)>() as u64,
        ));
        bytes = bytes.saturating_add(self.fields.capacity() as u64); // HashMap ctrl bytes (best-effort)
        for (name, _) in &self.fields {
            bytes = bytes.saturating_add(name_bytes(name));
        }

        bytes = bytes.saturating_add((self.methods.capacity() as u64).saturating_mul(
            size_of::<(Name, Vec<MethodDef>)>() as u64,
        ));
        bytes = bytes.saturating_add(self.methods.capacity() as u64); // HashMap ctrl bytes
        for (name, methods) in &self.methods {
            bytes = bytes.saturating_add(name_bytes(name));
            bytes = bytes.saturating_add(
                (methods.capacity() as u64).saturating_mul(size_of::<MethodDef>() as u64),
            );
        }

        bytes = bytes.saturating_add(
            (self.constructors.capacity() as u64).saturating_mul(size_of::<ConstructorId>() as u64),
        );
        bytes = bytes.saturating_add(
            (self.initializers.capacity() as u64).saturating_mul(size_of::<InitializerId>() as u64),
        );

        bytes = bytes.saturating_add((self.nested_types.capacity() as u64).saturating_mul(
            size_of::<(Name, ItemId)>() as u64,
        ));
        bytes = bytes.saturating_add(self.nested_types.capacity() as u64); // HashMap ctrl bytes
        for (name, _) in &self.nested_types {
            bytes = bytes.saturating_add(name_bytes(name));
        }

        bytes
    }
}
