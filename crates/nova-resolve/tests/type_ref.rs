use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nova_core::{FileId, Name, PackageName, QualifiedName, TypeIndex, TypeName};
use nova_hir::queries::HirDatabase;
use nova_jdk::JdkIndex;
use nova_resolve::type_ref::resolve_type_ref_text;
use nova_resolve::{build_scopes, Resolver};
use nova_types::{Type, TypeEnv, TypeStore};

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

    let result = resolve_type_ref_text(&resolver, &scopes.scopes, scopes.file_scope, &env, &type_vars, "String", None);

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
    let string_id = env.lookup_class("java.lang.String").expect("java.lang.String");
    assert_eq!(result.ty, Type::class(string_id, vec![]));
}
