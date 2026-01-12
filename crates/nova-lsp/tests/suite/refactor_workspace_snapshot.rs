use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use nova_lsp::refactor_workspace::RefactorWorkspaceSnapshot;
use nova_refactor::{FileId, RefactorDatabase};
use tempfile::TempDir;

fn file_uri(path: &std::path::Path) -> String {
    let abs = nova_core::AbsPathBuf::new(path.to_path_buf()).expect("absolute path");
    nova_core::path_to_file_uri(&abs).expect("file URI")
}

#[test]
fn snapshot_loads_workspace_with_overlays_and_builds_index() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("project");
    let src_dir = root.join("src/main/java/com/example");
    fs::create_dir_all(&src_dir).expect("create source dir");

    let a_path = src_dir.join("A.java");
    let b_path = src_dir.join("B.java");

    let a_disk = r#"package com.example;

public class A {
    public void foo() {}
}
"#;
    let b_disk = r#"package com.example;

public class B {
    public void bar() {}
}
"#;
    fs::write(&a_path, a_disk).expect("write A.java");
    fs::write(&b_path, b_disk).expect("write B.java");

    let a_overlay = r#"package com.example;

public class A {
    public void fooRenamed() {}
}
"#;

    let a_uri_string = file_uri(&a_path);
    let b_uri_string = file_uri(&b_path);

    let mut overlays: HashMap<String, Arc<str>> = HashMap::new();
    overlays.insert(a_uri_string.clone(), Arc::<str>::from(a_overlay));

    let uri: lsp_types::Uri = a_uri_string.parse().expect("parse URI");
    let snapshot = RefactorWorkspaceSnapshot::build(&uri, &overlays).expect("build snapshot");

    assert_eq!(snapshot.files().len(), 2);
    assert!(snapshot
        .files()
        .contains_key(&FileId::new(a_uri_string.clone())));
    assert!(snapshot
        .files()
        .contains_key(&FileId::new(b_uri_string.clone())));

    let a_text = snapshot
        .db()
        .file_text(&FileId::new(a_uri_string.clone()))
        .expect("A text");
    assert_eq!(a_text, a_overlay);

    let b_text = snapshot
        .db()
        .file_text(&FileId::new(b_uri_string.clone()))
        .expect("B text");
    assert_eq!(b_text, b_disk);

    let index = snapshot.build_index();
    assert!(index.find_method("A", "fooRenamed").is_some());
    assert!(index.find_method("A", "foo").is_none());
    assert!(index.find_method("B", "bar").is_some());
}
