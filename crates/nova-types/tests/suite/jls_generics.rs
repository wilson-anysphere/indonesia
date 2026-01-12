use nova_types::{
    instantiate_supertype, is_assignable, is_subtype, resolve_method_call, CallKind, ClassDef,
    ClassKind, ClassType, FieldDef, MethodCall, MethodDef, MethodResolution, TyContext, Type,
    TypeEnv, TypeParamDef, TypeStore, WildcardBound,
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
fn instantiate_supertype_is_order_independent_for_type_var_and_intersection_bounds() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;
    let integer = env.well_known().integer;

    // interface I<X>
    let i_x = env.add_type_param("X", vec![Type::class(object, vec![])]);
    let iface = env.add_class(ClassDef {
        name: "com.example.I".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![i_x],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    // class A implements I<String>
    let a = env.add_class(ClassDef {
        name: "com.example.A".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![Type::class(iface, vec![Type::class(string, vec![])])],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    // class B implements I<Integer>
    let b = env.add_class(ClassDef {
        name: "com.example.B".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![Type::class(iface, vec![Type::class(integer, vec![])])],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    // Two type vars with identical bounds in opposite order.
    let t1 = env.add_type_param("T1", vec![Type::class(a, vec![]), Type::class(b, vec![])]);
    let t2 = env.add_type_param("T2", vec![Type::class(b, vec![]), Type::class(a, vec![])]);

    // A and B provide conflicting instantiations of `I` (String vs Integer), so viewing the type
    // variable as `I` is ambiguous. The result should still be deterministic across bound order.
    let args1 = instantiate_supertype(&env, &Type::TypeVar(t1), iface);
    let args2 = instantiate_supertype(&env, &Type::TypeVar(t2), iface);
    assert_eq!(args1, args2);
    assert!(args1.is_none());

    // And the same for raw intersection types in opposite order.
    let i1 = Type::Intersection(vec![Type::class(b, vec![]), Type::class(a, vec![])]);
    let i2 = Type::Intersection(vec![Type::class(a, vec![]), Type::class(b, vec![])]);
    let i_args1 = instantiate_supertype(&env, &i1, iface);
    let i_args2 = instantiate_supertype(&env, &i2, iface);
    assert_eq!(i_args1, i_args2);
    assert!(i_args1.is_none());
}

#[test]
fn capture_conversion_allocates_capture_vars() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let integer = env.well_known().integer;

    let list_extends_integer = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(integer, vec![]),
        )))],
    );

    let mut ctx = TyContext::new(&env);
    let captured = ctx.capture_conversion(&list_extends_integer);
    let Type::Class(ClassType { args, .. }) = captured else {
        panic!("expected captured class type");
    };
    assert_eq!(args.len(), 1);
    let Type::TypeVar(tv) = &args[0] else {
        panic!("expected captured type var");
    };

    let tv_data = ctx.type_param(*tv).unwrap();
    assert!(tv_data.name.starts_with("CAP#"));
    assert_eq!(tv_data.upper_bounds, vec![Type::class(integer, vec![])]);
    assert_eq!(tv_data.lower_bound, None);
}

#[test]
fn capture_conversion_substitutes_self_referential_bounds() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;

    // Model: `class EnumLike<E extends EnumLike<E>> {}`.
    //
    // We need to reserve the class id before defining the self-referential bound.
    let enum_like = env.intern_class_id("com.example.EnumLike");
    let e = env.add_type_param("E", vec![Type::class(object, vec![])]);
    env.define_type_param(
        e,
        TypeParamDef {
            name: "E".to_string(),
            upper_bounds: vec![Type::class(enum_like, vec![Type::TypeVar(e)])],
            lower_bound: None,
        },
    );
    env.define_class(
        enum_like,
        ClassDef {
            name: "com.example.EnumLike".to_string(),
            kind: ClassKind::Class,
            type_params: vec![e],
            super_class: Some(Type::class(object, vec![])),
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![],
        },
    );

    let receiver = Type::class(enum_like, vec![Type::Wildcard(WildcardBound::Unbounded)]);
    let mut ctx = TyContext::new(&env);
    let captured = ctx.capture_conversion(&receiver);
    let Type::Class(ClassType { args, .. }) = captured else {
        panic!("expected captured class type");
    };
    let Type::TypeVar(cap) = &args[0] else {
        panic!("expected capture var");
    };

    let cap_def = ctx.type_param(*cap).unwrap();
    assert_eq!(
        cap_def.upper_bounds,
        vec![Type::class(enum_like, vec![Type::TypeVar(*cap)])]
    );
    assert_eq!(cap_def.lower_bound, None);
}

