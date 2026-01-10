use nova_types::{is_subtype, ClassType, Type, TypeEnv, TypeStore, WildcardBound};

use pretty_assertions::assert_eq;

#[test]
fn inheritance_type_arg_substitution() {
    let env = TypeStore::with_minimal_jdk();

    let array_list = env.class_id("java.util.ArrayList").unwrap();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;
    let object = env.well_known().object;

    let array_list_string = Type::class(array_list, vec![Type::class(string, vec![])]);
    let list_string = Type::class(list, vec![Type::class(string, vec![])]);
    let list_object = Type::class(list, vec![Type::class(object, vec![])]);

    assert!(is_subtype(&env, &array_list_string, &list_string));
    assert!(!is_subtype(&env, &array_list_string, &list_object));
}

#[test]
fn capture_conversion_allocates_capture_vars() {
    let mut env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let integer = env.well_known().integer;

    let list_extends_integer = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(Type::class(integer, vec![]))))],
    );

    let captured = env.capture_conversion(&list_extends_integer);
    let Type::Class(ClassType { args, .. }) = captured else {
        panic!("expected captured class type");
    };
    assert_eq!(args.len(), 1);
    let Type::TypeVar(tv) = &args[0] else {
        panic!("expected captured type var");
    };

    let tv_data = env.type_param(*tv).unwrap();
    assert!(tv_data.name.starts_with("CAP#"));
    assert_eq!(tv_data.upper_bounds, vec![Type::class(integer, vec![])]);
    assert_eq!(tv_data.lower_bound, None);
}
