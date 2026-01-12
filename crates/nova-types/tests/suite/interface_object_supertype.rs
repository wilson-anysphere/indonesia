use nova_types::{
    resolve_method_call, CallKind, ClassDef, ClassKind, MethodCall, MethodResolution, TyContext,
    PrimitiveType, Type, TypeEnv, TypeStore,
};

use pretty_assertions::assert_eq;

#[test]
fn interface_receivers_can_resolve_object_methods_without_explicit_super_class() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    // Java member lookup treats `Object` methods as members of every interface type
    // (JLS 4.10.2), even if the interface has no explicit superclass.

    // Regression setup: custom interface definition with no explicit `super_class`.
    let iface = env.add_class(ClassDef {
        name: "com.example.I".to_string(),
        kind: ClassKind::Interface,
        type_params: vec![],
        super_class: None,
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let call = MethodCall {
        receiver: Type::class(iface, vec![]),
        call_kind: CallKind::Instance,
        name: "toString",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let res = resolve_method_call(&mut ctx, &call);
    let MethodResolution::Found(found) = res else {
        panic!("expected method resolution success: {:?}", res);
    };

    assert_eq!(found.owner, object);
    assert_eq!(found.return_type, Type::class(string, vec![]));
    assert_eq!(found.params, Vec::<Type>::new());

    // `equals(Object)`
    let call = MethodCall {
        receiver: Type::class(iface, vec![]),
        call_kind: CallKind::Instance,
        name: "equals",
        args: vec![Type::class(iface, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let res = resolve_method_call(&mut ctx, &call);
    let MethodResolution::Found(found) = res else {
        panic!("expected method resolution success: {:?}", res);
    };

    assert_eq!(found.owner, object);
    assert_eq!(found.return_type, Type::Primitive(PrimitiveType::Boolean));
    assert_eq!(found.params, vec![Type::class(object, vec![])]);

    // `hashCode()`
    let call = MethodCall {
        receiver: Type::class(iface, vec![]),
        call_kind: CallKind::Instance,
        name: "hashCode",
        args: vec![],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&env);
    let res = resolve_method_call(&mut ctx, &call);
    let MethodResolution::Found(found) = res else {
        panic!("expected method resolution success: {:?}", res);
    };

    assert_eq!(found.owner, object);
    assert_eq!(found.return_type, Type::Primitive(PrimitiveType::Int));
    assert_eq!(found.params, Vec::<Type>::new());
}
