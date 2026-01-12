use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nova_core::{FileId, Name, PackageName, QualifiedName, TypeIndex, TypeName};
use nova_hir::queries::HirDatabase;
use nova_jdk::JdkIndex;
use nova_resolve::type_ref::resolve_type_ref_text;
use nova_resolve::{build_scopes, Resolver};
use nova_types::{ClassDef, ClassKind, PrimitiveType, Type, TypeEnv, TypeStore, WildcardBound};

#[derive(Default)]
struct TestDb {
    files: HashMap<FileId, Arc<str>>,
}

impl TestDb {
    fn set_file_text(&mut self, file: FileId, text: impl Into<Arc<str>>) {
        self.files.insert(file, text.into());
    }
}

impl HirDatabase for TestDb {
    fn file_text(&self, file: FileId) -> Arc<str> {
        self.files
            .get(&file)
            .cloned()
            .unwrap_or_else(|| Arc::from(""))
    }
}

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
    imports: &[&str],
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
    index.add_type("com.example", "Outer");
    index.add_type("com.example", "Outer$Inner");

    let file = FileId::from_raw(0);
    let mut db = TestDb::default();
    let mut src = String::new();
    for line in imports {
        src.push_str(line);
        if !line.ends_with('\n') {
            src.push('\n');
        }
    }
    src.push_str("class C {}\n");
    db.set_file_text(file, src);

    let result = build_scopes(&db, file);
    (jdk, index, result.scopes, result.file_scope)
}

#[test]
fn resolves_string_and_primitives_and_arrays_and_varargs() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let string_id = env.lookup_class("java.lang.String").unwrap();

    let ty = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "String", None);
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

    let ty = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "int[][]", None);
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
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
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
            vec![Type::Wildcard(WildcardBound::Extends(Box::new(
                string.clone()
            )))]
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
        Type::class(list_id, vec![Type::class(list_id, vec![string.clone()])])
    );

    // `>>>` should be treated as three `>` tokens in type contexts.
    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<List<List<String>>>",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::class(
            list_id,
            vec![Type::class(
                list_id,
                vec![Type::class(list_id, vec![string.clone()])]
            )]
        )
    );

    // Wildcard keywords should still parse when whitespace is stripped.
    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<?extendsString>",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::class(
            list_id,
            vec![Type::Wildcard(WildcardBound::Extends(Box::new(
                string.clone()
            )))]
        )
    );

    // Varargs is parsed as one additional array dimension.
    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<String>...",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::Array(Box::new(Type::class(list_id, vec![string.clone()])))
    );
}

#[test]
fn resolves_nested_type_via_imported_outer() {
    let (jdk, index, scopes, scope) = setup(&["import java.util.Map;"]);
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
fn resolves_fully_qualified_nested_type_via_index() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "java.util.Map.Entry",
        None,
    );
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::Named("java.util.Map$Entry".to_string()));
}

