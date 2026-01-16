use nova_types::{
    lub, resolve_method_call, CallKind, ClassDef, ClassKind, MethodCall, MethodDef,
    MethodResolution, TyContext, Type, TypeEnv, TypeStore, WildcardBound,
};

use pretty_assertions::assert_eq;

#[test]
fn lub_string_integer_is_object() {
    let env = TypeStore::with_minimal_jdk();
    let string = Type::class(env.well_known().string, vec![]);
    let integer = Type::class(env.well_known().integer, vec![]);
    let object = Type::class(env.well_known().object, vec![]);

    assert_eq!(lub(&env, &string, &integer), object);
}

#[test]
fn lub_arraylist_string_list_string_is_list_string() {
    let env = TypeStore::with_minimal_jdk();
    let array_list = env.class_id("java.util.ArrayList").unwrap();
    let list = env.class_id("java.util.List").unwrap();
    let string = Type::class(env.well_known().string, vec![]);

    let array_list_string = Type::class(array_list, vec![string.clone()]);
    let list_string = Type::class(list, vec![string]);

    assert_eq!(lub(&env, &array_list_string, &list_string), list_string);
}

#[test]
fn lub_list_string_list_integer_is_list_unbounded_wildcard() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();

    let list_string = Type::class(list, vec![Type::class(env.well_known().string, vec![])]);
    let list_integer = Type::class(list, vec![Type::class(env.well_known().integer, vec![])]);

    // We represent `List<? extends Object>` as `List<?>`.
    let expected = Type::class(list, vec![Type::Wildcard(WildcardBound::Unbounded)]);
    assert_eq!(lub(&env, &list_string, &list_integer), expected);
}

#[test]
fn lub_string_array_integer_array_is_object_array() {
    let env = TypeStore::with_minimal_jdk();
    let string_array = Type::Array(Box::new(Type::class(env.well_known().string, vec![])));
    let integer_array = Type::Array(Box::new(Type::class(env.well_known().integer, vec![])));
    let object_array = Type::Array(Box::new(Type::class(env.well_known().object, vec![])));

    assert_eq!(lub(&env, &string_array, &integer_array), object_array);
}

#[test]
fn lub_equivalent_intersections_is_normalized_and_commutative() {
    let env = TypeStore::with_minimal_jdk();

    let cloneable = Type::class(env.well_known().cloneable, vec![]);
    let serializable = Type::class(env.well_known().serializable, vec![]);

    let a = Type::Intersection(vec![cloneable.clone(), serializable.clone()]);
    let b = Type::Intersection(vec![serializable.clone(), cloneable.clone()]);

    let expected = Type::Intersection(vec![serializable, cloneable]);

    assert_eq!(lub(&env, &a, &b), expected);
    assert_eq!(lub(&env, &b, &a), expected);
}

#[test]
fn lub_errorish_is_commutative() {
    let env = TypeStore::with_minimal_jdk();
    assert_eq!(lub(&env, &Type::Unknown, &Type::Error), Type::Error);
    assert_eq!(lub(&env, &Type::Error, &Type::Unknown), Type::Error);
}

#[test]
fn lub_is_order_independent_for_intersection_with_conflicting_generic_instances() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let number = env.class_id("java.lang.Number").unwrap();

    let list_integer = Type::class(list, vec![Type::class(env.well_known().integer, vec![])]);
    let list_string = Type::class(list, vec![Type::class(env.well_known().string, vec![])]);
    let list_double = Type::class(
        list,
        vec![Type::class(
            env.class_id("java.lang.Double").unwrap(),
            vec![],
        )],
    );

    // Two instantiations of the same generic type are not directly compatible; when they appear in
    // an intersection (usually during recovery), LUB should stay stable regardless of component
    // ordering and avoid “picking” a single instantiation.
    let i1 = Type::Intersection(vec![list_integer.clone(), list_string.clone()]);
    let i2 = Type::Intersection(vec![list_string, list_integer]);

    // `List<Integer>` and `List<String>` share `List<?>` as their best-effort merged instantiation,
    // so the LUB with `List<Double>` should not “accidentally” pick the `Integer` path and yield
    // `List<? extends Number>`.
    let expected = Type::class(list, vec![Type::Wildcard(WildcardBound::Unbounded)]);
    assert_eq!(lub(&env, &i1, &list_double), expected);
    assert_eq!(lub(&env, &i2, &list_double), expected);

    // Sanity: ensure we aren't producing the narrower (and order-dependent) result.
    let not_expected = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(number, vec![]),
        )))],
    );
    assert_ne!(expected, not_expected);
}

#[test]
fn inference_uses_lub_for_generic_instances() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let list = env.class_id("java.util.List").unwrap();

    let t = env.add_type_param("T", vec![Type::class(object, vec![])]);
    let util = env.add_class(ClassDef {
        name: "com.example.Util".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "pick".to_string(),
            type_params: vec![t],
            params: vec![Type::TypeVar(t), Type::TypeVar(t)],
            return_type: Type::TypeVar(t),
            is_static: true,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    let list_string = Type::class(list, vec![Type::class(env.well_known().string, vec![])]);
    let list_integer = Type::class(list, vec![Type::class(env.well_known().integer, vec![])]);
    let expected_t = Type::class(list, vec![Type::Wildcard(WildcardBound::Unbounded)]);

    let call = MethodCall {
        receiver: Type::class(util, vec![]),
        call_kind: CallKind::Static,
        name: "pick",
        args: vec![list_string, list_integer],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(res) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success");
    };

    assert_eq!(res.inferred_type_args, vec![expected_t.clone()]);
    assert_eq!(res.return_type, expected_t);
}
