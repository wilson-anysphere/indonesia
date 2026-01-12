use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nova_core::{FileId, Name, PackageName, QualifiedName, TypeIndex, TypeName};
use nova_hir::queries::HirDatabase;
use nova_jdk::JdkIndex;
use nova_resolve::type_ref::resolve_type_ref_text;
use nova_resolve::{build_scopes, Resolver};
use nova_types::{
    ClassDef, ClassKind, PrimitiveType, Span, Type, TypeEnv, TypeStore, WildcardBound,
};

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
    let tp1 = env.add_type_param("T1", vec![Type::class(object, vec![])]);
    let tp2 = env.add_type_param("T2", vec![Type::class(object, vec![])]);
    let inner_id = env.add_class(ClassDef {
        name: "com.example.Outer$Inner".to_string(),
        kind: ClassKind::Class,
        type_params: vec![tp1, tp2],
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
fn nested_binary_guess_resolves_from_env_when_owner_resolves() {
    // This exercises the "safe" `$`-nesting fallback in `type_ref`: if the first
    // segment resolves to a type in scope (e.g. via imports) but the remaining
    // nested type does not resolve via the classpath/module index, we still
    // attempt to resolve the binary nested name (`Outer$Inner`) from the `TypeEnv`.
    //
    // This fallback is intentionally limited to `$`-style guesses derived from a
    // resolver-confirmed owner, so it does not bypass JPMS/module-access checks.
    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    index.add_type("com.example", "Outer");
    // Intentionally omit `Outer$Inner` so resolver/index-based resolution fails.

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
        "Outer.Inner",
        None,
    );

    assert_eq!(ty.diagnostics, Vec::new());
    assert_eq!(ty.ty, Type::class(inner_id, vec![]));
}

#[test]
fn does_not_fallback_to_env_for_unresolved_qualified_name() {
    // Regression test: type_ref parsing previously fell back to `TypeEnv::lookup_class`
    // for unresolved qualified names, which can bypass JPMS/module-access restrictions
    // when the resolver intentionally returns `None` for inaccessible types.
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);

    // Ensure the type exists in the environment, but not in the resolver's index.
    let mut env = TypeStore::with_minimal_jdk();
    let object = env.well_known().object;
    env.add_class(ClassDef {
        name: "com.example.Hidden".to_string(),
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
        &scopes,
        scope,
        &env,
        &type_vars,
        "com.example.Hidden",
        None,
    );

    assert_eq!(ty.ty, Type::Named("com.example.Hidden".to_string()));
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
    assert!(ty.diagnostics.iter().any(|d| d.code == "invalid-type-ref"
        && d.message
            .contains("type variables cannot have type arguments")));
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
fn does_not_resolve_qualified_types_from_placeholder_env_entries() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let mut env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    // `TypeStore::intern_class_id` creates a placeholder `ClassDef` with no supertypes or members.
    // These placeholders exist so external loaders can reserve stable ids. `TypeRef` parsing must
    // not treat them as "successful" name resolution.
    let _ = env.intern_class_id("com.example.dep.Foo");

    let ty = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "com.example.dep.Foo",
        None,
    );

    assert_eq!(ty.ty, Type::Named("com.example.dep.Foo".to_string()));
    assert!(ty.diagnostics.iter().any(|d| d.code == "unresolved-type"));
}

#[test]
fn type_vars_shadow_classes() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let mut env = TypeStore::with_minimal_jdk();

    let tv = env.add_type_param("String", vec![Type::class(env.well_known().object, vec![])]);
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

    let diamond =
        resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "List<>", None);
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

#[test]
fn type_ref_reports_java_lang_vs_star_import_ambiguity() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import q.*;
class C {}
"#,
    );

    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    index.add_type("q", "String");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&jdk).with_classpath(&index);

    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let result = resolve_type_ref_text(
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &env,
        &type_vars,
        "String",
        None,
    );

    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "ambiguous-type"),
        "expected ambiguous-type diagnostic, got {:#?}",
        result.diagnostics
    );

    let diag = result
        .diagnostics
        .iter()
        .find(|d| d.code.as_ref() == "ambiguous-type")
        .expect("ambiguous-type diagnostic");
    assert!(
        diag.message.contains("q.String") && diag.message.contains("java.lang.String"),
        "expected candidates to include q.String and java.lang.String, got: {}",
        diag.message
    );

    // Best-effort: prefer java.lang to keep type inference stable, but still surface ambiguity.
    let string_id = env
        .lookup_class("java.lang.String")
        .expect("java.lang.String");
    assert_eq!(result.ty, Type::class(string_id, vec![]));
}

