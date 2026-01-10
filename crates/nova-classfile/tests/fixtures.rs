use nova_classfile::{
    BaseType, ClassFile, ClassMember, ConstValue, ElementValue, FieldType, ReturnType, TypeSignature,
};

#[test]
fn parse_simple_classfile_and_stub() {
    let bytes = include_bytes!("../testdata/Simple.class");
    let class = ClassFile::parse(bytes).unwrap();
    assert_eq!(class.this_class, "com/example/Simple");
    assert_eq!(class.super_class.as_deref(), Some("java/lang/Object"));
    assert!(class.signature.is_none());
    assert_eq!(class.fields.len(), 1);
    assert_eq!(class.fields[0].name, "f");
    assert_eq!(class.fields[0].descriptor, "I");

    let stub = class.stub().unwrap();
    assert_eq!(stub.internal_name, "com/example/Simple");
    assert_eq!(
        stub.fields[0].parsed_descriptor,
        FieldType::Base(BaseType::Int)
    );

    let m = stub.methods.iter().find(|m| m.name == "m").unwrap();
    assert_eq!(m.parsed_descriptor.params.len(), 0);
    assert_eq!(m.parsed_descriptor.return_type, ReturnType::Void);
}

#[test]
fn parse_generic_signatures() {
    let bytes = include_bytes!("../testdata/Generic.class");
    let class = ClassFile::parse(bytes).unwrap();
    assert_eq!(class.this_class, "com/example/Generic");
    assert_eq!(
        class.signature.as_deref(),
        Some("<T:Ljava/lang/Number;>Ljava/lang/Object;")
    );

    let stub = class.stub().unwrap();
    let sig = stub.signature.unwrap();
    assert_eq!(sig.type_parameters.len(), 1);
    assert_eq!(sig.type_parameters[0].name, "T");
    let bound = sig.type_parameters[0].class_bound.as_ref().unwrap();
    match bound {
        TypeSignature::Class(ct) => assert_eq!(ct.internal_name(), "java/lang/Number"),
        other => panic!("unexpected bound: {other:?}"),
    }

    let field = &stub.fields[0];
    assert_eq!(field.name, "value");
    assert_eq!(field.signature, Some(TypeSignature::TypeVariable("T".into())));

    let method = stub.methods.iter().find(|m| m.name == "id").unwrap();
    let msig = method.signature.clone().unwrap();
    assert_eq!(msig.type_parameters.len(), 1);
    assert_eq!(msig.type_parameters[0].name, "U");
    assert_eq!(msig.parameters, vec![TypeSignature::TypeVariable("U".into())]);
    assert_eq!(msig.return_type, Some(TypeSignature::TypeVariable("U".into())));
}

#[test]
fn parse_runtime_visible_annotations() {
    let bytes = include_bytes!("../testdata/Annotated.class");
    let class = ClassFile::parse(bytes).unwrap();
    assert_eq!(class.this_class, "com/example/Annotated");
    assert_eq!(class.runtime_visible_annotations.len(), 1);
    assert!(class.runtime_invisible_annotations.is_empty());

    let ann = &class.runtime_visible_annotations[0];
    assert_eq!(ann.type_descriptor, "Lcom/example/Ann;");
    assert_eq!(ann.type_internal_name.as_deref(), Some("com/example/Ann"));
    assert_eq!(ann.elements.len(), 2);

    let mut elems = ann
        .elements
        .iter()
        .map(|(k, v)| (k.as_str(), v))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        elems.remove("value").unwrap(),
        &ElementValue::Const(ConstValue::String("hello".into()))
    );
    assert_eq!(
        elems.remove("nums").unwrap(),
        &ElementValue::Array(vec![
            ElementValue::Const(ConstValue::Int(1)),
            ElementValue::Const(ConstValue::Int(2)),
        ])
    );
}

#[test]
fn parse_runtime_invisible_annotations() {
    let bytes = include_bytes!("../testdata/InvisibleAnnotated.class");
    let class = ClassFile::parse(bytes).unwrap();
    assert_eq!(class.this_class, "com/example/InvisibleAnnotated");
    assert!(class.runtime_visible_annotations.is_empty());
    assert_eq!(class.runtime_invisible_annotations.len(), 1);

    let ann = &class.runtime_invisible_annotations[0];
    assert_eq!(ann.type_descriptor, "Lcom/example/Ann;");
    assert_eq!(ann.type_internal_name.as_deref(), Some("com/example/Ann"));
    assert_eq!(
        ann.elements,
        vec![(
            "value".to_string(),
            ElementValue::Const(ConstValue::String("hello".into()))
        )]
    );

    // Stubs should surface both visible and invisible annotations.
    let stub = class.stub().unwrap();
    assert_eq!(stub.annotations.len(), 1);
    assert_eq!(stub.annotations[0].type_descriptor, "Lcom/example/Ann;");
}

#[test]
fn parse_inner_classes_attribute() {
    let bytes = include_bytes!("../testdata/Outer.class");
    let class = ClassFile::parse(bytes).unwrap();
    assert_eq!(class.this_class, "com/example/Outer");
    assert_eq!(class.inner_classes.len(), 1);
    let inner = &class.inner_classes[0];
    assert_eq!(inner.inner_class, "com/example/Outer$Inner");
    assert_eq!(inner.outer_class.as_deref(), Some("com/example/Outer"));
    assert_eq!(inner.inner_name.as_deref(), Some("Inner"));
    assert_eq!(inner.access_flags, 0x0001);
}

#[test]
fn stub_is_best_effort_for_unparseable_signature_attribute() {
    let class = ClassFile {
        minor_version: 0,
        major_version: 52,
        access_flags: 0x0021,
        this_class: "com/example/BadSignature".into(),
        super_class: Some("java/lang/Object".into()),
        interfaces: Vec::new(),
        fields: vec![ClassMember {
            access_flags: 0x0001,
            name: "f".into(),
            descriptor: "I".into(),
            signature: Some("not a signature".into()),
            runtime_visible_annotations: Vec::new(),
            runtime_invisible_annotations: Vec::new(),
        }],
        methods: Vec::new(),
        signature: Some("not a signature".into()),
        runtime_visible_annotations: Vec::new(),
        runtime_invisible_annotations: Vec::new(),
        inner_classes: Vec::new(),
    };

    let stub = class.stub().unwrap();
    assert_eq!(stub.raw_signature.as_deref(), Some("not a signature"));
    assert!(stub.signature.is_none(), "parsed signature should be omitted");

    let field = &stub.fields[0];
    assert_eq!(field.raw_signature.as_deref(), Some("not a signature"));
    assert!(field.signature.is_none(), "parsed field signature should be omitted");
}
