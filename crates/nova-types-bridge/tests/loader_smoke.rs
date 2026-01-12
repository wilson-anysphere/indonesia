use std::collections::HashMap;

use nova_types::{
    ClassDef, ClassKind, ConstructorDef, FieldStub, MethodDef, MethodStub, PrimitiveType, Type,
    TypeDefStub, TypeEnv, TypeProvider, TypeStore, WildcardBound,
};
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
fn does_not_overwrite_non_placeholder_minimal_jdk_types() {
    // Regression test: `ExternalTypeLoader::ensure_class` should *not* overwrite the in-memory
    // minimal JDK type model (which is more precise than Nova's built-in `nova-jdk` stubs).
    //
    // Previously, callers could accidentally clobber definitions like `java.lang.Math` (losing
    // overloads) and `java.util.Collections.emptyList` (losing generics), which then broke target
    // typing and overload resolution in downstream type checking.
    let mut store = TypeStore::with_minimal_jdk();
    let type_params_before = store.type_param_count();

    let math_stub = TypeDefStub {
        binary_name: "java.lang.Math".to_string(),
        access_flags: 0x0001, // ACC_PUBLIC
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![MethodStub {
            name: "max".to_string(),
            descriptor: "(II)I".to_string(),
            signature: None,
            access_flags: 0x0001 | 0x0008, // ACC_PUBLIC | ACC_STATIC
        }],
    };

    let collections_stub = TypeDefStub {
        binary_name: "java.util.Collections".to_string(),
        access_flags: 0x0001, // ACC_PUBLIC
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![MethodStub {
            name: "emptyList".to_string(),
            descriptor: "()Ljava/util/List;".to_string(),
            signature: None,
            access_flags: 0x0001 | 0x0008, // ACC_PUBLIC | ACC_STATIC
        }],
    };

    let mut provider = MapProvider::default();
    provider
        .stubs
        .insert("java.lang.Math".to_string(), math_stub);
    provider
        .stubs
        .insert("java.util.Collections".to_string(), collections_stub);

    let math_id = store
        .lookup_class("java.lang.Math")
        .expect("minimal JDK should define java.lang.Math");
    let collections_id = store
        .lookup_class("java.util.Collections")
        .expect("minimal JDK should define java.util.Collections");
    let list_id = store
        .lookup_class("java.util.List")
        .expect("minimal JDK should define java.util.List");

    {
        let mut loader = ExternalTypeLoader::new(&mut store, &provider);
        let ensured_math = loader
            .ensure_class("java.lang.Math")
            .expect("math should still be present");
        assert_eq!(ensured_math, math_id, "expected ensure_class to reuse Math ClassId");
        let ensured_collections = loader
            .ensure_class("java.util.Collections")
            .expect("collections should still be present");
        assert_eq!(
            ensured_collections, collections_id,
            "expected ensure_class to reuse Collections ClassId"
        );
    }
    assert_eq!(
        store.type_param_count(),
        type_params_before,
        "expected ensure_class to avoid allocating extra type params when skipping non-placeholder defs"
    );

    // Ensure `Math.max(float, float)` survived (built-in stub only provides int overload).
    let math_def = store.class(math_id).expect("math def should exist");
    assert!(
        math_def.methods.iter().any(|m| {
            m.name == "max"
                && m.params
                    == vec![
                        Type::Primitive(PrimitiveType::Float),
                        Type::Primitive(PrimitiveType::Float),
                    ]
                && m.return_type == Type::Primitive(PrimitiveType::Float)
        }),
        "expected minimal JDK overload Math.max(float,float) to remain after ensure_class; got {:?}",
        math_def.methods
    );

    // Ensure `Collections.emptyList` stayed generic (built-in stub erases signature).
    let collections_def = store
        .class(collections_id)
        .expect("collections def should exist");
    let empty = collections_def
        .methods
        .iter()
        .find(|m| m.name == "emptyList")
        .expect("minimal JDK should define Collections.emptyList");
    assert_eq!(
        empty.type_params.len(),
        1,
        "expected Collections.emptyList to remain generic"
    );
    let t = empty.type_params[0];
    assert_eq!(
        empty.return_type,
        Type::class(list_id, vec![Type::TypeVar(t)])
    );
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

#[test]
fn cycle_safe_loading() {
    let a_stub = TypeDefStub {
        binary_name: "com.example.A".to_string(),
        access_flags: 0x0000,
        super_binary_name: Some("com.example.B".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![],
    };
    let b_stub = TypeDefStub {
        binary_name: "com.example.B".to_string(),
        access_flags: 0x0000,
        super_binary_name: Some("com.example.A".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![],
    };

    let mut provider = MapProvider::default();
    provider.stubs.insert("com.example.A".to_string(), a_stub);
    provider.stubs.insert("com.example.B".to_string(), b_stub);

    let mut store = nova_types::TypeStore::default();
    let mut loader = ExternalTypeLoader::new(&mut store, &provider);

    let a_id = loader
        .ensure_class("com.example.A")
        .expect("class A should load");
    let b_id = store
        .lookup_class("com.example.B")
        .expect("class B should have been loaded recursively");

    let a_def = store.class(a_id).unwrap();
    let b_def = store.class(b_id).unwrap();

    assert_eq!(a_def.super_class, Some(Type::class(b_id, vec![])));
    assert_eq!(b_def.super_class, Some(Type::class(a_id, vec![])));
}

#[test]
fn parses_wildcard_type_arguments_in_field_signatures() {
    let list_stub = TypeDefStub {
        binary_name: "java.util.List".to_string(),
        access_flags: 0x0200, // ACC_INTERFACE
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<E:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![],
    };

    let outer_stub = TypeDefStub {
        binary_name: "com.example.Outer".to_string(),
        access_flags: 0x0000,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<T:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![FieldStub {
            name: "items".to_string(),
            descriptor: "Ljava/util/List;".to_string(),
            signature: Some("Ljava/util/List<+TT;>;".to_string()),
            access_flags: 0x0000,
        }],
        methods: vec![],
    };

    let mut provider = MapProvider::default();
    provider
        .stubs
        .insert("java.util.List".to_string(), list_stub);
    provider
        .stubs
        .insert("com.example.Outer".to_string(), outer_stub);

    let mut store = nova_types::TypeStore::default();
    let mut loader = ExternalTypeLoader::new(&mut store, &provider);

    let outer_id = loader
        .ensure_class("com.example.Outer")
        .expect("Outer should load");
    let list_id = store
        .lookup_class("java.util.List")
        .expect("List should have been loaded via field signature");

    let outer_def = store.class(outer_id).unwrap();
    let t = outer_def.type_params[0];
    let field = outer_def.fields.iter().find(|f| f.name == "items").unwrap();

    let expected = Type::class(
        list_id,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::TypeVar(t),
        )))],
    );
    assert_eq!(field.ty, expected);
}

