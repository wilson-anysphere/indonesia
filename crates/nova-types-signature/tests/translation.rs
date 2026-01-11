use nova_classfile::{
    parse_class_signature, parse_field_signature, parse_method_descriptor, parse_method_signature,
};
use nova_types::{ClassDef, ClassKind, PrimitiveType, Type, TypeEnv, TypeStore, WildcardBound};
use nova_types_signature::{
    class_sig_from_classfile, method_sig_from_classfile, ty_from_field_sig, TypeVarScope,
};
use pretty_assertions::assert_eq;

#[test]
fn self_referential_bound_allocates_type_var_ids_before_bounds() {
    let mut store = TypeStore::with_minimal_jdk();
    let object = store.class_id("java.lang.Object").unwrap();

    // java.lang.Comparable<T>
    let comparable_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
    let comparable = store.add_class(ClassDef {
        name: "java.lang.Comparable".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![comparable_t],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let sig = parse_class_signature(
        "<T:Ljava/lang/Object;:Ljava/lang/Comparable<TT;>;>Ljava/lang/Object;",
    )
    .unwrap();

    let (type_params, _super_class, _interfaces) =
        class_sig_from_classfile(&mut store, &TypeVarScope::new(), &sig);
    assert_eq!(type_params.len(), 1);
    let t = type_params[0];

    let tp = store.type_param(t).unwrap();
    assert_eq!(
        tp.upper_bounds,
        vec![
            Type::class(object, vec![]),
            Type::class(comparable, vec![Type::TypeVar(t)]),
        ]
    );
}

#[test]
fn interface_only_bounds_do_not_get_implicit_object() {
    let mut store = TypeStore::with_minimal_jdk();
    let serializable = store.class_id("java.io.Serializable").unwrap();

    let sig = parse_class_signature("<T::Ljava/io/Serializable;>Ljava/lang/Object;").unwrap();

    let (type_params, _super_class, _interfaces) =
        class_sig_from_classfile(&mut store, &TypeVarScope::new(), &sig);
    let t = type_params[0];

    let tp = store.type_param(t).unwrap();
    assert_eq!(tp.upper_bounds, vec![Type::class(serializable, vec![])]);
}

#[test]
fn wildcards_translate() {
    let store = TypeStore::with_minimal_jdk();
    let list = store.class_id("java.util.List").unwrap();
    let number = store.class_id("java.lang.Number").unwrap();

    let scope = TypeVarScope::new();

    let sig = parse_field_signature("Ljava/util/List<*>;").unwrap();
    assert_eq!(
        ty_from_field_sig(&store, &scope, &sig),
        Type::class(list, vec![Type::Wildcard(WildcardBound::Unbounded)])
    );

    let sig = parse_field_signature("Ljava/util/List<+Ljava/lang/Number;>;").unwrap();
    assert_eq!(
        ty_from_field_sig(&store, &scope, &sig),
        Type::class(
            list,
            vec![Type::Wildcard(WildcardBound::Extends(Box::new(
                Type::class(number, vec![])
            )))]
        )
    );

    let sig = parse_field_signature("Ljava/util/List<-Ljava/lang/Number;>;").unwrap();
    assert_eq!(
        ty_from_field_sig(&store, &scope, &sig),
        Type::class(
            list,
            vec![Type::Wildcard(WildcardBound::Super(Box::new(Type::class(
                number,
                vec![]
            ))))]
        )
    );
}

#[test]
fn method_type_params_shadow_class_type_params() {
    let mut store = TypeStore::with_minimal_jdk();
    let number = store.class_id("java.lang.Number").unwrap();

    // class <T: Object>
    let csig = parse_class_signature("<T:Ljava/lang/Object;>Ljava/lang/Object;").unwrap();
    let (class_type_params, _super_class, _interfaces) =
        class_sig_from_classfile(&mut store, &TypeVarScope::new(), &csig);
    let class_t = class_type_params[0];

    let mut class_scope = TypeVarScope::new();
    class_scope.insert("T", class_t);

    // method <T: Number>(T)T
    let msig = parse_method_signature("<T:Ljava/lang/Number;>(TT;)TT;").unwrap();
    let desc = parse_method_descriptor("(Ljava/lang/Number;)Ljava/lang/Number;").unwrap();
    let (method_type_params, params, ret) =
        method_sig_from_classfile(&mut store, &class_scope, &msig, &desc);
    let method_t = method_type_params[0];

    assert_ne!(method_t, class_t);
    assert_eq!(params, vec![Type::TypeVar(method_t)]);
    assert_eq!(ret, Type::TypeVar(method_t));

    let method_tp = store.type_param(method_t).unwrap();
    assert_eq!(method_tp.upper_bounds, vec![Type::class(number, vec![])]);
}

#[test]
fn nested_class_segments_flatten_and_apply_mismatch_heuristics() {
    let mut store = TypeStore::with_minimal_jdk();
    let object = store.class_id("java.lang.Object").unwrap();

    // com.example.Outer<T>
    let outer_t = store.add_type_param("T", vec![Type::class(object, vec![])]);
    let _outer = store.add_class(ClassDef {
        name: "com.example.Outer".to_string(),
        kind: ClassKind::Class,
        type_params: vec![outer_t],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    // com.example.Outer$Inner<T, U>
    let inner_u = store.add_type_param("U", vec![Type::class(object, vec![])]);
    let inner = store.add_class(ClassDef {
        name: "com.example.Outer$Inner".to_string(),
        kind: ClassKind::Class,
        type_params: vec![outer_t, inner_u],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let mut scope = TypeVarScope::new();
    scope.insert("T", outer_t);
    scope.insert("U", inner_u);

    let sig = parse_field_signature("Lcom/example/Outer<TT;>.Inner<TU;>;").unwrap();
    assert_eq!(
        ty_from_field_sig(&store, &scope, &sig),
        Type::class(inner, vec![Type::TypeVar(outer_t), Type::TypeVar(inner_u)])
    );

    let sig = parse_field_signature("Lcom/example/Outer.Inner<TU;>;").unwrap();
    assert_eq!(
        ty_from_field_sig(&store, &scope, &sig),
        Type::class(inner, vec![Type::Unknown, Type::TypeVar(inner_u)])
    );
}

#[test]
fn arrays_and_primitives_in_method_signatures() {
    let mut store = TypeStore::with_minimal_jdk();
    let string = store.class_id("java.lang.String").unwrap();

    let msig = parse_method_signature("([I[[Ljava/lang/String;)I").unwrap();
    let desc = parse_method_descriptor("([I[[Ljava/lang/String;)I").unwrap();

    let (_type_params, params, ret) =
        method_sig_from_classfile(&mut store, &TypeVarScope::new(), &msig, &desc);

    assert_eq!(
        params,
        vec![
            Type::Array(Box::new(Type::Primitive(PrimitiveType::Int))),
            Type::Array(Box::new(Type::Array(Box::new(Type::class(string, vec![]))))),
        ]
    );
    assert_eq!(ret, Type::Primitive(PrimitiveType::Int));
}
