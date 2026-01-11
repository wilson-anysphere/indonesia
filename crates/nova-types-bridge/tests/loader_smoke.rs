use std::collections::HashMap;

use nova_types::{ClassKind, MethodStub, Type, TypeDefStub, TypeEnv, TypeProvider};
use nova_types_bridge::ExternalTypeLoader;

#[derive(Default)]
struct MapProvider {
    stubs: HashMap<String, TypeDefStub>,
}

impl TypeProvider for MapProvider {
    fn lookup_type(&self, binary_name: &str) -> Option<TypeDefStub> {
        self.stubs.get(binary_name).cloned()
    }
}

#[test]
fn loads_generic_class_without_panicking() {
    let list_stub = TypeDefStub {
        binary_name: "java.util.List".to_string(),
        access_flags: 0x0200, // ACC_INTERFACE
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<E:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![
            MethodStub {
                name: "get".to_string(),
                descriptor: "(I)Ljava/lang/Object;".to_string(),
                signature: Some("(I)TE;".to_string()),
                access_flags: 0x0400, // ACC_ABSTRACT
            },
            MethodStub {
                name: "add".to_string(),
                descriptor: "(Ljava/lang/Object;)Z".to_string(),
                signature: Some("(TE;)Z".to_string()),
                access_flags: 0x0400, // ACC_ABSTRACT
            },
        ],
    };

    let mut provider = MapProvider::default();
    provider
        .stubs
        .insert("java.util.List".to_string(), list_stub);

    let mut store = nova_types::TypeStore::default();
    let mut loader = ExternalTypeLoader::new(&mut store, &provider);

    let list_id = loader
        .ensure_class("java.util.List")
        .expect("class should load from provider");
    let def = store.class(list_id).expect("class def should be present");

    assert_eq!(def.kind, ClassKind::Interface);
    assert_eq!(def.type_params.len(), 1);

    let e = def.type_params[0];
    let get = def.methods.iter().find(|m| m.name == "get").unwrap();
    assert_eq!(get.return_type, Type::TypeVar(e));
}

#[test]
fn resolves_self_referential_type_param_bounds() {
    // Roughly models `Enum<E extends Enum<E>>`.
    let enum_stub = TypeDefStub {
        binary_name: "java.lang.Enum".to_string(),
        access_flags: 0x0000,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<E:Ljava/lang/Enum<TE;>;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![],
    };

    let mut provider = MapProvider::default();
    provider
        .stubs
        .insert("java.lang.Enum".to_string(), enum_stub);

    let mut store = nova_types::TypeStore::default();
    let mut loader = ExternalTypeLoader::new(&mut store, &provider);

    let enum_id = loader
        .ensure_class("java.lang.Enum")
        .expect("class should load from provider");

    let def = store.class(enum_id).expect("class def should be present");
    assert_eq!(def.type_params.len(), 1);
    let e = def.type_params[0];

    let bounds = store.type_param(e).expect("type param should be defined");
    assert_eq!(bounds.upper_bounds.len(), 1);
    assert_eq!(
        bounds.upper_bounds[0],
        Type::class(enum_id, vec![Type::TypeVar(e)])
    );
}
