use crate::ast_id::AstId;
use crate::ids::{
    AnnotationId, ClassId, ConstructorId, EnumId, FieldId, InitializerId, InterfaceId, MethodId,
    RecordId,
};
use nova_types::Span;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    pub raw: u16,
}

impl Modifiers {
    pub const PUBLIC: u16 = 1 << 0;
    pub const PROTECTED: u16 = 1 << 1;
    pub const PRIVATE: u16 = 1 << 2;
    pub const STATIC: u16 = 1 << 3;
    pub const FINAL: u16 = 1 << 4;
    pub const ABSTRACT: u16 = 1 << 5;
    pub const NATIVE: u16 = 1 << 6;
    pub const SYNCHRONIZED: u16 = 1 << 7;
    pub const TRANSIENT: u16 = 1 << 8;
    pub const VOLATILE: u16 = 1 << 9;
    pub const STRICTFP: u16 = 1 << 10;
    pub const DEFAULT: u16 = 1 << 11;
    pub const SEALED: u16 = 1 << 12;
    pub const NON_SEALED: u16 = 1 << 13;
}

#[derive(Debug, Clone)]
pub struct AnnotationUse {
    pub name: String,
    pub range: Span,
}

impl PartialEq for AnnotationUse {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for AnnotationUse {}

#[derive(Debug, Clone)]
pub struct TypeParam {
    pub name: String,
    pub name_range: Span,
    pub bounds: Vec<String>,
    pub bounds_ranges: Vec<Span>,
}

impl PartialEq for TypeParam {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.bounds == other.bounds
    }
}

impl Eq for TypeParam {}

#[derive(Debug, Clone)]
pub struct ModuleDecl {
    pub name: String,
    pub name_range: Span,
    pub is_open: bool,
    pub directives: Vec<ModuleDirective>,
    pub range: Span,
    pub body_range: Span,
}

impl PartialEq for ModuleDecl {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.is_open == other.is_open
            && self.directives == other.directives
    }
}

impl Eq for ModuleDecl {}

#[derive(Debug, Clone)]
pub enum ModuleDirective {
    Requires {
        module: String,
        is_transitive: bool,
        is_static: bool,
        range: Span,
    },
    Exports {
        package: String,
        to: Vec<String>,
        range: Span,
    },
    Opens {
        package: String,
        to: Vec<String>,
        range: Span,
    },
    Uses {
        service: String,
        range: Span,
    },
    Provides {
        service: String,
        implementations: Vec<String>,
        range: Span,
    },
    Unknown {
        range: Span,
    },
}

impl PartialEq for ModuleDirective {
    fn eq(&self, other: &Self) -> bool {
        use ModuleDirective::*;
        match (self, other) {
            (
                Requires {
                    module,
                    is_transitive,
                    is_static,
                    ..
                },
                Requires {
                    module: other_module,
                    is_transitive: other_transitive,
                    is_static: other_static,
                    ..
                },
            ) => {
                module == other_module
                    && is_transitive == other_transitive
                    && is_static == other_static
            }
            (
                Exports { package, to, .. },
                Exports {
                    package: other_package,
                    to: other_to,
                    ..
                },
            ) => package == other_package && to == other_to,
            (
                Opens { package, to, .. },
                Opens {
                    package: other_package,
                    to: other_to,
                    ..
                },
            ) => package == other_package && to == other_to,
            (
                Uses { service, .. },
                Uses {
                    service: other_service,
                    ..
                },
            ) => service == other_service,
            (
                Provides {
                    service,
                    implementations,
                    ..
                },
                Provides {
                    service: other_service,
                    implementations: other_impls,
                    ..
                },
            ) => service == other_service && implementations == other_impls,
            (Unknown { .. }, Unknown { .. }) => true,
            _ => false,
        }
    }
}

impl Eq for ModuleDirective {}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemTree {
    pub package: Option<PackageDecl>,
    pub imports: Vec<Import>,
    pub module: Option<ModuleDecl>,
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

#[derive(Debug, Clone)]
pub struct RecordComponent {
    pub ty: String,
    pub ty_range: Span,
    pub name: String,
    pub name_range: Span,
}

impl PartialEq for RecordComponent {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty && self.name == other.name
    }
}

