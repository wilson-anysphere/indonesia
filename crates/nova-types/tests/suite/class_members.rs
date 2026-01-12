use std::collections::HashMap;

use nova_types::{
    resolve_constructor_call, resolve_field, CallKind, ClassDef, ClassKind, FieldDef, FieldStub,
    MethodResolution, MethodStub, Type, TypeDefStub, TypeEnv, TypeProvider, TypeStore,
};
use nova_types_bridge::ExternalTypeLoader;

#[derive(Default)]
struct StubProvider {
    stubs: HashMap<String, TypeDefStub>,
}

impl StubProvider {
    fn insert(&mut self, stub: TypeDefStub) {
        self.stubs.insert(stub.binary_name.clone(), stub);
    }
}

impl TypeProvider for StubProvider {
    fn lookup_type(&self, binary_name: &str) -> Option<TypeDefStub> {
        self.stubs.get(binary_name).cloned()
    }
}

#[test]
fn resolves_field_from_loaded_stub_class() {
    const ACC_STATIC: u16 = 0x0008;
    const ACC_FINAL: u16 = 0x0010;

    let mut provider = StubProvider::default();
    provider.insert(TypeDefStub {
        binary_name: "com.example.Base".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![FieldStub {
            name: "baseField".to_string(),
            descriptor: "I".to_string(),
            signature: None,
            access_flags: 0,
        }],
        methods: vec![MethodStub {
            name: "<init>".to_string(),
            descriptor: "()V".to_string(),
            signature: None,
            access_flags: 0,
        }],
    });
    provider.insert(TypeDefStub {
        binary_name: "com.example.Foo".to_string(),
        access_flags: 0,
        super_binary_name: Some("com.example.Base".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![
            FieldStub {
                name: "instanceField".to_string(),
                descriptor: "Ljava/lang/String;".to_string(),
                signature: None,
                access_flags: 0,
            },
            FieldStub {
                name: "CONST".to_string(),
                descriptor: "I".to_string(),
                signature: None,
                access_flags: ACC_STATIC | ACC_FINAL,
            },
        ],
        methods: vec![
            MethodStub {
                name: "<init>".to_string(),
                descriptor: "()V".to_string(),
                signature: None,
                access_flags: 0,
            },
            MethodStub {
                name: "greet".to_string(),
                descriptor: "(I)Ljava/lang/String;".to_string(),
                signature: None,
                access_flags: 0,
            },
            MethodStub {
                name: "util".to_string(),
                descriptor: "()I".to_string(),
                signature: None,
                access_flags: ACC_STATIC,
            },
        ],
    });

    let mut env = TypeStore::with_minimal_jdk();
    let foo = {
        let mut loader = ExternalTypeLoader::new(&mut env, &provider);
        loader
            .ensure_class("com.example.Foo")
            .expect("Foo stub should load")
    };

    let receiver = Type::class(foo, vec![]);

    let field = resolve_field(&env, &receiver, "instanceField", CallKind::Instance)
        .expect("field should resolve");
    assert_eq!(field.ty, Type::class(env.well_known().string, vec![]));
    assert!(!field.is_static);
    assert!(!field.is_final);

    // Inherited field.
    let inherited =
        resolve_field(&env, &receiver, "baseField", CallKind::Instance).expect("inherited field");
    assert_eq!(inherited.ty, Type::int());

    // Static field can be resolved from a static access.
    let konst = resolve_field(&env, &receiver, "CONST", CallKind::Static).expect("static field");
    assert_eq!(konst.ty, Type::int());
    assert!(konst.is_static);
    assert!(konst.is_final);

    // But instance field access through a static receiver should fail.
    assert!(resolve_field(&env, &receiver, "instanceField", CallKind::Static).is_none());

    // Basic method stub translation (descriptor-based, no Signature attribute).
    let foo_def = env.class(foo).expect("Foo should be defined");

    let greet = foo_def
        .methods
        .iter()
        .find(|m| m.name == "greet")
        .expect("greet method should be loaded");
    assert_eq!(greet.params, vec![Type::int()]);
    assert_eq!(
        greet.return_type,
        Type::class(env.well_known().string, vec![])
    );
    assert!(!greet.is_static);
    assert!(!greet.is_varargs);
    assert!(!greet.is_abstract);

    let util = foo_def
        .methods
        .iter()
        .find(|m| m.name == "util")
        .expect("util method should be loaded");
    assert_eq!(util.params, vec![]);
    assert_eq!(util.return_type, Type::int());
    assert!(util.is_static);
}

#[test]
fn resolve_field_intersection_receiver_is_order_independent() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    let iface = env.add_class(ClassDef {
        name: "com.example.IFields".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![FieldDef {
            name: "foo".to_string(),
            ty: Type::class(object, vec![]),
            is_static: true,
            is_final: true,
        }],
        constructors: vec![],
        methods: vec![],
    });

    let class = env.add_class(ClassDef {
        name: "com.example.AFields".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![Type::class(iface, vec![])],
        fields: vec![FieldDef {
            name: "foo".to_string(),
            ty: Type::class(string, vec![]),
            is_static: false,
            is_final: false,
        }],
        constructors: vec![],
        methods: vec![],
    });

    let receiver_iface_first =
        Type::Intersection(vec![Type::class(iface, vec![]), Type::class(class, vec![])]);
    let receiver_class_first =
        Type::Intersection(vec![Type::class(class, vec![]), Type::class(iface, vec![])]);

    let f1 = resolve_field(&env, &receiver_iface_first, "foo", CallKind::Instance)
        .expect("field should resolve");
    let f2 = resolve_field(&env, &receiver_class_first, "foo", CallKind::Instance)
        .expect("field should resolve");

    // Should always prefer the class-bound field regardless of intersection ordering.
    assert_eq!(f1.ty, Type::class(string, vec![]));
    assert!(!f1.is_static);
    assert_eq!(f2.ty, Type::class(string, vec![]));
    assert!(!f2.is_static);
}

