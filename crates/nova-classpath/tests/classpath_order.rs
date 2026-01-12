mod suite;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_deps_cache::{
    DependencyIndexBundle, DependencyIndexStore, DepsClassStub, DepsFieldStub, DepsMethodStub,
};
use tempfile::TempDir;

fn make_bundle(jar_sha256: String, method_name: &str) -> DependencyIndexBundle {
    let class = DepsClassStub {
        binary_name: "com.example.Dupe".to_string(),
        internal_name: "com/example/Dupe".to_string(),
        access_flags: 0,
        super_binary_name: None,
        interfaces: Vec::new(),
        signature: None,
        annotations: Vec::new(),
        fields: vec![DepsFieldStub {
            name: format!("FIELD_{method_name}"),
            descriptor: "I".to_string(),
            signature: None,
            access_flags: 0,
            annotations: Vec::new(),
        }],
        methods: vec![DepsMethodStub {
            name: method_name.to_string(),
            descriptor: "()V".to_string(),
            signature: None,
            access_flags: 0,
            annotations: Vec::new(),
        }],
    };

    let binary_names_sorted = vec![class.binary_name.clone()];
    DependencyIndexBundle {
        jar_sha256,
        classes: vec![class],
        packages: vec!["com.example".to_string()],
        package_prefixes: vec!["com".to_string(), "com.example".to_string()],
        trigram_index: nova_deps_cache::build_trigram_index(&binary_names_sorted),
        binary_names_sorted,
    }
}

#[test]
fn classpath_lookup_prefers_first_entry_on_duplicate_classes() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

    let jar1 = tmp.path().join("one.jar");
    let jar2 = tmp.path().join("two.jar");
    std::fs::write(&jar1, b"jar-one").unwrap();
    std::fs::write(&jar2, b"jar-two").unwrap();

    let sha1 = nova_deps_cache::sha256_hex(&jar1).unwrap();
    let sha2 = nova_deps_cache::sha256_hex(&jar2).unwrap();

    deps_store
        .store(&make_bundle(sha1.clone(), "from_jar1"))
        .unwrap();
    deps_store
        .store(&make_bundle(sha2.clone(), "from_jar2"))
        .unwrap();

    let index = ClasspathIndex::build_with_deps_store(
        &[
            ClasspathEntry::Jar(jar1.clone()),
            ClasspathEntry::Jar(jar2.clone()),
        ],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    let stub = index.lookup_binary("com.example.Dupe").unwrap();
    assert!(stub.methods.iter().any(|m| m.name == "from_jar1"));

    let index = ClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(jar2), ClasspathEntry::Jar(jar1)],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    let stub = index.lookup_binary("com.example.Dupe").unwrap();
    assert!(stub.methods.iter().any(|m| m.name == "from_jar2"));
}
