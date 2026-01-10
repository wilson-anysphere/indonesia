use crate::ids::{
    AnnotationId, ClassId, ConstructorId, EnumId, FieldId, InitializerId, InterfaceId, MethodId,
    RecordId,
};
use nova_types::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemTree {
    pub package: Option<PackageDecl>,
    pub imports: Vec<Import>,
    pub items: Vec<Item>,

    pub classes: Vec<Class>,
    pub interfaces: Vec<Interface>,
    pub enums: Vec<Enum>,
    pub records: Vec<Record>,
    pub annotations: Vec<Annotation>,

    pub fields: Vec<Field>,
    pub methods: Vec<Method>,
    pub constructors: Vec<Constructor>,
    pub initializers: Vec<Initializer>,
}

impl ItemTree {
    #[must_use]
    pub fn class(&self, id: ClassId) -> &Class {
        &self.classes[id.idx()]
    }

    #[must_use]
    pub fn method(&self, id: MethodId) -> &Method {
        &self.methods[id.idx()]
    }

    #[must_use]
    pub fn field(&self, id: FieldId) -> &Field {
        &self.fields[id.idx()]
    }

    #[must_use]
    pub fn constructor(&self, id: ConstructorId) -> &Constructor {
        &self.constructors[id.idx()]
    }

    #[must_use]
    pub fn initializer(&self, id: InitializerId) -> &Initializer {
        &self.initializers[id.idx()]
    }
}

impl Default for ItemTree {
    fn default() -> Self {
        ItemTree {
            package: None,
            imports: Vec::new(),
            items: Vec::new(),
            classes: Vec::new(),
            interfaces: Vec::new(),
            enums: Vec::new(),
            records: Vec::new(),
            annotations: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            constructors: Vec::new(),
            initializers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDecl {
    pub name: String,
    pub range: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub is_static: bool,
    pub is_star: bool,
    pub path: String,
    pub range: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Class(ClassId),
    Interface(InterfaceId),
    Enum(EnumId),
    Record(RecordId),
    Annotation(AnnotationId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    pub name: String,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enum {
    pub name: String,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub name: String,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Member {
    Field(FieldId),
    Method(MethodId),
    Constructor(ConstructorId),
    Initializer(InitializerId),
    Type(Item),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub ty: String,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub ty: String,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub return_ty: String,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
    pub params: Vec<Param>,
    pub body_range: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constructor {
    pub name: String,
    pub range: Span,
    pub name_range: Span,
    pub params: Vec<Param>,
    pub body_range: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Initializer {
    pub is_static: bool,
    pub range: Span,
    pub body_range: Span,
}