#[test]
fn resolves_constructor_overloads_from_loaded_stub_class() {
    const ACC_VARARGS: u16 = 0x0080;

    let mut provider = StubProvider::default();
    provider.insert(TypeDefStub {
        binary_name: "com.example.Ctors".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![
            MethodStub {
                name: "<init>".to_string(),
                descriptor: "()V".to_string(),
                signature: None,
                access_flags: 0,
            },
            MethodStub {
                name: "<init>".to_string(),
                descriptor: "(I)V".to_string(),
                signature: None,
                access_flags: 0,
            },
            MethodStub {
                name: "<init>".to_string(),
                descriptor: "([I)V".to_string(),
                signature: None,
                access_flags: ACC_VARARGS,
            },
        ],
    });

    let mut env = TypeStore::with_minimal_jdk();
    let class = {
        let mut loader = ExternalTypeLoader::new(&mut env, &provider);
        loader
            .ensure_class("com.example.Ctors")
            .expect("Ctors stub should load")
    };

    let MethodResolution::Found(res) = resolve_constructor_call(&env, class, &[], None) else {
        panic!("expected constructor resolution");
    };
    assert_eq!(res.params, vec![]);
    assert!(!res.is_varargs);

    let MethodResolution::Found(res) = resolve_constructor_call(&env, class, &[Type::int()], None)
    else {
        panic!("expected constructor resolution");
    };
    assert_eq!(res.params, vec![Type::int()]);
    assert!(!res.is_varargs);

    let MethodResolution::Found(res) =
        resolve_constructor_call(&env, class, &[Type::int(), Type::int()], None)
    else {
        panic!("expected constructor resolution");
    };
    assert_eq!(res.params, vec![Type::int(), Type::int()]);
    assert!(res.is_varargs);
    assert!(res.used_varargs);
}