#[test]
fn type_use_annotations_are_ignored() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class C {}\n");

    let jdk = JdkIndex::new();
    let index = TestIndex::default();
    let scopes = build_scopes(&db, file);

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let string_id = env
        .lookup_class("java.lang.String")
        .expect("java.lang.String");

    // Mimics `TypeRef.text` output where whitespace is stripped.
    let result = resolve_type_ref_text(
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &env,
        &type_vars,
        "@DeprecatedString",
        None,
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "invalid-type-ref"),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.ty, Type::class(string_id, vec![]));

    // Also accept the spaced variant for completeness.
    let result = resolve_type_ref_text(
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &env,
        &type_vars,
        "@Deprecated String",
        None,
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "invalid-type-ref"),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.ty, Type::class(string_id, vec![]));
}

#[test]
fn type_use_annotation_missing_type_is_diagnosed_when_anchored() {
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let text = "List<@Missing String>";
    let base_span = Span::new(0, text.len());
    let result = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        text,
        Some(base_span),
    );

    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "invalid-type-ref"),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );

    // Even though Nova's `Type` model doesn't represent type-use annotations yet,
    // the annotation type names should still be resolved for diagnostics when we
    // have a base span to anchor them.
    let diag = result
        .diagnostics
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("Missing"))
        .expect("expected unresolved-type diagnostic for missing annotation type");
    let span = diag
        .span
        .expect("expected anchored span for unresolved-type diagnostic");
    assert_eq!(
        &text[span.start..span.end],
        "Missing",
        "expected diagnostic span to cover the annotation name"
    );

    // The type should parse/resolve as if the annotation were not present.
    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<String>",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(result.ty, plain.ty);
}

#[test]
fn type_use_annotations_on_arrays_are_ignored() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class C {}\n");

    let jdk = JdkIndex::new();
    let index = TestIndex::default();
    let scopes = build_scopes(&db, file);

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let string_id = env
        .lookup_class("java.lang.String")
        .expect("java.lang.String");

    let result = resolve_type_ref_text(
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &env,
        &type_vars,
        "String@Deprecated[]",
        None,
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "invalid-type-ref"),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(
        result.ty,
        Type::Array(Box::new(Type::class(string_id, vec![])))
    );
}

#[test]
fn type_use_annotations_in_type_arguments_are_ignored() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import java.util.*;
class C {}
"#,
    );

    let jdk = JdkIndex::new();
    let index = TestIndex::default();
    let scopes = build_scopes(&db, file);

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let list_id = env.lookup_class("java.util.List").expect("java.util.List");
    let string_id = env
        .lookup_class("java.lang.String")
        .expect("java.lang.String");

    // Mimics `TypeRef.text` output where whitespace is stripped.
    let result = resolve_type_ref_text(
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &env,
        &type_vars,
        "List<@DeprecatedString>",
        None,
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "invalid-type-ref"),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(
        result.ty,
        Type::class(list_id, vec![Type::class(string_id, vec![])])
    );
}

#[test]
fn type_use_annotations_in_type_arguments_with_suffix_annotations_are_ignored() {
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<String[]>",
        None,
    );
    // `List<@A String @B []>` -> `List<@AString@B[]>`
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<@AString@B[]>",
        None,
    );

    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_before_fully_qualified_types_are_ignored() {
    // `@A java.util.List<@B String>` -> `@Ajava.util.List<@BString>`
    // `java.util.@A List<@B String>` -> `java.util.@AList<@BString>`
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "java.util.List<String>",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "@Ajava.util.List<@BString>",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);

    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "java.util.@AList<@BString>",
        None,
    );
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_before_varargs_are_ignored() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class C {}\n");

    let jdk = JdkIndex::new();
    let index = TestIndex::default();
    let scopes = build_scopes(&db, file);

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let string_id = env
        .lookup_class("java.lang.String")
        .expect("java.lang.String");

    let result = resolve_type_ref_text(
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &env,
        &type_vars,
        "String@Deprecated...",
        None,
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "invalid-type-ref"),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(
        result.ty,
        Type::Array(Box::new(Type::class(string_id, vec![])))
    );
}

#[test]
fn type_use_annotations_can_be_adjacent() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import java.util.*;
class C {}
"#,
    );

    let jdk = JdkIndex::new();
    let index = TestIndex::default();
    let scopes = build_scopes(&db, file);

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let list_id = env.lookup_class("java.util.List").expect("java.util.List");
    let string_id = env
        .lookup_class("java.lang.String")
        .expect("java.lang.String");

    let expected = Type::class(list_id, vec![Type::class(string_id, vec![])]);

    let result = resolve_type_ref_text(
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &env,
        &type_vars,
        "List<@A@BString>",
        None,
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "invalid-type-ref"),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(result.ty, expected);
}

#[test]
fn type_use_annotations_on_primitive_arrays_are_ignored() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class C {}\n");

    let jdk = JdkIndex::new();
    let index = TestIndex::default();
    let scopes = build_scopes(&db, file);

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let result = resolve_type_ref_text(
        &resolver,
        &scopes.scopes,
        scopes.file_scope,
        &env,
        &type_vars,
        "int@B[]",
        None,
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "invalid-type-ref"),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(
        result.ty,
        Type::Array(Box::new(Type::Primitive(PrimitiveType::Int)))
    );
}