#[test]
fn resolves_self_referential_method_type_param_bounds() {
    let comparable_stub = TypeDefStub {
        binary_name: "java.lang.Comparable".to_string(),
        access_flags: 0x0200, // ACC_INTERFACE
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: Some("<T:Ljava/lang/Object;>Ljava/lang/Object;".to_string()),
        fields: vec![],
        methods: vec![],
    };

    let util_stub = TypeDefStub {
        binary_name: "com.example.Util".to_string(),
        access_flags: 0x0000,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![MethodStub {
            name: "id".to_string(),
            descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
            signature: Some("<T:Ljava/lang/Comparable<TT;>;>(TT;)TT;".to_string()),
            access_flags: 0x0000,
        }],
    };

    let mut provider = MapProvider::default();
    provider
        .stubs
        .insert("java.lang.Comparable".to_string(), comparable_stub);
    provider
        .stubs
        .insert("com.example.Util".to_string(), util_stub);

    let mut store = nova_types::TypeStore::default();
    let mut loader = ExternalTypeLoader::new(&mut store, &provider);

    let util_id = loader
        .ensure_class("com.example.Util")
        .expect("Util should load");
    let comparable_id = store
        .lookup_class("java.lang.Comparable")
        .expect("Comparable should have been loaded via method type param bound");

    let util_def = store.class(util_id).unwrap();
    let method = util_def.methods.iter().find(|m| m.name == "id").unwrap();

    assert_eq!(method.type_params.len(), 1);
    let t = method.type_params[0];

    let tp = store
        .type_param(t)
        .expect("method type param should be defined");
    assert_eq!(
        tp.upper_bounds,
        vec![Type::class(comparable_id, vec![Type::TypeVar(t)])]
    );
    assert_eq!(method.params, vec![Type::TypeVar(t)]);
    assert_eq!(method.return_type, Type::TypeVar(t));
}

