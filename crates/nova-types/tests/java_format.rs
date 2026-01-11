use nova_types::{
    format_method_signature, format_resolved_method, format_type, resolve_method_call, CallKind,
    ClassDef, ClassKind, MethodCall, MethodDef, MethodResolution, MethodSearchPhase,
    ResolvedMethod, Type, TypeEnv, TypeStore, WildcardBound,
};

use pretty_assertions::assert_eq;

#[test]
fn formats_wildcard_generic_array() {
    let env = TypeStore::with_minimal_jdk();
    let list = env.class_id("java.util.List").unwrap();
    let string = env.well_known().string;

    let ty = Type::Array(Box::new(Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(string, vec![]),
        )))],
    )));

    assert_eq!(format_type(&env, &ty), "List<? extends String>[]");
}

#[test]
fn formats_intersection_types() {
    let mut env = TypeStore::with_minimal_jdk();
    let serializable = env.well_known().serializable;
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

    let ty = Type::Intersection(vec![
        Type::class(serializable, vec![]),
        Type::class(comparable, vec![]),
    ]);

    assert_eq!(format_type(&env, &ty), "Serializable & Comparable");
}

#[test]
fn formats_nested_class_names() {
    let mut env = TypeStore::with_minimal_jdk();
    let entry = env.add_class(ClassDef {
        name: "java.util.Map$Entry".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    assert_eq!(format_type(&env, &Type::class(entry, vec![])), "Map.Entry");
}

#[test]
fn formats_varargs_and_generic_methods() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;
    let serializable = env.well_known().serializable;
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

    let test_owner = env.add_class(ClassDef {
        name: "com.example.Test".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let t = env.add_type_param(
        "T",
        vec![
            Type::class(serializable, vec![]),
            Type::class(comparable, vec![]),
        ],
    );

    let generic = MethodDef {
        name: "max".to_string(),
        type_params: vec![t],
        params: vec![Type::TypeVar(t), Type::TypeVar(t)],
        return_type: Type::TypeVar(t),
        is_static: true,
        is_varargs: false,
        is_abstract: false,
    };

    assert_eq!(
        format_method_signature(&env, test_owner, &generic),
        "<T extends Serializable & Comparable> T max(T, T)"
    );

    let varargs = MethodDef {
        name: "join".to_string(),
        type_params: vec![],
        params: vec![Type::Array(Box::new(Type::class(string, vec![])))],
        return_type: Type::class(string, vec![]),
        is_static: true,
        is_varargs: true,
        is_abstract: false,
    };

    assert_eq!(
        format_method_signature(&env, test_owner, &varargs),
        "String join(String...)"
    );

    // Resolved method signatures are fully substituted (no type parameters).
    let resolved_generic = ResolvedMethod {
        owner: test_owner,
        name: "max".to_string(),
        params: vec![Type::class(string, vec![]), Type::class(string, vec![])],
        signature_params: vec![Type::class(string, vec![]), Type::class(string, vec![])],
        return_type: Type::class(string, vec![]),
        is_varargs: false,
        is_static: true,
        conversions: vec![],
        inferred_type_args: vec![Type::class(string, vec![])],
        warnings: vec![],
        used_varargs: false,
        phase: MethodSearchPhase::Strict,
    };

    assert_eq!(
        format_resolved_method(&env, &resolved_generic),
        "String max(String, String)"
    );
}

#[test]
fn resolved_method_collapses_varargs_patterns_for_display() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    let test_owner = env.add_class(ClassDef {
        name: "com.example.Varargs".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![MethodDef {
            name: "join".to_string(),
            type_params: vec![],
            params: vec![Type::Array(Box::new(Type::class(string, vec![])))],
            return_type: Type::class(string, vec![]),
            is_static: true,
            is_varargs: true,
            is_abstract: false,
        }],
    });

    let call = MethodCall {
        receiver: Type::class(test_owner, vec![]),
        call_kind: CallKind::Static,
        name: "join",
        args: vec![Type::class(string, vec![]), Type::class(string, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let MethodResolution::Found(resolved) = resolve_method_call(&mut env, &call) else {
        panic!("expected method resolution success");
    };
    assert!(
        resolved.used_varargs,
        "expected variable-arity varargs invocation"
    );
    assert_eq!(resolved.params.len(), 2);

    assert_eq!(
        format_resolved_method(&env, &resolved),
        "String join(String...)"
    );
}
