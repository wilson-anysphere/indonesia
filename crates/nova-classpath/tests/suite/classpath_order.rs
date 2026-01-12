use nova_classpath::{ClasspathEntry, ClasspathIndex, IndexOptions};
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

fn minimal_class_bytes(internal_name: &str, interfaces: &[&str]) -> Vec<u8> {
    fn push_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn push_utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1); // CONSTANT_Utf8
        push_u16(out, s.len() as u16);
        out.extend_from_slice(s.as_bytes());
    }
    fn push_class(out: &mut Vec<u8>, name_index: u16) {
        out.push(7); // CONSTANT_Class
        push_u16(out, name_index);
    }

    const MAJOR_JAVA_8: u16 = 52;
    let super_internal = "java/lang/Object";

    // Constant pool:
    // 1: Utf8 this
    // 2: Class #1
    // 3: Utf8 super
    // 4: Class #3
    // 5+: (interfaces) Utf8 + Class pairs
    let cp_count: u16 = (4 + interfaces.len() * 2 + 1) as u16;

    let mut bytes = Vec::new();
    push_u32(&mut bytes, 0xCAFEBABE);
    push_u16(&mut bytes, 0); // minor
    push_u16(&mut bytes, MAJOR_JAVA_8);
    push_u16(&mut bytes, cp_count);

    push_utf8(&mut bytes, internal_name);
    push_class(&mut bytes, 1);
    push_utf8(&mut bytes, super_internal);
    push_class(&mut bytes, 3);

    let mut interface_class_indices: Vec<u16> = Vec::with_capacity(interfaces.len());
    for (i, interface) in interfaces.iter().enumerate() {
        let utf8_index = 5 + (i * 2) as u16;
        let class_index = utf8_index + 1;
        push_utf8(&mut bytes, interface);
        push_class(&mut bytes, utf8_index);
        interface_class_indices.push(class_index);
    }

    // access_flags (public + super)
    push_u16(&mut bytes, 0x0021);
    // this_class
    push_u16(&mut bytes, 2);
    // super_class
    push_u16(&mut bytes, 4);
    // interfaces_count
    push_u16(&mut bytes, interfaces.len() as u16);
    for idx in interface_class_indices {
        push_u16(&mut bytes, idx);
    }
    // fields_count, methods_count, attributes_count
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);

    bytes
}

fn write_multi_release_jar(
    jar_path: &std::path::Path,
    base_class_bytes: &[u8],
    mr_class_bytes: &[u8],
) {
    use std::io::Write;

    let file = std::fs::File::create(jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::default();

    zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
    zip.write_all(b"Manifest-Version: 1.0\nMulti-Release: true\n")
        .unwrap();

    zip.start_file("com/example/mr/Override.class", options)
        .unwrap();
    zip.write_all(base_class_bytes).unwrap();

    zip.start_file("META-INF/versions/9/com/example/mr/Override.class", options)
        .unwrap();
    zip.write_all(mr_class_bytes).unwrap();

    zip.finish().unwrap();
}

#[test]
fn indexes_multi_release_jar_with_target_release() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

    let jar_path = tmp.path().join("mr.jar");
    let internal_name = "com/example/mr/Override";

    let base_bytes = minimal_class_bytes(internal_name, &[]);
    let mr_bytes = minimal_class_bytes(internal_name, &["java/lang/Runnable"]);
    write_multi_release_jar(&jar_path, &base_bytes, &mr_bytes);

    let entry = ClasspathEntry::Jar(jar_path);

    // Build with release 9 first to ensure the deps cache key is release-aware.
    let index_9 = ClasspathIndex::build_with_deps_store_and_options(
        std::slice::from_ref(&entry),
        None,
        Some(&deps_store),
        None,
        IndexOptions {
            target_release: Some(9),
        },
    )
    .unwrap();
    let stub = index_9.lookup_binary("com.example.mr.Override").unwrap();
    assert_eq!(stub.interfaces, vec!["java.lang.Runnable".to_string()]);

    let index_8 = ClasspathIndex::build_with_deps_store_and_options(
        std::slice::from_ref(&entry),
        None,
        Some(&deps_store),
        None,
        IndexOptions {
            target_release: Some(8),
        },
    )
    .unwrap();
    let stub = index_8.lookup_binary("com.example.mr.Override").unwrap();
    assert!(stub.interfaces.is_empty());

    let index_none = ClasspathIndex::build_with_deps_store_and_options(
        std::slice::from_ref(&entry),
        None,
        Some(&deps_store),
        None,
        IndexOptions {
            target_release: None,
        },
    )
    .unwrap();
    let stub = index_none.lookup_binary("com.example.mr.Override").unwrap();
    assert!(stub.interfaces.is_empty());
}

#[test]
fn indexes_exploded_multi_release_directory_with_target_release() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

    let dir = tmp.path().join("exploded-mr");
    let internal_name = "com/example/mr/Override";

    let base_bytes = minimal_class_bytes(internal_name, &[]);
    let mr_bytes = minimal_class_bytes(internal_name, &["java/lang/Runnable"]);

    std::fs::create_dir_all(dir.join("com/example/mr")).unwrap();
    std::fs::create_dir_all(dir.join("META-INF/versions/9/com/example/mr")).unwrap();
    std::fs::write(dir.join("com/example/mr/Override.class"), &base_bytes).unwrap();
    std::fs::write(
        dir.join("META-INF/versions/9/com/example/mr/Override.class"),
        &mr_bytes,
    )
    .unwrap();

    let cache_dir = tmp.path().join("cache");
    let entry = ClasspathEntry::ClassDir(dir);

    // Build with release 9 first to ensure the per-entry disk cache key is release-aware.
    let index_9 = ClasspathIndex::build_with_deps_store_and_options(
        std::slice::from_ref(&entry),
        Some(&cache_dir),
        Some(&deps_store),
        None,
        IndexOptions {
            target_release: Some(9),
        },
    )
    .unwrap();
    let stub = index_9.lookup_binary("com.example.mr.Override").unwrap();
    assert_eq!(stub.interfaces, vec!["java.lang.Runnable".to_string()]);

    let index_8 = ClasspathIndex::build_with_deps_store_and_options(
        std::slice::from_ref(&entry),
        Some(&cache_dir),
        Some(&deps_store),
        None,
        IndexOptions {
            target_release: Some(8),
        },
    )
    .unwrap();
    let stub = index_8.lookup_binary("com.example.mr.Override").unwrap();
    assert!(stub.interfaces.is_empty());

    let index_none = ClasspathIndex::build_with_deps_store_and_options(
        std::slice::from_ref(&entry),
        Some(&cache_dir),
        Some(&deps_store),
        None,
        IndexOptions {
            target_release: None,
        },
    )
    .unwrap();
    let stub = index_none.lookup_binary("com.example.mr.Override").unwrap();
    assert!(stub.interfaces.is_empty());
}
