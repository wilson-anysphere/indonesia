use nova_types::{ClassDef, ClassKind, MethodDef, PrimitiveType, Type, TypeEnv, TypeStore};

use pretty_assertions::assert_eq;

#[test]
fn intern_class_id_is_idempotent() {
    let mut store = TypeStore::default();
    let first = store.intern_class_id("com.example.Foo");
    let second = store.intern_class_id("com.example.Foo");
    assert_eq!(first, second);
}

#[test]
fn define_class_overwrites_placeholder() {
    let mut store = TypeStore::default();
    let id = store.intern_class_id("com.example.Foo");

    let ty_param = store.add_type_param("T", vec![Type::Named("java.lang.Object".to_string())]);
    store.define_class(
        id,
        ClassDef {
            name: "com.example.Foo".to_string(),
            kind: ClassKind::Class,
            type_params: vec![ty_param],
            super_class: None,
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![MethodDef {
                name: "m".to_string(),
                type_params: vec![],
                params: vec![Type::Primitive(PrimitiveType::Int)],
                return_type: Type::Void,
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            }],
        },
    );

    assert_eq!(store.class_id("com.example.Foo"), Some(id));
    let def = store.class(id).expect("class should be defined");
    assert_eq!(def.type_params, vec![ty_param]);
    assert_eq!(def.methods.len(), 1);
    assert_eq!(def.methods[0].name, "m");
}

#[test]
fn upsert_class_overwrites_without_changing_id() {
    let mut store = TypeStore::default();

    let first = store.upsert_class(ClassDef {
        name: "com.example.Bar".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let second = store.upsert_class(ClassDef {
        name: "com.example.Bar".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "f".to_string(),
            type_params: vec![],
            params: vec![],
            return_type: Type::Void,
            is_static: false,
            is_varargs: false,
            is_abstract: true,
        }],
    });

    assert_eq!(first, second);
    let def = store.class(first).expect("class should be defined");
    assert_eq!(def.kind, ClassKind::Interface);
    assert_eq!(def.methods.len(), 1);
    assert_eq!(def.methods[0].name, "f");
}
