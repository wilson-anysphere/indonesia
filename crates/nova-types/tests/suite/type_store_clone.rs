use nova_types::{
    ClassDef, ClassKind, MethodDef, PrimitiveType, Type, TypeEnv, TypeStore, TypeVarId,
};

use pretty_assertions::assert_eq;

#[test]
fn type_store_clone_preserves_ids_and_is_independent() {
    let mut store = TypeStore::with_minimal_jdk();

    let object = store.well_known().object;

    // Create a project/local type parameter and ensure cloning preserves its `TypeVarId`.
    let local_tp = store.add_type_param("T", vec![Type::class(object, vec![])]);

    let foo_def = ClassDef {
        name: "com.example.Foo".to_string(),
        kind: ClassKind::Class,
        type_params: vec![local_tp],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "foo".to_string(),
            type_params: vec![],
            params: vec![Type::Primitive(PrimitiveType::Int)],
            return_type: Type::Void,
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        }],
    };
    let foo_id = store.upsert_class(foo_def.clone());

    let bar_def = ClassDef {
        name: "com.example.Bar".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "bar".to_string(),
            type_params: vec![],
            params: vec![],
            return_type: Type::Primitive(PrimitiveType::Boolean),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        }],
    };
    let bar_id = store.upsert_class(bar_def.clone());

    // Create a tombstone so we can validate that tombstones survive cloning.
    let removed_bar_id = store.remove_class("com.example.Bar").expect("bar removed");
    assert_eq!(removed_bar_id, bar_id);
    assert_eq!(store.lookup_class("com.example.Bar"), None);

    let original_type_param_count = store.type_param_count();
    let mut cloned = store.clone();

    // `well_known` ids should be preserved.
    assert_eq!(store.well_known().object, cloned.well_known().object);
    assert_eq!(store.well_known().string, cloned.well_known().string);
    assert_eq!(store.well_known().integer, cloned.well_known().integer);
    assert_eq!(store.well_known().cloneable, cloned.well_known().cloneable);
    assert_eq!(
        store.well_known().serializable,
        cloned.well_known().serializable
    );

    // `well_known` ids should still point at the expected class definitions in both stores.
    let wk = store.well_known().clone();
    for (id, expected_name) in [
        (wk.object, "java.lang.Object"),
        (wk.string, "java.lang.String"),
        (wk.integer, "java.lang.Integer"),
        (wk.cloneable, "java.lang.Cloneable"),
        (wk.serializable, "java.io.Serializable"),
    ] {
        assert_eq!(
            store.class(id).expect("well-known class missing").name,
            expected_name
        );
        assert_eq!(
            cloned.class(id).expect("well-known class missing in clone").name,
            expected_name
        );
    }

    // `type_params` should be preserved (these are populated by `with_minimal_jdk`).
    assert_eq!(store.type_param_count(), cloned.type_param_count());
    for idx in 0..store.type_param_count() {
        let id = TypeVarId(idx as u32);
        let orig = store.type_param(id).expect("type param should exist");
        let cloned_tp = cloned.type_param(id).expect("type param should exist in clone");
        assert_eq!(orig.name, cloned_tp.name);
        assert_eq!(orig.upper_bounds, cloned_tp.upper_bounds);
        assert_eq!(orig.lower_bound, cloned_tp.lower_bound);
    }

    // The name -> id map should match across clones for present classes.
    let names = [
        "java.lang.Object",
        "java.lang.String",
        "java.lang.Integer",
        "java.util.List",
        "com.example.Foo",
    ];
    for name in names {
        let orig_id = store.lookup_class(name).expect("class should exist");
        let cloned_id = cloned.lookup_class(name).expect("class should exist in clone");
        assert_eq!(orig_id, cloned_id);

        // And the class definitions should match across clones.
        assert_eq!(
            store.class(orig_id).unwrap().name,
            cloned.class(cloned_id).unwrap().name
        );
    }

    // The tombstone should also be preserved at clone time.
    assert_eq!(store.lookup_class("com.example.Bar"), None);
    assert_eq!(cloned.lookup_class("com.example.Bar"), None);
    let reinserted_bar_id = cloned.upsert_class(bar_def.clone());
    assert_eq!(reinserted_bar_id, removed_bar_id);
    assert_eq!(cloned.lookup_class("com.example.Bar"), Some(removed_bar_id));
    assert_eq!(
        cloned.class(reinserted_bar_id).unwrap().methods[0].name,
        "bar"
    );
    // Re-inserting in the clone should not affect the original.
    assert_eq!(store.lookup_class("com.example.Bar"), None);

    // Adding a type parameter in the clone should not affect the original.
    let added_to_clone = cloned.add_type_param("U", vec![Type::class(object, vec![])]);
    assert_eq!(cloned.type_param_count(), original_type_param_count + 1);
    assert_eq!(store.type_param_count(), original_type_param_count);
    assert!(store.type_param(added_to_clone).is_none());
    assert_eq!(cloned.type_param(added_to_clone).unwrap().name, "U");

    // Removing a class in the clone should not affect the original.
    let removed_foo_id = cloned.remove_class("com.example.Foo").expect("foo removed in clone");
    assert_eq!(removed_foo_id, foo_id);
    assert_eq!(cloned.lookup_class("com.example.Foo"), None);
    assert_eq!(store.lookup_class("com.example.Foo"), Some(foo_id));
    assert_eq!(store.class(foo_id).unwrap().methods[0].name, "foo");
}

#[test]
fn default_type_store_can_be_cloned_and_mutated_independently() {
    let store = TypeStore::default();
    let mut cloned = store.clone();

    // `well_known` ids should resolve to the expected class names in both stores.
    let wk = store.well_known().clone();
    for (id, expected_name) in [
        (wk.object, "java.lang.Object"),
        (wk.string, "java.lang.String"),
        (wk.integer, "java.lang.Integer"),
        (wk.cloneable, "java.lang.Cloneable"),
        (wk.serializable, "java.io.Serializable"),
    ] {
        assert_eq!(
            store.class(id).expect("well-known class missing").name,
            expected_name
        );
        assert_eq!(
            cloned.class(id).expect("well-known class missing in clone").name,
            expected_name
        );
    }

    // Mutating the clone should not mutate the original store.
    let local_tp = cloned.add_type_param("T", vec![Type::class(wk.object, vec![])]);
    let foo_id = cloned.upsert_class(ClassDef {
        name: "com.example.Foo".to_string(),
        kind: ClassKind::Class,
        type_params: vec![local_tp],
        super_class: Some(Type::class(wk.object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });
    assert_eq!(cloned.lookup_class("com.example.Foo"), Some(foo_id));
    assert_eq!(store.lookup_class("com.example.Foo"), None);
    assert!(store.type_param(local_tp).is_none());

    // Mutating a shared core class in the clone should not affect the original.
    cloned
        .remove_class("java.lang.String")
        .expect("String should exist in default TypeStore");
    assert_eq!(cloned.lookup_class("java.lang.String"), None);
    assert_eq!(store.lookup_class("java.lang.String"), Some(wk.string));
}
