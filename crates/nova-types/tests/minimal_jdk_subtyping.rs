use nova_types::{is_subtype, Type, TypeEnv, TypeStore};

#[test]
fn minimal_jdk_interfaces_are_subtypes_of_object() {
    let env = TypeStore::with_minimal_jdk();

    let object = Type::class(env.well_known().object, vec![]);

    let list = env.class_id("java.util.List").expect("List must exist in minimal JDK");
    let string = env.well_known().string;
    let list_string = Type::class(list, vec![Type::class(string, vec![])]);
    assert!(is_subtype(&env, &list_string, &object));

    let cloneable = Type::class(env.well_known().cloneable, vec![]);
    assert!(is_subtype(&env, &cloneable, &object));
}