#[test]
fn required_examples_type_use_annotations_are_skipped() {
    // Mirror the examples from the type-use annotation parsing regression:
    // - `List<@A String>` parses/resolves like `List<String>`
    // - `int@B[]` parses/resolves like `int[]`
    // - `String@A...` parses/resolves like `String...` (varargs is one array dimension)
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<String>",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<@A String>",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);

    let plain = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "int[]", None);
    let annotated =
        resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "int@B[]", None);
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);

    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String...",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String@A...",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_with_arguments_are_ignored() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    // Nested parens inside the annotation argument list should not break parsing.
    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String...",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String@A(x=(y))...",
        None,
    );

    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_can_be_qualified_and_glued_to_type_tokens() {
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    // Source `List<@com.example.A String>` becomes `List<@com.example.AString>` in `TypeRef.text`.
    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<String>",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<@com.example.AString>",
        None,
    );

    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_before_qualified_segments_are_ignored() {
    // JLS allows annotations on type uses in qualified class types, e.g.
    // `Outer.@A Inner`. `TypeRef.text` will typically strip whitespace to
    // `Outer.@AInner`.
    let (jdk, index, scopes, scope) = setup(&["import java.util.Map;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "Map.Entry",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "Map.@AEntry",
        None,
    );

    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_before_qualified_segments_and_before_suffixes_are_ignored() {
    // `Map.@A Entry @B []` -> `Map.@AEntry@B[]`
    let (jdk, index, scopes, scope) = setup(&["import java.util.Map;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "Map.Entry[]",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "Map.@AEntry@B[]",
        None,
    );

    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_between_multiple_array_dims_are_ignored() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String[][]",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String@A[]@B[]",
        None,
    );

    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_before_primitives_are_ignored() {
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "int", None);
    let annotated =
        resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "@Aint", None);
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);

    let plain = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "int[]", None);
    let annotated =
        resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "@Aint[]", None);
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);

    // Varargs is encoded as one array dimension in `nova_types::Type`.
    let plain = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "int...", None);
    let annotated = resolve_type_ref_text(
        &resolver, &scopes, scope, &env, &type_vars, "@Aint...", None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_before_type_and_before_suffix_are_ignored() {
    // When whitespace is stripped, element-type annotations and array/varargs annotations can
    // become glued together:
    //
    //   `@A int @B []`     -> `@Aint@B[]`
    //   `@A String @B []`  -> `@AString@B[]`
    //
    // Both should parse/resolve as if annotations were absent.
    let (jdk, index, scopes, scope) = setup(&[]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "int[]", None);
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "@Aint@B[]",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);

    let plain = resolve_type_ref_text(
        &resolver, &scopes, scope, &env, &type_vars, "String[]", None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "@AString@B[]",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);

    // Varargs: `@A String @B ...` -> `@AString@B...`
    let plain = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "String...",
        None,
    );
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "@AString@B...",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);

    let plain = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "int...", None);
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "@Aint@B...",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_can_annotate_wildcards() {
    let (jdk, index, scopes, scope) = setup(&["import java.util.*;"]);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let env = TypeStore::with_minimal_jdk();
    let type_vars = HashMap::new();

    let plain = resolve_type_ref_text(&resolver, &scopes, scope, &env, &type_vars, "List<?>", None);
    let annotated = resolve_type_ref_text(
        &resolver,
        &scopes,
        scope,
        &env,
        &type_vars,
        "List<@A?>",
        None,
    );

    assert_eq!(plain.diagnostics, Vec::new());
    assert_eq!(annotated.diagnostics, Vec::new());
    assert_eq!(annotated.ty, plain.ty);
}

#[test]
fn type_use_annotations_on_parameterized_qualifying_nested_type_are_ignored() {
    // Ensure type-use annotations don't interfere with parsing of per-segment
    // generic args + nested class qualifiers.
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

    let plain = resolve_type_ref_text(
        &resolver,
        &result.scopes,
        result.file_scope,
        &env,
        &type_vars,
        "Outer<String>.Inner<Integer>",
        None,
    );
    assert_eq!(plain.diagnostics, Vec::new());

    // Mimics whitespace-stripped `TypeRef.text` output:
    // `Outer<@A String>.@B Inner<@C Integer>` -> `Outer<@AString>.@BInner<@CInteger>`
    let annotated = resolve_type_ref_text(
        &resolver,
        &result.scopes,
        result.file_scope,
        &env,
        &type_vars,
        "Outer<@AString>.@BInner<@CInteger>",
        None,
    );
    assert_eq!(annotated.diagnostics, Vec::new());

    // Both should resolve to the nested binary name with flattened args.
    assert_eq!(annotated.ty, plain.ty);
    assert_eq!(
        annotated.ty,
        Type::class(
            inner_id,
            vec![
                Type::class(env.lookup_class("java.lang.String").unwrap(), vec![]),
                Type::class(env.lookup_class("java.lang.Integer").unwrap(), vec![]),
            ]
        )
    );
}
