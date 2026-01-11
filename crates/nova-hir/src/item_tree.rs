use crate::ast_id::AstId;
use crate::ids::{
    AnnotationId, ClassId, ConstructorId, EnumId, FieldId, InitializerId, InterfaceId, MethodId,
    RecordId,
};
use nova_types::Span;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemTree {
    pub package: Option<PackageDecl>,
    pub imports: Vec<Import>,
    pub items: Vec<Item>,

    pub classes: BTreeMap<AstId, Class>,
    pub interfaces: BTreeMap<AstId, Interface>,
    pub enums: BTreeMap<AstId, Enum>,
    pub records: BTreeMap<AstId, Record>,
    pub annotations: BTreeMap<AstId, Annotation>,

    pub fields: BTreeMap<AstId, Field>,
    pub methods: BTreeMap<AstId, Method>,
    pub constructors: BTreeMap<AstId, Constructor>,
    pub initializers: BTreeMap<AstId, Initializer>,
}

impl ItemTree {
    #[must_use]
    pub fn class(&self, id: ClassId) -> &Class {
        self.classes.get(&id.ast_id).expect("invalid ClassId")
    }

    #[must_use]
    pub fn interface(&self, id: InterfaceId) -> &Interface {
        self.interfaces
            .get(&id.ast_id)
            .expect("invalid InterfaceId")
    }

    #[must_use]
    pub fn enum_(&self, id: EnumId) -> &Enum {
        self.enums.get(&id.ast_id).expect("invalid EnumId")
    }

    #[must_use]
    pub fn record(&self, id: RecordId) -> &Record {
        self.records.get(&id.ast_id).expect("invalid RecordId")
    }

    #[must_use]
    pub fn annotation(&self, id: AnnotationId) -> &Annotation {
        self.annotations
            .get(&id.ast_id)
            .expect("invalid AnnotationId")
    }

    #[must_use]
    pub fn method(&self, id: MethodId) -> &Method {
        self.methods.get(&id.ast_id).expect("invalid MethodId")
    }

    #[must_use]
    pub fn field(&self, id: FieldId) -> &Field {
        self.fields.get(&id.ast_id).expect("invalid FieldId")
    }

    #[must_use]
    pub fn constructor(&self, id: ConstructorId) -> &Constructor {
        self.constructors
            .get(&id.ast_id)
            .expect("invalid ConstructorId")
    }

    #[must_use]
    pub fn initializer(&self, id: InitializerId) -> &Initializer {
        self.initializers
            .get(&id.ast_id)
            .expect("invalid InitializerId")
    }
}

impl Default for ItemTree {
    fn default() -> Self {
        ItemTree {
            package: None,
            imports: Vec::new(),
            items: Vec::new(),
            classes: BTreeMap::new(),
            interfaces: BTreeMap::new(),
            enums: BTreeMap::new(),
            records: BTreeMap::new(),
            annotations: BTreeMap::new(),
            fields: BTreeMap::new(),
            methods: BTreeMap::new(),
            constructors: BTreeMap::new(),
            initializers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackageDecl {
    pub name: String,
    pub range: Span,
}

impl PartialEq for PackageDecl {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for PackageDecl {}

#[derive(Debug, Clone)]
pub struct Import {
    pub is_static: bool,
    pub is_star: bool,
    pub path: String,
    pub range: Span,
}

impl PartialEq for Import {
    fn eq(&self, other: &Self) -> bool {
        self.is_static == other.is_static
            && self.is_star == other.is_star
            && self.path == other.path
    }
}

impl Eq for Import {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Class(ClassId),
    Interface(InterfaceId),
    Enum(EnumId),
    Record(RecordId),
    Annotation(AnnotationId),
}

#[derive(Debug, Clone)]
pub struct Class {
    pub name: String,
    pub name_range: Span,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Class {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.members == other.members
    }
}

impl Eq for Class {}

#[derive(Debug, Clone)]
pub struct Interface {
    pub name: String,
    pub name_range: Span,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Interface {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.members == other.members
    }
}

impl Eq for Interface {}

#[derive(Debug, Clone)]
pub struct Enum {
    pub name: String,
    pub name_range: Span,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Enum {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.members == other.members
    }
}

impl Eq for Enum {}

#[derive(Debug, Clone)]
pub struct Record {
    pub name: String,
    pub name_range: Span,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.members == other.members
    }
}

impl Eq for Record {}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub name: String,
    pub name_range: Span,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Annotation {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.members == other.members
    }
}

impl Eq for Annotation {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Member {
    Field(FieldId),
    Method(MethodId),
    Constructor(ConstructorId),
    Initializer(InitializerId),
    Type(Item),
}

#[derive(Debug, Clone)]
pub struct Field {
    pub ty: String,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
}

impl PartialEq for Field {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty && self.name == other.name
    }
}

impl Eq for Field {}

#[derive(Debug, Clone)]
pub struct Param {
    pub ty: String,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
}

impl PartialEq for Param {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty && self.name == other.name
    }
}

impl Eq for Param {}

#[derive(Debug, Clone)]
pub struct Method {
    pub return_ty: String,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
    pub params: Vec<Param>,
    pub body: Option<AstId>,
}

impl PartialEq for Method {
    fn eq(&self, other: &Self) -> bool {
        self.return_ty == other.return_ty
            && self.name == other.name
            && self.params == other.params
            && self.body.is_some() == other.body.is_some()
    }
}

impl Eq for Method {}

#[derive(Debug, Clone)]
pub struct Constructor {
    pub name: String,
    pub range: Span,
    pub name_range: Span,
    pub params: Vec<Param>,
    pub body: AstId,
}

impl PartialEq for Constructor {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.params == other.params
    }
}

impl Eq for Constructor {}

#[derive(Debug, Clone)]
pub struct Initializer {
    pub is_static: bool,
    pub range: Span,
    pub body: AstId,
}

impl PartialEq for Initializer {
    fn eq(&self, other: &Self) -> bool {
        self.is_static == other.is_static
    }
}

impl Eq for Initializer {}
