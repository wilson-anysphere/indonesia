use std::collections::{HashMap, HashSet};

use nova_core::{Name, PackageName, QualifiedName, TypeIndex, TypeName};
use nova_hir::{CompilationUnit, ImportDecl};
use nova_jdk::JdkIndex;
use nova_types::{ClassDef, ClassKind, Type, TypeEnv, TypeStore, WildcardBound};

use nova_resolve::type_ref::parse_type_ref;
use nova_resolve::{build_scopes, Resolver};

#[derive(Default)]
struct TestIndex {
    types: HashMap<String, TypeName>,
    package_to_types: HashMap<String, HashMap<String, TypeName>>,
    packages: HashSet<String>,
}

impl TestIndex {
    fn add_type(&mut self, package: &str, name: &str) -> TypeName {
        let fq = if package.is_empty() {
            name.to_string()
        } else {
            format!("{package}.{name}")
        };
        let id = TypeName::new(fq.clone());
        self.types.insert(fq, id.clone());
        self.packages.insert(package.to_string());
        self.package_to_types
            .entry(package.to_string())
            .or_default()
            .insert(name.to_string(), id.clone());
        id
    }
}

impl TypeIndex for TestIndex {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        self.types.get(&name.to_dotted()).cloned()
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        self.package_to_types
            .get(&package.to_dotted())
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.packages.contains(&package.to_dotted())
    }
}

#[test]
fn parses_simple_java_lang_type() {
    let jdk = JdkIndex::new();
    let unit = CompilationUnit::new(None);
    let scopes = build_scopes(&jdk, &unit);
    let resolver = Resolver::new(&jdk);

    let mut types = TypeStore::with_minimal_jdk();
    let string = types.well_known().string;

    let ty = parse_type_ref(
        "String",
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &mut types,
    );

    assert_eq!(ty, Type::class(string, vec![]));
}

#[test]
fn parses_generic_class_type() {
    let jdk = JdkIndex::new();
    let mut unit = CompilationUnit::new(None);
    unit.imports.push(ImportDecl::TypeStar {
        package: PackageName::from_dotted("java.util"),
    });
    let scopes = build_scopes(&jdk, &unit);
    let resolver = Resolver::new(&jdk);

    let mut types = TypeStore::with_minimal_jdk();
    let list = types.class_id("java.util.List").unwrap();
    let string = types.well_known().string;

    let ty = parse_type_ref(
        "List<String>",
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &mut types,
    );

    assert_eq!(ty, Type::class(list, vec![Type::class(string, vec![])]));
}

#[test]
fn parses_wildcards_and_arrays() {
    let jdk = JdkIndex::new();
    let mut unit = CompilationUnit::new(None);
    unit.imports.push(ImportDecl::TypeStar {
        package: PackageName::from_dotted("java.util"),
    });
    let scopes = build_scopes(&jdk, &unit);
    let resolver = Resolver::new(&jdk);

    let mut types = TypeStore::with_minimal_jdk();
    let list = types.class_id("java.util.List").unwrap();
    let string = types.well_known().string;

    let expected = Type::Array(Box::new(Type::class(
        list,
        vec![Type::Wildcard(WildcardBound::Extends(Box::new(
            Type::class(string, vec![]),
        )))],
    )));

    let ty = parse_type_ref(
        "List<? extends String>[]",
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &mut types,
    );

    assert_eq!(ty, expected);
}

#[test]
fn resolves_nested_member_types_through_imported_outer() {
    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    index.add_type("java.util", "Map");
    let _entry = index.add_type("java.util", "Map$Entry");

    let mut unit = CompilationUnit::new(None);
    unit.imports.push(ImportDecl::TypeSingle {
        ty: QualifiedName::from_dotted("java.util.Map"),
        alias: None,
    });
    let scopes = build_scopes(&jdk, &unit);
    let resolver = Resolver::new(&jdk).with_classpath(&index);

    let mut types = TypeStore::with_minimal_jdk();
    types.add_class(ClassDef {
        name: "java.util.Map".to_string(),
        kind: ClassKind::Interface,
        type_params: Vec::new(),
        super_class: None,
        interfaces: Vec::new(),
        fields: Vec::new(),
        constructors: Vec::new(),
        methods: Vec::new(),
    });
    let entry_id = types.add_class(ClassDef {
        name: "java.util.Map$Entry".to_string(),
        kind: ClassKind::Interface,
        type_params: Vec::new(),
        super_class: None,
        interfaces: Vec::new(),
        fields: Vec::new(),
        constructors: Vec::new(),
        methods: Vec::new(),
    });

    let ty = parse_type_ref(
        "Map.Entry<?>",
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &mut types,
    );

    assert_eq!(
        ty,
        Type::class(entry_id, vec![Type::Wildcard(WildcardBound::Unbounded)])
    );
}
