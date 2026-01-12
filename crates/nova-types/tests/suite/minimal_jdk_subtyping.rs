use nova_types::{is_subtype, Type, TypeEnv, TypeStore};

#[test]
fn minimal_jdk_interfaces_are_subtypes_of_object() {
    let env = TypeStore::with_minimal_jdk();

    let object = Type::class(env.well_known().object, vec![]);

    let list = env
        .class_id("java.util.List")
        .expect("List must exist in minimal JDK");
    let string = env.well_known().string;
    let list_string = Type::class(list, vec![Type::class(string, vec![])]);
    assert!(is_subtype(&env, &list_string, &object));

    let cloneable = Type::class(env.well_known().cloneable, vec![]);
    assert!(is_subtype(&env, &cloneable, &object));
}

#[test]
fn intersection_subtyping_is_order_independent() {
    let env = TypeStore::with_minimal_jdk();

    let cloneable = Type::class(env.well_known().cloneable, vec![]);
    let serializable = Type::class(env.well_known().serializable, vec![]);

    let ab = Type::Intersection(vec![cloneable.clone(), serializable.clone()]);
    let ba = Type::Intersection(vec![serializable.clone(), cloneable.clone()]);

    // `A & B` should be equivalent to `B & A` for subtyping purposes.
    assert!(is_subtype(&env, &ab, &ba));
    assert!(is_subtype(&env, &ba, &ab));

    // And it should be a subtype of each component.
    assert!(is_subtype(&env, &ab, &cloneable));
    assert!(is_subtype(&env, &ab, &serializable));

    // But neither component alone is a subtype of the full intersection.
    assert!(!is_subtype(&env, &cloneable, &ab));
    assert!(!is_subtype(&env, &serializable, &ab));
}