impl Eq for RecordComponent {}

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
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub type_params: Vec<TypeParam>,
    pub extends: Vec<String>,
    pub extends_ranges: Vec<Span>,
    pub implements: Vec<String>,
    pub implements_ranges: Vec<Span>,
    pub permits: Vec<String>,
    pub permits_ranges: Vec<Span>,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Class {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.type_params == other.type_params
            && self.extends == other.extends
            && self.implements == other.implements
            && self.permits == other.permits
            && self.members == other.members
    }
}

impl Eq for Class {}

#[derive(Debug, Clone)]
pub struct Interface {
    pub name: String,
    pub name_range: Span,
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub type_params: Vec<TypeParam>,
    pub extends: Vec<String>,
    pub extends_ranges: Vec<Span>,
    pub permits: Vec<String>,
    pub permits_ranges: Vec<Span>,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Interface {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.type_params == other.type_params
            && self.extends == other.extends
            && self.permits == other.permits
            && self.members == other.members
    }
}

impl Eq for Interface {}

#[derive(Debug, Clone)]
pub struct Enum {
    pub name: String,
    pub name_range: Span,
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub implements: Vec<String>,
    pub implements_ranges: Vec<Span>,
    pub permits: Vec<String>,
    pub permits_ranges: Vec<Span>,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Enum {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.implements == other.implements
            && self.permits == other.permits
            && self.members == other.members
    }
}

impl Eq for Enum {}

#[derive(Debug, Clone)]
pub struct Record {
    pub name: String,
    pub name_range: Span,
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub type_params: Vec<TypeParam>,
    pub implements: Vec<String>,
    pub implements_ranges: Vec<Span>,
    pub permits: Vec<String>,
    pub permits_ranges: Vec<Span>,
    pub components: Vec<RecordComponent>,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.type_params == other.type_params
            && self.implements == other.implements
            && self.permits == other.permits
            && self.components == other.components
            && self.members == other.members
    }
}

impl Eq for Record {}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub name: String,
    pub name_range: Span,
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub range: Span,
    pub body_range: Span,
    pub members: Vec<Member>,
}

impl PartialEq for Annotation {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.members == other.members
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Field,
    EnumConstant,
    RecordComponent,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub kind: FieldKind,
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub ty: String,
    pub ty_range: Span,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
}

impl PartialEq for Field {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.ty == other.ty
            && self.name == other.name
    }
}

impl Eq for Field {}

#[derive(Debug, Clone)]
pub struct Param {
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub ty: String,
    pub ty_range: Span,
    pub is_varargs: bool,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
}

impl PartialEq for Param {
    fn eq(&self, other: &Self) -> bool {
        self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.ty == other.ty
            && self.is_varargs == other.is_varargs
            && self.name == other.name
    }
}

impl Eq for Param {}

#[derive(Debug, Clone)]
pub struct Method {
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub type_params: Vec<TypeParam>,
    pub return_ty: String,
    pub return_ty_range: Span,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
    pub params: Vec<Param>,
    pub throws: Vec<String>,
    pub throws_ranges: Vec<Span>,
    pub body: Option<AstId>,
}

impl PartialEq for Method {
    fn eq(&self, other: &Self) -> bool {
        self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.type_params == other.type_params
            && self.return_ty == other.return_ty
            && self.name == other.name
            && self.params == other.params
            && self.throws == other.throws
            && self.body.is_some() == other.body.is_some()
    }
}

impl Eq for Method {}

#[derive(Debug, Clone)]
pub struct Constructor {
    pub modifiers: Modifiers,
    pub annotations: Vec<AnnotationUse>,
    pub type_params: Vec<TypeParam>,
    pub name: String,
    pub range: Span,
    pub name_range: Span,
    pub params: Vec<Param>,
    pub throws: Vec<String>,
    pub throws_ranges: Vec<Span>,
    pub body: Option<AstId>,
}

impl PartialEq for Constructor {
    fn eq(&self, other: &Self) -> bool {
        self.modifiers == other.modifiers
            && self.annotations == other.annotations
            && self.type_params == other.type_params
            && self.name == other.name
            && self.params == other.params
            && self.throws == other.throws
            && self.body.is_some() == other.body.is_some()
    }
}

impl Eq for Constructor {}

#[derive(Debug, Clone)]
pub struct Initializer {
    pub is_static: bool,
    pub range: Span,
    pub body: Option<AstId>,
}

impl PartialEq for Initializer {
    fn eq(&self, other: &Self) -> bool {
        self.is_static == other.is_static && self.body.is_some() == other.body.is_some()
    }
}

impl Eq for Initializer {}
