use nova_framework::{MemoryDatabase, VirtualMember};
use nova_hir::framework::{Annotation, ClassData, FieldData};
use nova_types::Type;

#[test]
fn builtin_registry_constructs_and_is_usable() {
    let _registry = nova_framework_builtins::builtin_registry();
}

#[test]
fn lombok_virtual_members_smoke_test() {
    let registry = nova_framework_builtins::builtin_registry();

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.projectlombok", "lombok");

    let class = ClassData {
        name: "MyType".to_string(),
        annotations: vec![Annotation::new("Getter")],
        fields: vec![FieldData {
            name: "x".to_string(),
            ty: Type::int(),
            is_static: false,
            is_final: false,
            annotations: Vec::new(),
        }],
        methods: Vec::new(),
        constructors: Vec::new(),
    };
    let class_id = db.add_class(project, class);

    let members = registry.framework_virtual_members(&db, class_id);
    let has_get_x = members.iter().any(|m| match m {
        VirtualMember::Method(method) => method.name == "getX",
        _ => false,
    });
    assert!(has_get_x, "expected Lombok analyzer to generate getX");
}