#[test]
fn resolves_parameterized_qualifying_nested_type() {
    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    index.add_type("com.example", "Outer");
    index.add_type("com.example", "Outer$Inner");

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let file = FileId::from_raw(0);
    let mut db = TestDb::default();
    db.set_file_text(file, "import com.example.Outer;\nclass C {}\n");
    let result = build_scopes(&db, file);

    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    let inner_id = env.add_class(ClassDef {
        name: "com.example.Outer$Inner".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(object, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let type_vars = HashMap::new();
    let ty = resolve_type_ref_text(
        &resolver,
        &result.scopes,
        result.file_scope,
        &env,
        &type_vars,
        "Outer<String>.Inner<Integer>",
        None,
    );

    // Ensure we don't hit the old parser bug where `.Inner<Integer>` becomes a trailing token.
    assert_eq!(ty.diagnostics, Vec::new());

    // Nested type arguments are flattened outer→inner to match Nova's `Type::Class`
    // representation (and `nova-types-signature` behavior).
    let string = Type::class(env.lookup_class("java.lang.String").unwrap(), vec![]);
    let integer = Type::class(env.lookup_class("java.lang.Integer").unwrap(), vec![]);
    assert_eq!(ty.ty, Type::class(inner_id, vec![string, integer]));
}

#[test]
fn unresolved_nested_type_uses_binary_guess_from_imported_outer() {
    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    // Use a non-`java.*` package so the resolver consults the classpath index.
    index.add_type("com.example", "Map");
    // Intentionally omit `Map$Entry` so scope-based resolution fails.

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let file = FileId::from_raw(0);
    let mut db = TestDb::default();
    db.set_file_text(file, "import com.example.Map;\nclass C {}\n");
    let result = build_scopes(&db, file);

    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let ty = resolve_type_ref_text(
        &resolver,
        &result.scopes,
        result.file_scope,
        &env,
        &type_vars,
        "Map.Entry",
        None,
    );

    assert_eq!(ty.ty, Type::Named("com.example.Map$Entry".to_string()));
    assert!(ty.diagnostics.iter().any(|d| d.code == "unresolved-type"));
}

#[test]
fn malformed_inputs_produce_diagnostics_but_do_not_crash() {
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
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
fn type_variables_shadow_imported_types() {
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let mut env = TypeStore::with_minimal_jdk();

    let tv = env.add_type_param("List", vec![Type::class(env.well_known().object, vec![])]);
    let mut type_vars = HashMap::new();
    type_vars.insert("List".to_string(), tv);

    let ty = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "List", None);
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::TypeVar(tv));

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<String>",
        None,
    );
    assert_eq!(ty.ty, Type::TypeVar(tv));
    assert!(ty
        .diagnostics
        .iter()
        .any(|d| d.code == "invalid-type-ref" && d.message.contains("type variables cannot have type arguments")));
}

#[test]
fn falls_back_to_type_variables_when_name_resolution_fails() {
    let (jdk, index, scopes, scope) = setup(&[]);
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
    assert!(ty.diagnostics.iter().any(|d| d.code == "unresolved-type"));
}

#[test]
fn type_vars_shadow_classes() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let mut env = TypeStore::with_minimal_jdk();

    let tv = env.add_type_param(
        "String",
        vec![Type::class(env.well_known().object, vec![])],
    );
    let mut type_vars = HashMap::new();
    type_vars.insert("String".to_string(), tv);

    let ty = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "String", None);
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::TypeVar(tv));
}

#[test]
fn parses_intersection_types() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let cloneable_id = env.lookup_class("java.lang.Cloneable").unwrap();
    let serializable_id = env.lookup_class("java.io.Serializable").unwrap();

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "java.lang.Cloneable&java.io.Serializable",
        None,
    );

    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(
        ty.ty,
        Type::Intersection(vec![
            Type::class(cloneable_id, vec![]),
            Type::class(serializable_id, vec![]),
        ])
    );
}

#[test]
fn resolves_catch_union_types_via_lub() {
    let (jdk, mut index, scopes, scope) = setup(&["import com.example.*;"]);
    index.add_type("com.example", "Base");
    index.add_type("com.example", "A");
    index.add_type("com.example", "B");

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let mut env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let object = Type::class(env.well_known().object, vec![]);

    let base_id = env.add_class(ClassDef {
        name: "com.example.Base".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(object),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });
    let _a_id = env.add_class(ClassDef {
        name: "com.example.A".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(base_id, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });
    let _b_id = env.add_class(ClassDef {
        name: "com.example.B".to_string(),
        kind: ClassKind::Class,
        type_params: vec![],
        super_class: Some(Type::class(base_id, vec![])),
        interfaces: vec![],
        fields: vec![],
        constructors: vec![],
        methods: vec![],
    });

    let ty = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "A|B", None);
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::class(base_id, vec![]));
}

#[test]
fn resolves_unicode_identifier_type_variable() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let mut env = TypeStore::with_minimal_jdk();

    let object = Type::class(env.well_known().object, vec![]);
    let delta = env.add_type_param("Δ", vec![object]);
    let mut type_vars = HashMap::new();
    type_vars.insert("Δ".to_string(), delta);

    let ty = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "Δ", None);
    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::TypeVar(delta));
}

#[test]
fn parses_diamond_as_raw_type() {
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "List", None);
    assert_eq!(plain.diagnostics, Vec::new());

    let diamond = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "List<>", None);
    assert_eq!(diamond.diagnostics, Vec::new());
    assert_eq!(diamond.ty, plain.ty);
}

#[test]
fn resolves_unicode_identifier_in_same_package() {
    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    index.add_type("p", "π");

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let file = FileId::from_raw(0);
    let mut db = TestDb::default();
    db.set_file_text(file, "package p; class C {}\n");
    let result = build_scopes(&db, file);

    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let ty = resolve_type_ref_text(
        &resolver,
        &result.scopes,
        result.file_scope,
        &env,
        &type_vars,
        "π",
        None,
    );

    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::Named("p.π".to_string()));
}
