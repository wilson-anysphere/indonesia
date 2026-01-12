use nova_types::{is_subtype, PrimitiveType, Type, TypeEnv, TypeStore};

#[test]
fn default_type_store_supports_well_known_subtyping_queries() {
    let env = TypeStore::default();

    // Ensure implicit `java.lang.*` lookup works for core types.
    let object = env
        .lookup_class("Object")
        .expect("TypeStore::default should define java.lang.Object");
    let cloneable = env
        .lookup_class("Cloneable")
        .expect("TypeStore::default should define java.lang.Cloneable");
    let serializable = env
        .lookup_class("java.io.Serializable")
        .expect("TypeStore::default should define java.io.Serializable");

    // `int[] <: Object | Cloneable | Serializable` requires `env.well_known()`.
    let int_array = Type::Array(Box::new(Type::Primitive(PrimitiveType::Int)));
    assert!(is_subtype(&env, &int_array, &Type::class(object, vec![])));
    assert!(is_subtype(
        &env,
        &int_array,
        &Type::class(cloneable, vec![])
    ));
    assert!(is_subtype(
        &env,
        &int_array,
        &Type::class(serializable, vec![])
    ));
}
