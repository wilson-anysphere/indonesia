use nova_hir::ids::{ConstructorId, InitializerId, ItemId, MethodId};

/// Stable identifier for a type definition within a file.
///
/// This is backed by `nova_hir`'s stable `ItemId` (file + index).
pub type TypeDefId = ItemId;

/// Stable identifier for a definition that owns a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefWithBodyId {
    Method(MethodId),
    Constructor(ConstructorId),
    Initializer(InitializerId),
}

/// Stable identifier for a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamId {
    pub owner: DefWithBodyId,
    pub index: u32,
}

impl ParamId {
    #[must_use]
    pub const fn new(owner: DefWithBodyId, index: u32) -> Self {
        Self { owner, index }
    }
}
