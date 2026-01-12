use nova_types::{
    resolve_method_call, CallKind, ClassDef, ClassKind, MethodCall, MethodDef, MethodResolution,
    TyContext, Type, TypeEnv, TypeStore,
};

use pretty_assertions::assert_eq;

#[test]
fn interface_receivers_can_resolve_object_methods_without_explicit_super_class() {
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let string = env.well_known().string;

    // The minimal JDK doesn't currently define `Object.toString()`, but Java member lookup treats
    // `Object` methods as members of every interface type (JLS 4.10.2).
    env.class_mut(object)
        .expect("java.lang.Object should exist")
        .methods
        .push(MethodDef {
            name: "toString".to_string(),
            type_params: vec![],
            params: vec![],
            return_type: Type::class(string, vec![]),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        });

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
}
