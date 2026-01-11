use std::collections::{HashMap, HashSet};

use nova_core::{Name, PackageName, QualifiedName, TypeIndex, TypeName};
use nova_hir::{CompilationUnit, ImportDecl};
use nova_jdk::JdkIndex;
use nova_resolve::type_ref::resolve_type_ref_text;
use nova_resolve::{build_scopes, Resolver};
use nova_types::{PrimitiveType, Type, TypeEnv, TypeStore, WildcardBound};

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

fn setup(
    imports: Vec<ImportDecl>,
) -> (
    JdkIndex,
    TestIndex,
    nova_resolve::ScopeGraph,
    nova_resolve::ScopeId,
) {
    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    // The built-in JDK index used in tests does not include `java.util.Map`, but
    // we want to exercise nested type resolution (`Map.Entry` -> `Map$Entry`).
    index.add_type("java.util", "Map");
    index.add_type("java.util", "Map$Entry");

    let mut unit = CompilationUnit::new(None);
    unit.imports = imports;
    let result = build_scopes(&jdk, &unit);
    (jdk, index, result.scopes, result.file_scope)
}

#[test]
fn resolves_string_and_primitives_and_arrays_and_varargs() {
    let (jdk, index, scopes, scope) = setup(vec![]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let string_id = env.lookup_class("java.lang.String").unwrap();

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::class(string_id, vec![]));

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "java.lang.String",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::class(string_id, vec![]));

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "int[][]",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::Array(Box::new(Type::Array(Box::new(Type::Primitive(
            PrimitiveType::Int
        )))))
    );

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String...",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::Array(Box::new(Type::class(string_id, vec![]))));
}

#[test]
fn resolves_generics_wildcards_arrays_and_nested_closing_angles() {
    let (jdk, index, scopes, scope) = setup(vec![ImportDecl::TypeStar {
        package: PackageName::from_dotted("java.util"),
    }]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let string = Type::class(env.lookup_class("java.lang.String").unwrap(), vec![]);
    let list_id = env.lookup_class("java.util.List").unwrap();

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<String>[]",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::Array(Box::new(Type::class(list_id, vec![string.clone()])))
    );

    let ty = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "List<?>", None);
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::class(list_id, vec![Type::Wildcard(WildcardBound::Unbounded)])
    );

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<? extends String>",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::class(
            list_id,
            vec![Type::Wildcard(WildcardBound::Extends(Box::new(string.clone())))]
        )
    );

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<?superString[]>",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::class(
            list_id,
            vec![Type::Wildcard(WildcardBound::Super(Box::new(Type::Array(
                Box::new(string.clone())
            ))))]
        )
    );

    // `>>` should be treated as two `>` tokens in type contexts.
    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<List<String>>",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::class(
            list_id,
            vec![Type::class(list_id, vec![string.clone()])]
        )
    );
}

#[test]
fn resolves_nested_type_via_imported_outer() {
    let (jdk, index, scopes, scope) = setup(vec![ImportDecl::TypeSingle {
        ty: QualifiedName::from_dotted("java.util.Map"),
        alias: None,
    }]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "Map.Entry",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::Named("java.util.Map$Entry".to_string()));
}

#[test]
fn malformed_inputs_produce_diagnostics_but_do_not_crash() {
    let (jdk, index, scopes, scope) = setup(vec![ImportDecl::TypeStar {
        package: PackageName::from_dotted("java.util"),
    }]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<String",
        None,
    );
    assert!(!ty.diagnostics.is_empty());

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<? extends>",
        None,
    );
    assert!(!ty.diagnostics.is_empty());
}

#[test]
fn falls_back_to_type_variables_when_name_resolution_fails() {
    let (jdk, index, scopes, scope) = setup(vec![]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let mut env = TypeStore::with_minimal_jdk();

    let tv = env.add_type_param("T", vec![Type::class(env.well_known().object, vec![])]);
    let mut type_vars = HashMap::new();
    type_vars.insert("T".to_string(), tv);

    let ty = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "T", None);
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::TypeVar(tv));

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "DoesNotExist",
        None,
    );
    assert_eq!(ty.ty, Type::Named("DoesNotExist".to_string()));
    assert!(ty
        .diagnostics
        .iter()
        .any(|d| d.code == "unresolved-type"));
}

