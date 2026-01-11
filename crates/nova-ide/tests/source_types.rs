use std::path::PathBuf;

use nova_ide::java_semantics::source_types::SourceTypeProvider;
use nova_types::{
    resolve_method_call, CallKind, MethodCall, MethodResolution, PrimitiveType, TyContext, Type,
    TypeEnv, TypeStore,
};

#[test]
fn source_types_enable_overload_resolution_across_files() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    source.update_file(
        &mut store,
        PathBuf::from("/p/A.java"),
        r#"
package p;
public class A {
  public String m(int x) { return "" + x; }
}
"#,
    );

    source.update_file(
        &mut store,
        PathBuf::from("/p/B.java"),
        r#"
package p;
public class B {
  public void test() { new A().m(1); }
}
"#,
    );

    let call = MethodCall {
        receiver: Type::Named("p.A".to_string()),
        call_kind: CallKind::Instance,
        name: "m",
        args: vec![Type::Primitive(PrimitiveType::Int)],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&store);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success, got {call:?}");
    };

    assert_eq!(found.return_type, Type::class(store.well_known().string, vec![]));
}

#[test]
fn source_type_provider_replaces_definitions_on_update() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();
    let path = PathBuf::from("/p/A.java");

    source.update_file(
        &mut store,
        path.clone(),
        r#"
package p;
class A {
  String m(int x) { return "" + x; }
}
"#,
    );

    let call = MethodCall {
        receiver: Type::Named("p.A".to_string()),
        call_kind: CallKind::Instance,
        name: "m",
        args: vec![Type::Primitive(PrimitiveType::Int)],
        expected_return: None,
        explicit_type_args: vec![],
    };

    let mut ctx = TyContext::new(&store);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success");
    };
    assert_eq!(found.return_type, Type::class(store.well_known().string, vec![]));

    source.update_file(
        &mut store,
        path.clone(),
        r#"
package p;
class A {
  int m(int x) { return x; }
}
"#,
    );

    let mut ctx = TyContext::new(&store);
    let MethodResolution::Found(found) = resolve_method_call(&mut ctx, &call) else {
        panic!("expected method resolution success after update");
    };
    assert_eq!(found.return_type, Type::Primitive(PrimitiveType::Int));

    // Removing the type from the file should remove it from lookup.
    source.update_file(
        &mut store,
        path,
        r#"
package p;
class C {}
"#,
    );

    assert!(store.class_id("p.A").is_none());
}