#[test]
fn ensure_class_does_not_overwrite_existing_non_placeholder_definition() {
    let foo_stub = TypeDefStub {
        binary_name: "com.example.Foo".to_string(),
        access_flags: 0x0000,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![MethodStub {
            name: "providerMethod".to_string(),
            descriptor: "()V".to_string(),
            signature: None,
            access_flags: 0x0000,
        }],
    };

    let mut provider = MapProvider::default();
    provider
        .stubs
        .insert("com.example.Foo".to_string(), foo_stub);

    let mut store = nova_types::TypeStore::default();
    let object_id = store.well_known().object;
    let foo_id = store.upsert_class(ClassDef {
        name: "com.example.Foo".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object_id, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![ConstructorDef {
            params: vec![],
            is_varargs: false,
            is_accessible: true,
        }],
        methods: vec![MethodDef {
            name: "workspaceMethod".to_string(),
            type_params: vec![],
            params: vec![],
            return_type: Type::Void,
            is_static: true,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    let mut loader = ExternalTypeLoader::new(&mut store, &provider);
    let ensured = loader
        .ensure_class("com.example.Foo")
        .expect("Foo should be present");
    assert_eq!(ensured, foo_id);

    let foo_def = store.class(foo_id).expect("Foo should stay defined");
    assert!(
        foo_def.methods.iter().any(|m| m.name == "workspaceMethod"),
        "expected the existing definition to remain intact"
    );
    assert!(
        foo_def.methods.iter().all(|m| m.name != "providerMethod"),
        "expected the provider stub to be ignored when a non-placeholder definition already exists"
    );
}

#[test]
fn ensure_class_does_not_overwrite_existing_supertype_during_recursive_load() {
    let foo_stub = TypeDefStub {
        binary_name: "com.example.Foo".to_string(),
        access_flags: 0x0000,
        super_binary_name: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![MethodStub {
            name: "providerMethod".to_string(),
            descriptor: "()V".to_string(),
            signature: None,
            access_flags: 0x0000,
        }],
    };

    let bar_stub = TypeDefStub {
        binary_name: "com.example.Bar".to_string(),
        access_flags: 0x0000,
        super_binary_name: Some("com.example.Foo".to_string()),
        interfaces: vec![],
        signature: None,
        fields: vec![],
        methods: vec![],
    };

    let mut provider = MapProvider::default();
    provider
        .stubs
        .insert("com.example.Foo".to_string(), foo_stub);
    provider
        .stubs
        .insert("com.example.Bar".to_string(), bar_stub);

    let mut store = nova_types::TypeStore::default();
    let object_id = store.well_known().object;
    let foo_id = store.upsert_class(ClassDef {
        name: "com.example.Foo".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object_id, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![ConstructorDef {
            params: vec![],
            is_varargs: false,
            is_accessible: true,
        }],
        methods: vec![MethodDef {
            name: "workspaceMethod".to_string(),
            type_params: vec![],
            params: vec![],
            return_type: Type::Void,
            is_static: true,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    let mut loader = ExternalTypeLoader::new(&mut store, &provider);
    let bar_id = loader
        .ensure_class("com.example.Bar")
        .expect("Bar should load");

    let bar_def = store.class(bar_id).expect("Bar def should exist");
    assert_eq!(
        bar_def.super_class,
        Some(Type::class(foo_id, vec![])),
        "expected Bar's super class to resolve to the existing Foo"
    );

    let foo_def = store.class(foo_id).expect("Foo should stay defined");
    assert!(
        foo_def.methods.iter().any(|m| m.name == "workspaceMethod"),
        "expected Foo to retain the existing definition even when referenced as Bar's supertype"
    );
    assert!(
        foo_def.methods.iter().all(|m| m.name != "providerMethod"),
        "expected recursive ensure_class(Foo) to avoid overwriting existing defs"
    );
}
