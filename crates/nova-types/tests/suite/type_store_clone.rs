use nova_types::{
    ClassDef, ClassKind, MethodDef, PrimitiveType, Type, TypeEnv, TypeStore, TypeVarId,
};

use pretty_assertions::assert_eq;

#[test]
fn type_store_clone_preserves_ids_and_is_independent() {
    let mut store = TypeStore::with_minimal_jdk();

    let object = store.well_known().object;

    let foo_def = ClassDef {
        name: "com.example.Foo".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
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

    // Removing a class in the original should not affect the clone.
    store.remove_class("com.example.Foo").expect("foo removed");
    assert_eq!(store.lookup_class("com.example.Foo"), None);
    assert_eq!(cloned.lookup_class("com.example.Foo"), Some(foo_id));
    assert_eq!(cloned.class(foo_id).unwrap().methods[0].name, "foo");
}

