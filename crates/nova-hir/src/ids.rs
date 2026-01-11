use crate::ast_id::AstId;
use nova_vfs::FileId;
use std::fmt;

macro_rules! impl_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name {
            pub file: FileId,
            pub ast_id: AstId,
        }

        impl $name {
            #[must_use]
            pub fn new(file: FileId, ast_id: AstId) -> Self {
                Self { file, ast_id }
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    f,
                    concat!(stringify!($name), "({:?}, {:?})"),
                    self.file, self.ast_id
                )
            }
        }
    };
}

impl_id!(ClassId);
impl_id!(InterfaceId);
impl_id!(EnumId);
impl_id!(RecordId);
impl_id!(AnnotationId);

impl_id!(FieldId);
impl_id!(MethodId);
impl_id!(ConstructorId);
impl_id!(InitializerId);

/// A stable identifier for a type-level item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemId {
    Class(ClassId),
    Interface(InterfaceId),
    Enum(EnumId),
    Record(RecordId),
    Annotation(AnnotationId),
}

impl ItemId {
    #[must_use]
    pub fn file(self) -> FileId {
        match self {
            ItemId::Class(id) => id.file,
            ItemId::Interface(id) => id.file,
            ItemId::Enum(id) => id.file,
            ItemId::Record(id) => id.file,
            ItemId::Annotation(id) => id.file,
        }
    }
}
