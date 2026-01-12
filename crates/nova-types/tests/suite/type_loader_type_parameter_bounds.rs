use std::collections::HashMap;

use nova_types::{MethodStub, Type, TypeDefStub, TypeEnv, TypeProvider, TypeStore, TypeStoreLoader};

use pretty_assertions::assert_eq;

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
fn interface_only_class_type_parameter_bound_does_not_add_implicit_object() {
    let mut provider = StubProvider::default();
    provider.insert(TypeDefStub {
        binary_name: "com.example.InterfaceBound".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        // Equivalent to `class InterfaceBound<T extends java.io.Serializable> {}`
        //
        // Note the double-colon which represents an *empty* class bound followed by an
        // interface bound in JVMS signatures.
        signature: Some("<T::Ljava/io/Serializable;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![],
    });

    let mut store = TypeStore::default();
    let mut loader = TypeStoreLoader::new(&mut store, &provider);

    let class_id = loader
        .ensure_class("com.example.InterfaceBound")
        .expect("InterfaceBound should load");

    let serializable = loader.store().well_known().serializable;
    let class_def = loader
        .store()
        .class(class_id)
        .expect("InterfaceBound should be defined");
    assert_eq!(class_def.type_params.len(), 1);

    let tp_id = class_def.type_params[0];
    let tp_def = loader
        .store()
        .type_param(tp_id)
        .expect("type parameter should exist");

    // The *effective* first bound is Serializable, so we should not insert an implicit Object
    // before it.
    assert_eq!(tp_def.upper_bounds, vec![Type::class(serializable, vec![])]);
}

#[test]
fn interface_only_method_type_parameter_bound_does_not_add_implicit_object() {
    let mut provider = StubProvider::default();
    provider.insert(TypeDefStub {
        binary_name: "com.example.MethodBound".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![MethodStub {
            name: "id".to_string(),
            // Erasure descriptor for `<T extends Serializable> T id(T)`.
            descriptor: "(Ljava/io/Serializable;)Ljava/io/Serializable;".to_string(),
            signature: Some("<T::Ljava/io/Serializable;>(TT;)TT;".to_string()),
            access_flags: 0,
        }],
    });

    let mut store = TypeStore::default();
    let mut loader = TypeStoreLoader::new(&mut store, &provider);

    let class_id = loader
        .ensure_class("com.example.MethodBound")
        .expect("MethodBound should load");

    let serializable = loader.store().well_known().serializable;
    let class_def = loader
        .store()
        .class(class_id)
        .expect("MethodBound should be defined");

    let id_method = class_def
        .methods
        .iter()
        .find(|m| m.name == "id")
        .expect("id method should be loaded");
    assert_eq!(id_method.type_params.len(), 1);

    let tp_id = id_method.type_params[0];
    let tp_def = loader
        .store()
        .type_param(tp_id)
        .expect("method type parameter should exist");

    assert_eq!(tp_def.upper_bounds, vec![Type::class(serializable, vec![])]);
}

