use std::collections::HashMap;

use nova_types::{FieldStub, Type, TypeDefStub, TypeEnv, TypeProvider, TypeStore};
use nova_types_bridge::ExternalTypeLoader;

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
fn type_store_loader_flattens_inner_class_args_across_segments() {
    let mut provider = StubProvider::default();

    provider.insert(TypeDefStub {
        binary_name: "com.example.Outer".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<T:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![],
    });

    // Inner class that (for signature purposes) expects both the outer and inner
    // type arguments, i.e. `Outer<T>.Inner<U>` => `Outer$Inner<T, U>`.
    provider.insert(TypeDefStub {
        binary_name: "com.example.Outer$Inner".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<T:Ljava/lang/Object;U:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![],
    });

    provider.insert(TypeDefStub {
        binary_name: "com.example.Ref".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<T:Ljava/lang/Object;U:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![FieldStub {
            name: "value".to_string(),
            // Erased descriptor.
            descriptor: "Lcom/example/Outer$Inner;".to_string(),
            // Generic signature with per-segment args.
            signature: Some("Lcom/example/Outer<TT;>.Inner<TU;>;".to_string()),
            access_flags: 0,
        }],
        methods: vec![],
    });

    let mut store = TypeStore::default();
    let mut loader = ExternalTypeLoader::new(&mut store, &provider);

    let ref_id = loader
        .ensure_class("com.example.Ref")
        .expect("Ref should load");
    let inner_id = loader
        .store
        .lookup_class("com.example.Outer$Inner")
        .expect("Outer$Inner should have been loaded");

    let ref_def = loader.store.class(ref_id).unwrap();
    let (t, u) = (ref_def.type_params[0], ref_def.type_params[1]);

    let field = ref_def
        .fields
        .iter()
        .find(|f| f.name == "value")
        .expect("Ref.value field should exist");

    assert_eq!(
        field.ty,
        Type::class(inner_id, vec![Type::TypeVar(t), Type::TypeVar(u)])
    );
}

#[test]
fn type_store_loader_reconciles_inner_class_arg_mismatches_by_dropping_leading_args() {
    let mut provider = StubProvider::default();

    provider.insert(TypeDefStub {
        binary_name: "com.example.Outer".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<T:Ljava/lang/Object;U:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![],
    });

    // Target class expects 2 type arguments.
    provider.insert(TypeDefStub {
        binary_name: "com.example.Outer$Inner".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<A:Ljava/lang/Object;B:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![],
    });

    // Signature provides 3 args across segments (`Outer<T, U>.Inner<V>`). The loader should
    // keep the suffix to match the target's arity => `[U, V]`.
    provider.insert(TypeDefStub {
        binary_name: "com.example.Ref3".to_string(),
        access_flags: 0,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some(
            "<T:Ljava/lang/Object;U:Ljava/lang/Object;V:Ljava/lang/Object;>Ljava/lang/Object;"
                .to_string(),
        ),
        fields: vec![FieldStub {
            name: "value".to_string(),
            descriptor: "Lcom/example/Outer$Inner;".to_string(),
            signature: Some("Lcom/example/Outer<TT;TU;>.Inner<TV;>;".to_string()),
            access_flags: 0,
        }],
        methods: vec![],
    });

    let mut store = TypeStore::default();
    let mut loader = ExternalTypeLoader::new(&mut store, &provider);

    let ref_id = loader
        .ensure_class("com.example.Ref3")
        .expect("Ref3 should load");
    let inner_id = loader
        .store
        .lookup_class("com.example.Outer$Inner")
        .expect("Outer$Inner should have been loaded");

    let ref_def = loader.store.class(ref_id).unwrap();
    let (t, u, v) = (
        ref_def.type_params[0],
        ref_def.type_params[1],
        ref_def.type_params[2],
    );

    let field = ref_def
        .fields
        .iter()
        .find(|f| f.name == "value")
        .expect("Ref3.value field should exist");

    // Uses `[U, V]` (drops `T`).
    assert_eq!(
        field.ty,
        Type::class(inner_id, vec![Type::TypeVar(u), Type::TypeVar(v)])
    );

    // Sanity: we didn't accidentally capture the first type argument.
    assert_ne!(
        field.ty,
        Type::class(inner_id, vec![Type::TypeVar(t), Type::TypeVar(v)])
    );
}