#[test]
fn capture_conversion_sorts_capture_upper_bounds() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;

    let cloneable = Type::class(env.well_known().cloneable, vec![]);
    let serializable = Type::class(env.well_known().serializable, vec![]);

    let t1 = env.add_type_param("T1", vec![cloneable.clone(), serializable.clone()]);
    let t2 = env.add_type_param("T2", vec![serializable.clone(), cloneable.clone()]);

    let foo1 = env.add_class(ClassDef {
        name: "com.example.Foo1".to_string(),
        kind: ClassKind::Class,
        type_params: vec![t1],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });
    let foo2 = env.add_class(ClassDef {
        name: "com.example.Foo2".to_string(),
        kind: ClassKind::Class,
        type_params: vec![t2],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let mut ctx = TyContext::new(&env);
    let captured1 = ctx.capture_conversion(&Type::class(
        foo1,
        vec![Type::Wildcard(WildcardBound::Unbounded)],
    ));
    let captured2 = ctx.capture_conversion(&Type::class(
        foo2,
        vec![Type::Wildcard(WildcardBound::Unbounded)],
    ));

    let Type::Class(ClassType { args: args1, .. }) = captured1 else {
        panic!("expected captured class type");
    };
    let Type::TypeVar(cap1) = args1[0] else {
        panic!("expected capture type var");
    };
    let Type::Class(ClassType { args: args2, .. }) = captured2 else {
        panic!("expected captured class type");
    };
    let Type::TypeVar(cap2) = args2[0] else {
        panic!("expected capture type var");
    };

    // Capture upper bounds should be normalized deterministically regardless of the formal
    // type parameter's bound ordering.
    let expected = vec![serializable, cloneable];
    assert_eq!(ctx.type_param(cap1).unwrap().upper_bounds, expected);
    assert_eq!(ctx.type_param(cap2).unwrap().upper_bounds, expected);
}

#[test]
fn method_resolution_applies_capture_conversion_for_extends_wildcard() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;

    let receiver = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(string, vec![]),
        )))],
    );

    let call = MethodCall {
        receiver,
        call_kind: CallKind::Instance,
        name: "get",
        args: vec![Type::int()],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(resolved) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success");
    };

    // `List<? extends String>.get(int)` should return a capture variable `CAP#n` with upper bound `String`.
    let Type::TypeVar(cap) = resolved.return_type.clone() else {
        panic!(
            "expected capture type var return, got {:?}",
            resolved.return_type
        );
    };
    let cap_data = ctx.type_param(cap).unwrap();
    assert_eq!(cap_data.upper_bounds, vec![Type::class(string, vec![])]);
    assert_eq!(cap_data.lower_bound, None);
    assert!(is_assignable(
        &ctx,
        &resolved.return_type,
        &Type::class(string, vec![])
    ));
}

#[test]
fn method_resolution_applies_capture_conversion_for_super_wildcard() {
    let env = TypeStore::with_minimal_jdk();
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
        call_kind: CallKind::Instance,
        name: "add",
        args: vec![Type::class(string, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let mut ctx_ok = TyContext::new(&env);
    let MethodResolution::Found(resolved) = resolve_method_call(&mut ctx_ok, &call_ok) else {
        panic!("expected method resolution success");
    };

    let Type::TypeVar(cap) = resolved.params[0].clone() else {
        panic!(
            "expected capture type var param, got {:?}",
            resolved.params[0]
        );
    };
    let cap_data = ctx_ok.type_param(cap).unwrap();
    assert_eq!(cap_data.lower_bound, Some(Type::class(string, vec![])));

    // But `List<? super String>.add(Object)` should not type-check.
    let call_bad = MethodCall {
        receiver,
        call_kind: CallKind::Instance,
        name: "add",
        args: vec![Type::class(object, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let mut ctx_bad = TyContext::new(&env);
    assert!(matches!(
        resolve_method_call(&mut ctx_bad, &call_bad),
        MethodResolution::NotFound(_) | MethodResolution::Ambiguous(_)
    ));
}

#[test]
fn wildcard_type_argument_containment_extends() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;
    let object = env.well_known().object;

    let list_extends_string = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(string, vec![]),
        )))],
    );
    let list_extends_object = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(object, vec![]),
        )))],
    );

    assert!(is_subtype(&env, &list_extends_string, &list_extends_object));
}

#[test]
fn wildcard_type_argument_containment_super() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;
    let object = env.well_known().object;

    let list_super_object = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Super(Box::new(Type::class(
            object,
            vec![],
        ))))],
    );
    let list_super_string = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Super(Box::new(Type::class(
            string,
            vec![],
        ))))],
    );

    assert!(is_subtype(&env, &list_super_object, &list_super_string));
    assert!(!is_subtype(&env, &list_super_string, &list_super_object));
}

