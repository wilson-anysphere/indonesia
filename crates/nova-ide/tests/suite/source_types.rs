use std::path::PathBuf;

use nova_ide::java_semantics::source_types::SourceTypeProvider;
use nova_types::{
    is_subtype, resolve_method_call, CallKind, MethodCall, MethodResolution, PrimitiveType,
    TyContext, Type, TypeEnv, TypeStore,
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

    assert_eq!(
        found.return_type,
        Type::class(store.well_known().string, vec![])
    );
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
    assert_eq!(
        found.return_type,
        Type::class(store.well_known().string, vec![])
    );

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

#[test]
fn source_type_provider_preserves_static_field_and_method_modifiers() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    source.update_file(
        &mut store,
        PathBuf::from("/p/Statics.java"),
        r#"
package p;
class Statics {
  static final int CONST = 1;
  static int util() { return 1; }
}
"#,
    );

    let class_id = store
        .class_id("p.Statics")
        .expect("class should be registered");
    let class_def = store.class(class_id).expect("class def should exist");

    let field = class_def
        .fields
        .iter()
        .find(|f| f.name == "CONST")
        .expect("field CONST should exist");
    assert!(field.is_static);
    assert!(field.is_final);

    let method = class_def
        .methods
        .iter()
        .find(|m| m.name == "util")
        .expect("method util should exist");
    assert!(method.is_static);
}

#[test]
fn source_types_lower_enum_constants_as_static_final_fields() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    source.update_file(
        &mut store,
        PathBuf::from("/p/Color.java"),
        r#"
package p;
enum Color { RED, GREEN }
"#,
    );

    let color_id = store
        .class_id("p.Color")
        .expect("expected p.Color to be defined");
    let color = store.class(color_id).expect("expected p.Color class def");

    let red = color
        .fields
        .iter()
        .find(|f| f.name == "RED")
        .expect("expected enum constant RED to be lowered as a field");
    assert!(red.is_static);
    assert!(red.is_final);
}

#[test]
fn source_types_capture_implements_relationships_for_subtyping() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    source.update_file(
        &mut store,
        PathBuf::from("/p/Test.java"),
        r#"
package p;
interface I {}
class Impl implements I {}
"#,
    );

    assert!(is_subtype(
        &store,
        &Type::Named("p.Impl".to_string()),
        &Type::Named("p.I".to_string())
    ));
}

#[test]
fn source_types_capture_extends_relationships_for_super_class() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    source.update_file(
        &mut store,
        PathBuf::from("/p/Test.java"),
        r#"
package p;
class A { int x; }
class B extends A {}
"#,
    );

    let b_id = store
        .class_id("p.B")
        .expect("expected SourceTypeProvider to register p.B");
    let b_def = store.class(b_id).unwrap();
    let Some(sc) = &b_def.super_class else {
        panic!("expected p.B to have a super_class");
    };

    match sc {
        Type::Class(class_ty) => {
            let super_def = store.class(class_ty.def).unwrap();
            assert_eq!(super_def.name, "p.A");
        }
        Type::Named(name) => {
            assert_eq!(name, "p.A");
        }
        other => panic!("unexpected super_class type: {other:?}"),
    }
}

#[test]
fn source_types_resolve_supertypes_even_when_defined_in_later_files() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    // Define the subtype first; the supertype is added in a later `update_file` call.
    source.update_file(
        &mut store,
        PathBuf::from("/p/B.java"),
        r#"
package p;
class B extends A {}
"#,
    );
    source.update_file(
        &mut store,
        PathBuf::from("/p/A.java"),
        r#"
package p;
class A { int x; }
"#,
    );

    assert!(is_subtype(
        &store,
        &Type::Named("p.B".to_string()),
        &Type::Named("p.A".to_string())
    ));
}

#[test]
fn source_types_resolve_imported_interfaces_even_when_defined_in_later_files() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    source.update_file(
        &mut store,
        PathBuf::from("/a/Impl.java"),
        r#"
package a;
import z.I;
class Impl implements I {}
"#,
    );
    source.update_file(
        &mut store,
        PathBuf::from("/z/I.java"),
        r#"
package z;
public interface I {}
"#,
    );

    assert!(is_subtype(
        &store,
        &Type::Named("a.Impl".to_string()),
        &Type::Named("z.I".to_string())
    ));
}

#[test]
fn source_types_resolve_fully_qualified_interfaces_even_when_defined_in_later_files() {
    let mut store = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    // `implements z.I` should be treated as a fully-qualified package/type name rather than
    // rewriting `z` into an in-scope type like `a.z$I` when `z.I` hasn't been loaded yet.
    source.update_file(
        &mut store,
        PathBuf::from("/a/Impl.java"),
        r#"
package a;
class Impl implements z.I {}
"#,
    );
    source.update_file(
        &mut store,
        PathBuf::from("/z/I.java"),
        r#"
package z;
public interface I {}
"#,
    );

    assert!(is_subtype(
        &store,
        &Type::Named("a.Impl".to_string()),
        &Type::Named("z.I".to_string())
    ));
}
