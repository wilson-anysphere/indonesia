use nova_types::{
    resolve_method_call, CallKind, ClassDef, ClassKind, MethodCall, MethodDef, MethodResolution,
    TyContext, Type, TypeEnv, TypeStore,
};

use pretty_assertions::assert_eq;

#[test]
fn infer_upper_bound_intersection_is_order_independent() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;

    let cloneable = Type::class(env.well_known().cloneable, vec![]);
    let serializable = Type::class(env.well_known().serializable, vec![]);

    // Same bounds, but listed in opposite order.
    let t1 = env.add_type_param("T1", vec![cloneable.clone(), serializable.clone()]);
    let t2 = env.add_type_param("T2", vec![serializable.clone(), cloneable.clone()]);

    let test = env.add_class(ClassDef {
        name: "com.example.GlbDeterminism".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![
            MethodDef {
                name: "m1".to_string(),
                type_params: vec![t1],
                params: vec![],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "m2".to_string(),
                type_params: vec![t2],
                params: vec![],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
        ],
    });

    let call1 = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m1",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let call2 = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m2",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx1 = TyContext::new(&env);
    let MethodResolution::Found(res1) = resolve_method_call(&mut ctx1, &call1) else {
        panic!("expected method resolution success for m1");
    };

    let mut ctx2 = TyContext::new(&env);
    let MethodResolution::Found(res2) = resolve_method_call(&mut ctx2, &call2) else {
        panic!("expected method resolution success for m2");
    };

    // The inferred type argument should not depend on the order of upper bounds.
    assert_eq!(res1.inferred_type_args, res2.inferred_type_args);

    // And it should be normalized deterministically (sorted by `type_sort_key`).
    let expected = Type::Intersection(vec![serializable, cloneable]);
    assert_eq!(res1.inferred_type_args, vec![expected]);
}

#[test]
fn infer_upper_bound_intersection_is_order_independent_with_errorish_bounds() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;

    let cloneable = Type::class(env.well_known().cloneable, vec![]);

    // Same bounds, but listed in opposite order. The presence of an "errorish" bound like
    // `Unknown` previously made GLB order-dependent due to the recovery behavior of `is_subtype`.
    let t1 = env.add_type_param("T1", vec![Type::Unknown, cloneable.clone()]);
    let t2 = env.add_type_param("T2", vec![cloneable.clone(), Type::Unknown]);

    let test = env.add_class(ClassDef {
        name: "com.example.GlbDeterminismErrorish".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![
            MethodDef {
                name: "m1".to_string(),
                type_params: vec![t1],
                params: vec![],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "m2".to_string(),
                type_params: vec![t2],
                params: vec![],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
        ],
    });

    let call1 = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m1",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let call2 = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m2",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx1 = TyContext::new(&env);
    let MethodResolution::Found(res1) = resolve_method_call(&mut ctx1, &call1) else {
        panic!("expected method resolution success for m1");
    };

    let mut ctx2 = TyContext::new(&env);
    let MethodResolution::Found(res2) = resolve_method_call(&mut ctx2, &call2) else {
        panic!("expected method resolution success for m2");
    };

    assert_eq!(res1.inferred_type_args, res2.inferred_type_args);
    assert_eq!(res1.inferred_type_args, vec![Type::Unknown]);
}

#[test]
fn infer_upper_bound_intersection_prunes_redundant_supertypes() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;

    let i = env.add_class(ClassDef {
        name: "com.example.I".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });
    let a = env.add_class(ClassDef {
        name: "com.example.A".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![Type::class(i, vec![])],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let i_ty = Type::class(i, vec![]);
    let a_ty = Type::class(a, vec![]);
    let serializable = Type::class(env.well_known().serializable, vec![]);

    // Same bounds but in different orders. `A` is a subtype of `I`, so `I` should be
    // pruned from the inferred intersection regardless of order.
    //
    // The first ordering triggers the classic order-dependence when GLB is reduced
    // left-to-right:
    //   glb(I, Serializable) => I & Serializable
    //   glb(I & Serializable, A) => A & I & Serializable  (I is redundant)
    let t1 = env.add_type_param("T1", vec![i_ty.clone(), serializable.clone(), a_ty.clone()]);
    let t2 = env.add_type_param("T2", vec![a_ty.clone(), i_ty, serializable.clone()]);

    let test = env.add_class(ClassDef {
        name: "com.example.GlbDeterminismRedundant".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![
            MethodDef {
                name: "m1".to_string(),
                type_params: vec![t1],
                params: vec![],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "m2".to_string(),
                type_params: vec![t2],
                params: vec![],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
        ],
    });

    let call1 = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m1",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let call2 = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m2",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx1 = TyContext::new(&env);
    let MethodResolution::Found(res1) = resolve_method_call(&mut ctx1, &call1) else {
        panic!("expected method resolution success for m1");
    };

    let mut ctx2 = TyContext::new(&env);
    let MethodResolution::Found(res2) = resolve_method_call(&mut ctx2, &call2) else {
        panic!("expected method resolution success for m2");
    };

    assert_eq!(res1.inferred_type_args, res2.inferred_type_args);
    assert_eq!(
        res1.inferred_type_args,
        vec![Type::Intersection(vec![a_ty, serializable])]
    );
}

#[test]
fn infer_upper_bound_intersection_normalizes_equivalent_intersection_bounds() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;

    let cloneable = Type::class(env.well_known().cloneable, vec![]);
    let serializable = Type::class(env.well_known().serializable, vec![]);

    let comparable = env.add_class(ClassDef {
        name: "java.lang.Comparable".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });
    let comparable = Type::class(comparable, vec![]);

    // Equivalent intersections but in different, non-canonical orders.
    let i1 = Type::Intersection(vec![cloneable.clone(), serializable.clone(), comparable.clone()]);
    let i2 = Type::Intersection(vec![comparable.clone(), serializable.clone(), cloneable.clone()]);

    // Same bounds, but listed in opposite order.
    let t1 = env.add_type_param("T1", vec![i1.clone(), i2.clone()]);
    let t2 = env.add_type_param("T2", vec![i2, i1]);

    let test = env.add_class(ClassDef {
        name: "com.example.GlbDeterminismEquivalentIntersections".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![
            MethodDef {
                name: "m1".to_string(),
                type_params: vec![t1],
                params: vec![],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "m2".to_string(),
                type_params: vec![t2],
                params: vec![],
                return_type: Type::Void,
                is_static: true,
                is_varargs: false,
                is_abstract: false,
            },
        ],
    });

    let call1 = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m1",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let call2 = MethodCall {
        receiver: Type::class(test, vec![]),
        call_kind: CallKind::Static,
        name: "m2",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx1 = TyContext::new(&env);
    let MethodResolution::Found(res1) = resolve_method_call(&mut ctx1, &call1) else {
        panic!("expected method resolution success for m1");
    };

    let mut ctx2 = TyContext::new(&env);
    let MethodResolution::Found(res2) = resolve_method_call(&mut ctx2, &call2) else {
        panic!("expected method resolution success for m2");
    };

    assert_eq!(res1.inferred_type_args, res2.inferred_type_args);

    // Fully normalized: sorted by `type_sort_key`.
    let expected = Type::Intersection(vec![serializable, cloneable, comparable]);
    assert_eq!(res1.inferred_type_args, vec![expected]);
}
