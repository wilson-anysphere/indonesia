use nova_types::{
    resolve_method_call, CallKind, ClassDef, ClassKind, MethodCall, MethodDef, MethodResolution,
    PrimitiveType, TyContext, Type, TypeEnv, TypeStore, TypeWarning,
};

use pretty_assertions::assert_eq;

#[test]
fn static_vs_instance_call_kind_filtering_and_warning() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;

    let test = env.add_class(ClassDef {
        name: "com.example.CallKinds".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![
            // Instance overload: m(int)
            MethodDef {
                name: "m".to_string(),
                type_params: vec![],
                params: vec![Type::Primitive(PrimitiveType::Int)],
                return_type: Type::Void,
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            },
            // Static overload: m(long)
            MethodDef {
                name: "m".to_string(),
                type_params: vec![],
                params: vec![Type::Primitive(PrimitiveType::Long)],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
        ],
    });

    // `CallKinds.m(1)` should ignore the instance overload and pick `m(long)`.
    let call_static = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m",
        args: vec![Type::Primitive(PrimitiveType::Int)],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call_static) else {
        panic!("expected method resolution success");
    };
    assert!(found.is_static);
    assert_eq!(found.params, vec![Type::Primitive(PrimitiveType::Long)]);

    // `new CallKinds().m(1)` should pick the instance overload.
    let call_instance = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Instance,
        name: "m",
        args: vec![Type::Primitive(PrimitiveType::Int)],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call_instance) else {
        panic!("expected method resolution success");
    };
    assert!(!found.is_static);
    assert_eq!(found.params, vec![Type::Primitive(PrimitiveType::Int)]);

    // Best-effort: allow `recv.m(1L)` to resolve to the static method, but surface a warning.
    let call_static_via_instance = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Instance,
        name: "m",
        args: vec![Type::Primitive(PrimitiveType::Long)],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call_static_via_instance)
    else {
        panic!("expected method resolution success");
    };
    assert!(found.is_static);
    assert!(found
        .warnings
        .contains(&TypeWarning::StaticAccessViaInstance));
}

#[test]
fn overriding_removes_obvious_duplicates() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;

    let base = env.add_class(ClassDef {
        name: "com.example.Base".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "m".to_string(),
            type_params: vec![],
            params: vec![Type::Primitive(PrimitiveType::Int)],
            return_type: Type::Void,
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    let sub = env.add_class(ClassDef {
        name: "com.example.Sub".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(base, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "m".to_string(),
            type_params: vec![],
            params: vec![Type::Primitive(PrimitiveType::Int)],
            return_type: Type::Void,
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    let call = MethodCall {
        receiver: Type::class(sub, vec![]),
        call_kind: CallKind::Instance,
        name: "m",
        args: vec![Type::Primitive(PrimitiveType::Int)],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success");
    };
    assert_eq!(found.owner, sub);
}

#[test]
fn tie_breaks_on_conversion_cost() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let integer = env.well_known().integer;
    let long_wrapper = env.class_id("java.lang.Long").expect("Long should exist");

    let test = env.add_class(ClassDef {
        name: "com.example.Costs".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![
            MethodDef {
                name: "m".to_string(),
                type_params: vec![],
                params: vec![Type::class(integer, vec![])],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "m".to_string(),
                type_params: vec![],
                params: vec![Type::class(long_wrapper, vec![])],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "m".to_string(),
                type_params: vec![],
                params: vec![Type::class(object, vec![])],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
        ],
    });

    let call = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m",
        args: vec![Type::Primitive(PrimitiveType::Int)],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success");
    };
    assert_eq!(found.params, vec![Type::class(integer, vec![])]);
}

#[test]
fn not_found_includes_useful_diagnostics() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    let test = env.add_class(ClassDef {
        name: "com.example.Diagnostics".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "m".to_string(),
            type_params: vec![],
            params: vec![
                Type::Primitive(PrimitiveType::Int),
                Type::Primitive(PrimitiveType::Int),
            ],
            return_type: Type::Void,
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    // Wrong arity should be reported.
    let wrong_arity = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Instance,
        name: "m",
        args: vec![Type::Primitive(PrimitiveType::Int)],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let mut ctx = TyContext::new(&env);
    let MethodResolution::NotFound(nf) = resolve_method_call(&mut ctx, &wrong_arity) else {
        panic!("expected method resolution failure");
    };
    assert_eq!(nf.candidates.len(), 1);
    assert!(nf.candidates[0].failures.iter().any(|f| matches!(
        f.reason,
        nova_types::MethodCandidateFailureReason::WrongArity {
            expected: 2,
            found: 1,
            ..
        }
    )));

    // Conversion failure should mention the argument index and the types involved.
    let conv_fail = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Instance,
        name: "m",
        args: vec![Type::class(string, vec![]), Type::class(string, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let mut ctx = TyContext::new(&env);
    let MethodResolution::NotFound(nf) = resolve_method_call(&mut ctx, &conv_fail) else {
        panic!("expected method resolution failure");
    };
    assert!(nf.candidates[0].failures.iter().any(|f| matches!(
        &f.reason,
        nova_types::MethodCandidateFailureReason::ArgumentConversion { arg_index: 0, .. }
    )));
}

#[test]
fn not_found_reports_inference_bound_failures() {
    let mut env = TypeStore::with_minimal_jdk();

    let object = env.well_known().object;
    let number = env
        .class_id("java.lang.Number")
        .expect("Number should exist");
    let string = env.well_known().string;

    let n = env.add_type_param("N", vec![Type::class(number, vec![])]);
    let util = env.add_class(ClassDef {
        name: "com.example.InferenceBounds".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "id".to_string(),
            type_params: vec![n],
            params: vec![Type::TypeVar(n)],
            return_type: Type::TypeVar(n),
            is_static: true,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    // Explicit type arguments must satisfy bounds: `<N extends Number> id(N)`.
    let call = MethodCall {
        receiver: Type::class(util, vec![]),
        call_kind: CallKind::Static,
        name: "id",
        args: vec![Type::class(string, vec![])],
        expected_return: None,
        explicit_type_args: vec![Type::class(string, vec![])],
    };

    let mut ctx = TyContext::new(&env);
    let MethodResolution::NotFound(nf) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution failure");
    };
    assert!(nf.candidates[0].failures.iter().any(|f| matches!(
        &f.reason,
        nova_types::MethodCandidateFailureReason::TypeArgOutOfBounds { type_param, .. } if *type_param == n
    )));
}
