use std::collections::HashMap;

use nova_types::{
    resolve_constructor_call, resolve_field, CallKind, FieldStub, MethodResolution, MethodStub,
    Type, TypeDefStub, TypeEnv, TypeProvider, TypeStore,
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
        methods: vec![MethodStub {
            name: "<init>".to_string(),
            descriptor: "()V".to_string(),
            signature: None,
            access_flags: 0,
        }],
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
