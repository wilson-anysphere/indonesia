use nova_ide::framework_class_data::extract_classes_from_source;
use nova_types::{PrimitiveType, Type};

#[test]
fn extracts_class_annotations_fields_methods_and_constructors() {
    let source = r#"
package com.example;

@com.example.MyAnno
public class Foo {
  @com.example.FieldAnn
  public static final int[] VALUES = new int[0];

  private final java.util.List<String> names;

  public Foo(int id, java.lang.String name) {}

  public static String hello(int a, String[] b) { return ""; }

  void doIt() {}
}
"#;

    let classes = extract_classes_from_source(source);
    let foo = classes
        .iter()
        .find(|class| class.name == "Foo")
        .expect("expected to extract Foo class");

    // Class annotations.
    let anno = foo
        .annotations
        .iter()
        .find(|a| a.name == "MyAnno")
        .expect("expected @MyAnno on class");
    let span = anno.span.expect("expected annotation span");
    assert!(
        source
            .get(span.start..span.end)
            .is_some_and(|s| s.contains("@com.example.MyAnno")),
        "expected span to include annotation; got {:?}",
        span
    );

    // Fields.
    let values = foo
        .fields
        .iter()
        .find(|f| f.name == "VALUES")
        .expect("expected VALUES field");
    assert!(values.is_static);
    assert!(values.is_final);
    assert_eq!(
        values.ty,
        Type::Array(Box::new(Type::Primitive(PrimitiveType::Int)))
    );
    let field_anno = values
        .annotations
        .iter()
        .find(|a| a.name == "FieldAnn")
        .expect("expected @FieldAnn on VALUES");
    assert!(field_anno.span.is_some(), "expected field annotation span");

    let names = foo
        .fields
        .iter()
        .find(|f| f.name == "names")
        .expect("expected names field");
    assert!(!names.is_static);
    assert!(names.is_final);
    assert_eq!(names.ty, Type::Named("java.util.List".to_string()));

    // Methods.
    let hello = foo
        .methods
        .iter()
        .find(|m| m.name == "hello")
        .expect("expected hello method");
    assert!(hello.is_static);
    assert_eq!(hello.return_type, Type::Named("String".to_string()));
    assert_eq!(hello.params.len(), 2);
    assert_eq!(hello.params[0].name, "a");
    assert_eq!(hello.params[0].ty, Type::Primitive(PrimitiveType::Int));
    assert_eq!(hello.params[1].name, "b");
    assert_eq!(
        hello.params[1].ty,
        Type::Array(Box::new(Type::Named("String".to_string())))
    );

    let do_it = foo
        .methods
        .iter()
        .find(|m| m.name == "doIt")
        .expect("expected doIt method");
    assert!(!do_it.is_static);
    assert_eq!(do_it.return_type, Type::Void);
    assert!(do_it.params.is_empty());

    // Constructors.
    assert_eq!(foo.constructors.len(), 1);
    let ctor = &foo.constructors[0];
    assert_eq!(ctor.params.len(), 2);
    assert_eq!(ctor.params[0].name, "id");
    assert_eq!(ctor.params[0].ty, Type::Primitive(PrimitiveType::Int));
    assert_eq!(ctor.params[1].name, "name");
    assert_eq!(
        ctor.params[1].ty,
        Type::Named("java.lang.String".to_string())
    );
}