#[test]
fn generic_subtyping_remains_invariant_without_wildcards() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;
    let object = env.well_known().object;

    let list_string = Type::class(list, vec![Type::class(string, vec![])]);
    let list_object = Type::class(list, vec![Type::class(object, vec![])]);

    assert!(!is_subtype(&env, &list_string, &list_object));
}

#[test]
fn method_resolution_is_deterministic_across_invocations() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;

    let receiver = Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(string, vec![]),
        )))],
    );

    let call = MethodCall {
        receiver,
        call_kind: CallKind::Instance,
        name: "get",
        args: vec![Type::int()],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx1 = TyContext::new(&env);
    let MethodResolution::Found(res1) = resolve_method_call(&mut ctx1, &call) else {
        panic!("expected method resolution success");
    };

    let mut ctx2 = TyContext::new(&env);
    let MethodResolution::Found(res2) = resolve_method_call(&mut ctx2, &call) else {
        panic!("expected method resolution success");
    };

    assert_eq!(res1, res2);
}

#[test]
fn method_resolution_is_order_independent() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;
    let integer = env.well_known().integer;

    let call_string = MethodCall {
        receiver: Type::class(
            list,
            vec![Type::Wildcard(WildcardBound::Extends(Box::new(
                Type::class(string, vec![]),
            )))],
        ),
        call_kind: CallKind::Instance,
        name: "get",
        args: vec![Type::int()],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let call_integer = MethodCall {
        receiver: Type::class(
            list,
            vec![Type::Wildcard(WildcardBound::Extends(Box::new(
                Type::class(integer, vec![]),
            )))],
        ),
        call_kind: CallKind::Instance,
        name: "get",
        args: vec![Type::int()],
        expected_return: None,
        explicit_type_args: vec![],
    };

    // Resolve string-then-integer.
    let mut ctx_a1 = TyContext::new(&env);
    let MethodResolution::Found(res_a1) = resolve_method_call(&mut ctx_a1, &call_string) else {
        panic!("expected method resolution success");
    };
    let mut ctx_b1 = TyContext::new(&env);
    let MethodResolution::Found(res_b1) = resolve_method_call(&mut ctx_b1, &call_integer) else {
        panic!("expected method resolution success");
    };

    // Resolve integer-then-string.
    let mut ctx_b2 = TyContext::new(&env);
    let MethodResolution::Found(res_b2) = resolve_method_call(&mut ctx_b2, &call_integer) else {
        panic!("expected method resolution success");
    };
    let mut ctx_a2 = TyContext::new(&env);
    let MethodResolution::Found(res_a2) = resolve_method_call(&mut ctx_a2, &call_string) else {
        panic!("expected method resolution success");
    };

    assert_eq!(res_a1, res_a2);
    assert_eq!(res_b1, res_b2);
}

#[test]
fn method_resolution_prefers_class_bound_over_interface_bound_for_type_var_receiver() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    let iface = env.add_class(ClassDef {
        name: "com.example.I".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "foo".to_string(),
            type_params: vec![],
            params: vec![],
            return_type: Type::class(object, vec![]),
            is_static: false,
            is_varargs: false,
            is_abstract: true,
        }],
    });

    let class = env.add_class(ClassDef {
        name: "com.example.A".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![Type::class(iface, vec![])],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "foo".to_string(),
            type_params: vec![],
            params: vec![],
            return_type: Type::class(string, vec![]),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        }],
    });

    // Intentionally put the interface bound first (even though Java source syntax requires the
    // class bound first) to ensure receiver normalization is robust and deterministic.
    let tv = env.add_type_param(
        "T",
        vec![Type::class(iface, vec![]), Type::class(class, vec![])],
    );

    let call = MethodCall {
        receiver: Type::TypeVar(tv),
        call_kind: CallKind::Instance,
        name: "foo",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(res) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success");
    };

    assert_eq!(res.return_type, Type::class(string, vec![]));
}

#[test]
fn method_resolution_type_var_receiver_keeps_non_errorish_bounds_when_unknown_present() {
    let mut env = TypeStore::with_minimal_jdk();
    let string = env.well_known().string;

    let iface = env.add_class(ClassDef {
        name: "com.example.IUnknownBound".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "foo".to_string(),
            type_params: vec![],
            params: vec![],
            return_type: Type::class(string, vec![]),
            is_static: false,
            is_varargs: false,
            is_abstract: true,
        }],
    });

    // If receiver normalization prunes via `is_subtype` (where `Unknown` is treated as compatible
    // with everything), it's easy to accidentally collapse `Unknown & I` to `Unknown`, preventing
    // member lookup from using `I`.
    //
    // Ensure we keep the real bound regardless of ordering.
    let t1 = env.add_type_param("T1", vec![Type::Unknown, Type::class(iface, vec![])]);
    let t2 = env.add_type_param("T2", vec![Type::class(iface, vec![]), Type::Unknown]);

    for tv in [t1, t2] {
        let call = MethodCall {
            receiver: Type::TypeVar(tv),
            call_kind: CallKind::Instance,
            name: "foo",
            args: vec![],
            expected_return: None,
            explicit_type_args: vec![],
        };

        let mut ctx = TyContext::new(&env);
        let MethodResolution::Found(res) = resolve_method_call(&mut ctx, &call) else {
            panic!("expected method resolution success");
        };

        assert_eq!(res.return_type, Type::class(string, vec![]));
    }
}

