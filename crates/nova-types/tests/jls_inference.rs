use nova_types::{
    resolve_method_call, ClassDef, ClassKind, MethodCall, MethodDef, MethodResolution, Type,
    TypeEnv, TypeStore,
};

use pretty_assertions::assert_eq;

#[test]
fn infer_simple_identity() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    let t = env.add_type_param("T", vec![Type::class(object, vec![])]);
    let test = env.add_class(ClassDef {
        name: "com.example.Test".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "id".to_string(),
            type_params: vec![t],
            params: vec![Type::TypeVar(t)],
            return_type: Type::TypeVar(t),
            is_static: true,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    let call = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: nova_types::CallKind::Static,
        name: "id",
        args: vec![Type::class(string, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let MethodResolution::Found(res) = resolve_method_call(&mut env, &call) else {
        panic!("expected method resolution success");
    };
    assert_eq!(res.inferred_type_args, vec![Type::class(string, vec![])]);
    assert_eq!(res.return_type, Type::class(string, vec![]));
}

#[test]
fn infer_from_return_context() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;
    let list = env.class_id("java.util.List").unwrap();

    let t = env.add_type_param("T", vec![Type::class(object, vec![])]);
    let test = env.add_class(ClassDef {
        name: "com.example.Test2".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "empty".to_string(),
            type_params: vec![t],
            params: vec![],
            return_type: Type::class(list, vec![Type::TypeVar(t)]),
            is_static: true,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    let expected = Type::class(list, vec![Type::class(string, vec![])]);
    let call = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: nova_types::CallKind::Static,
        name: "empty",
        args: vec![],
        expected_return: Some(expected.clone()),
        explicit_type_args: vec![],
    };

    let MethodResolution::Found(res) = resolve_method_call(&mut env, &call) else {
        panic!("expected method resolution success");
    };
    assert_eq!(res.inferred_type_args, vec![Type::class(string, vec![])]);
    assert_eq!(res.return_type, expected);
}

#[test]
fn inferred_type_respects_bounds() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let integer = env.well_known().integer;

    let n = env.add_type_param("N", vec![Type::class(integer, vec![])]);
    let test = env.add_class(ClassDef {
        name: "com.example.Test3".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "m".to_string(),
            type_params: vec![n],
            params: vec![Type::TypeVar(n)],
            return_type: Type::Void,
            is_static: true,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    let call = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: nova_types::CallKind::Static,
        name: "m",
        args: vec![Type::class(integer, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let MethodResolution::Found(res) = resolve_method_call(&mut env, &call) else {
        panic!("expected method resolution success");
    };
    assert_eq!(res.inferred_type_args, vec![Type::class(integer, vec![])]);
}
