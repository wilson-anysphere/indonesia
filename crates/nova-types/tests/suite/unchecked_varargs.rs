use nova_types::{
    resolve_method_call, CallKind, ClassDef, ClassKind, MethodCall, MethodDef, MethodResolution,
    TyContext, Type, TypeEnv, TypeStore, TypeWarning, UncheckedReason,
};

#[test]
fn warns_for_non_reifiable_varargs_parameter_in_variable_arity_form() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    // `<T> void m(T... xs)`
    let t = env.add_type_param("T", vec![]);
    let util = env.add_class(ClassDef {
        name: "com.example.UncheckedVarargs".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![
            MethodDef {
                name: "m".to_string(),
                type_params: vec![t],
                params: vec![Type::Array(Box::new(Type::TypeVar(t)))],
                return_type: Type::Void,
                is_static: true,
                is_varargs: true,
                is_abstract: false,
            },
            // `void n(String... xs)`
            MethodDef {
                name: "n".to_string(),
                type_params: vec![],
                params: vec![Type::Array(Box::new(Type::class(string, vec![])))],
                return_type: Type::Void,
                is_static: true,
                is_varargs: true,
                is_abstract: false,
            },
        ],
    });

    // Variable-arity call (`m("a", "b")`).
    let call = MethodCall {
        receiver: Type::class(util, vec![]),
        call_kind: CallKind::Static,
        name: "m",
        args: vec![Type::class(string, vec![]), Type::class(string, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success");
    };

    assert!(
        found.used_varargs,
        "expected variable-arity varargs invocation"
    );
    assert!(found
        .warnings
        .contains(&TypeWarning::Unchecked(UncheckedReason::UncheckedVarargs)));
}

#[test]
fn no_warning_for_reifiable_varargs_parameter_in_variable_arity_form() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    // `void n(String... xs)`
    let util = env.add_class(ClassDef {
        name: "com.example.ReifiableVarargs".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "n".to_string(),
            type_params: vec![],
            params: vec![Type::Array(Box::new(Type::class(string, vec![])))],
            return_type: Type::Void,
            is_static: true,
            is_varargs: true,
            is_abstract: false,
        }],
    });

    let call = MethodCall {
        receiver: Type::class(util, vec![]),
        call_kind: CallKind::Static,
        name: "n",
        args: vec![Type::class(string, vec![]), Type::class(string, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success");
    };

    assert!(
        found.used_varargs,
        "expected variable-arity varargs invocation"
    );
    assert!(
        !found
            .warnings
            .contains(&TypeWarning::Unchecked(UncheckedReason::UncheckedVarargs)),
        "expected no unchecked-varargs warning for reifiable `String[]` parameter"
    );
}