#[test]
fn field_resolution_prefers_class_bound_over_interface_bound_for_type_var_receiver() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    let iface = env.add_class(ClassDef {
        name: "com.example.IFIeld".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![FieldDef {
            name: "foo".to_string(),
            ty: Type::class(object, vec![]),
            is_static: true,
            is_final: true,
        }],
        constructors: vec![],
        methods: vec![],
    });

    let class = env.add_class(ClassDef {
        name: "com.example.AField".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![Type::class(iface, vec![])],
        fields: vec![FieldDef {
            name: "foo".to_string(),
            ty: Type::class(string, vec![]),
            is_static: false,
            is_final: false,
        }],
        constructors: vec![],
        methods: vec![],
    });

    // Intentionally put the interface bound first.
    let tv = env.add_type_param(
        "T",
        vec![Type::class(iface, vec![]), Type::class(class, vec![])],
    );

    let receiver = Type::TypeVar(tv);

    let mut ctx = TyContext::new(&env);
    let field = ctx
        .resolve_field(&receiver, "foo", CallKind::Instance)
        .expect("field should resolve");

    assert_eq!(field.ty, Type::class(string, vec![]));
    assert!(!field.is_static);
}

#[test]
fn field_resolution_applies_capture_conversion_for_extends_wildcard() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let number = env.class_id("java.lang.Number").unwrap();

    let t = env.add_type_param("T", vec![Type::class(object, vec![])]);
    let boxed = env.add_class(ClassDef {
        name: "com.example.Box".to_string(),
        kind: ClassKind::Class,
        type_params: vec![t],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![FieldDef {
            name: "value".to_string(),
            ty: Type::TypeVar(t),
            is_static: false,
            is_final: false,
        }],
        constructors: vec![],
        methods: vec![],
    });

    let receiver = Type::class(
        boxed,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(number, vec![]),
        )))],
    );

    let mut ctx = TyContext::new(&env);
    let field = ctx
        .resolve_field(&receiver, "value", CallKind::Instance)
        .expect("field should resolve");

    let Type::TypeVar(cap) = field.ty.clone() else {
        panic!("expected captured type var, got {:?}", field.ty);
    };
    let cap_def = ctx.type_param(cap).unwrap();
    assert_eq!(cap_def.upper_bounds, vec![Type::class(number, vec![])]);
    assert_eq!(cap_def.lower_bound, None);

    // Reading is safe (capture <: Number), but writing is not (Number is not necessarily <: capture).
    assert!(is_assignable(&ctx, &field.ty, &Type::class(number, vec![])));
    assert!(!is_assignable(
        &ctx,
        &Type::class(number, vec![]),
        &field.ty
    ));
}

#[test]
fn field_resolution_applies_capture_conversion_for_super_wildcard() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let integer = env.well_known().integer;

    let t = env.add_type_param("T", vec![Type::class(object, vec![])]);
    let boxed = env.add_class(ClassDef {
        name: "com.example.Box2".to_string(),
        kind: ClassKind::Class,
        type_params: vec![t],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![FieldDef {
            name: "value".to_string(),
            ty: Type::TypeVar(t),
            is_static: false,
            is_final: false,
        }],
        constructors: vec![],
        methods: vec![],
    });

    let receiver = Type::class(
        boxed,
        vec![Type::Wildcard(WildcardBound::Super(Box::new(Type::class(
            integer,
            vec![],
        ))))],
    );

    let mut ctx = TyContext::new(&env);
    let field = ctx
        .resolve_field(&receiver, "value", CallKind::Instance)
        .expect("field should resolve");

    let Type::TypeVar(cap) = field.ty.clone() else {
        panic!("expected captured type var, got {:?}", field.ty);
    };
    let cap_def = ctx.type_param(cap).unwrap();
    assert_eq!(cap_def.upper_bounds, vec![Type::class(object, vec![])]);
    assert_eq!(cap_def.lower_bound, Some(Type::class(integer, vec![])));

    // Reading is only safe as Object, writing Integer is safe.
    assert!(is_assignable(&ctx, &field.ty, &Type::class(object, vec![])));
    assert!(!is_assignable(
        &ctx,
        &field.ty,
        &Type::class(integer, vec![])
    ));
    assert!(is_assignable(
        &ctx,
        &Type::class(integer, vec![]),
        &field.ty
    ));
}
