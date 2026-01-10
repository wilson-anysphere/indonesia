use nova_types::{
    is_assignable, is_subtype, resolve_method_call, ClassType, MethodCall, MethodResolution, Type, TypeEnv,
    TypeStore, WildcardBound,
};

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

#[test]
fn method_resolution_applies_capture_conversion_for_extends_wildcard() {
    let mut env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;

    let receiver = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(Type::class(
            string,
            vec![],
        ))))],
    );

    let call = MethodCall {
        receiver,
        name: "get",
        args: vec![Type::int()],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let MethodResolution::Found(resolved) = resolve_method_call(&mut env, &call) else {
        panic!("expected method resolution success");
    };

    // `List<? extends String>.get(int)` should return a capture variable `CAP#n` with upper bound `String`.
    let Type::TypeVar(cap) = resolved.return_type.clone() else {
        panic!("expected capture type var return, got {:?}", resolved.return_type);
    };
    let cap_data = env.type_param(cap).unwrap();
    assert_eq!(cap_data.upper_bounds, vec![Type::class(string, vec![])]);
    assert_eq!(cap_data.lower_bound, None);
    assert!(is_assignable(&env, &resolved.return_type, &Type::class(string, vec![])));
}

#[test]
fn method_resolution_applies_capture_conversion_for_super_wildcard() {
    let mut env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;
    let object = env.well_known().object;

    let receiver = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Super(Box::new(Type::class(
            string,
            vec![],
        ))))],
    );

    // `List<? super String>.add(String)` should be applicable (via capture conversion + lower bound).
    let call_ok = MethodCall {
        receiver: receiver.clone(),
        name: "add",
        args: vec![Type::class(string, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let MethodResolution::Found(resolved) = resolve_method_call(&mut env, &call_ok) else {
        panic!("expected method resolution success");
    };

    let Type::TypeVar(cap) = resolved.params[0].clone() else {
        panic!("expected capture type var param, got {:?}", resolved.params[0]);
    };
    let cap_data = env.type_param(cap).unwrap();
    assert_eq!(cap_data.lower_bound, Some(Type::class(string, vec![])));

    // But `List<? super String>.add(Object)` should not type-check.
    let call_bad = MethodCall {
        receiver,
        name: "add",
        args: vec![Type::class(object, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };
    assert!(matches!(
        resolve_method_call(&mut env, &call_bad),
        MethodResolution::NotFound | MethodResolution::Ambiguous(_)
    ));
}
