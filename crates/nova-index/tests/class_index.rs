use nova_index::ClassIndex;
use nova_project::{SourceRoot, SourceRootKind, SourceRootOrigin};

#[test]
fn class_index_prefers_sources_over_generated() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let src_root = root.join("src/main/java");
    let gen_root = root.join("target/generated-sources/annotations");

    let src_file = src_root.join("com/example/Foo.java");
    let gen_file = gen_root.join("com/example/Foo.java");

    std::fs::create_dir_all(src_file.parent().unwrap()).unwrap();
    std::fs::create_dir_all(gen_file.parent().unwrap()).unwrap();

    std::fs::write(
        &src_file,
        r#"
            package com.example;
            public class Foo {}
        "#,
    )
    .unwrap();

    std::fs::write(
        &gen_file,
        r#"
            package com.example;
            public class Foo {}
        "#,
    )
    .unwrap();

    let roots = vec![
        SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: src_root,
        },
        SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Generated,
            path: gen_root,
        },
    ];

    let index = ClassIndex::build(&roots).unwrap();
    let picked = index.lookup("com.example.Foo").unwrap();

    assert_eq!(picked.file, src_file);
}

#[test]
fn class_index_tie_breaker_is_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let a_root = root.join("src/main/java");
    let b_root = root.join("src/test/java");

    let a_file = a_root.join("com/example/Foo.java");
    let b_file = b_root.join("com/example/Foo.java");

    std::fs::create_dir_all(a_file.parent().unwrap()).unwrap();
    std::fs::create_dir_all(b_file.parent().unwrap()).unwrap();

    std::fs::write(
        &a_file,
        r#"
            package com.example;
            public class Foo {}
        "#,
    )
    .unwrap();
    std::fs::write(
        &b_file,
        r#"
            package com.example;
            public class Foo {}
        "#,
    )
    .unwrap();

    let roots = vec![
        SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: a_root,
        },
        SourceRoot {
            kind: SourceRootKind::Test,
            origin: SourceRootOrigin::Source,
            path: b_root,
        },
    ];

    let index = ClassIndex::build(&roots).unwrap();
    let picked = index.lookup("com.example.Foo").unwrap();

    // Main sources should win over tests when both are user-authored.
    assert_eq!(picked.kind, SourceRootKind::Main);
}
