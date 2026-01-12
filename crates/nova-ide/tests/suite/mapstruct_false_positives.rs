use std::sync::Arc;

use crate::framework_harness::ide_with_default_registry;
use nova_db::InMemoryFileStore;
use nova_scheduler::CancellationToken;
use tempfile::tempdir;

#[test]
fn mybatis_mapper_does_not_emit_mapstruct_diagnostics() {
    let dir = tempdir().expect("tempdir");

    // Ensure `framework_cache::project_root_for_path` can discover a stable project root.
    std::fs::write(dir.path().join("pom.xml"), "<project />\n").expect("write pom.xml");

    // Provide a tiny MyBatis `@Mapper` annotation stub so core Java diagnostics don't complain
    // about an unresolved import in this fixture.
    let mybatis_mapper_path = dir
        .path()
        .join("src/main/java/org/apache/ibatis/annotations/Mapper.java");
    std::fs::create_dir_all(mybatis_mapper_path.parent().expect("parent")).expect("mkdirs");
    let mybatis_mapper_text = r#"package org.apache.ibatis.annotations;

public @interface Mapper {}
"#;
    std::fs::write(&mybatis_mapper_path, mybatis_mapper_text).expect("write MyBatis Mapper stub");

    let mapper_path = dir.path().join("src/main/java/com/example/MyMapper.java");
    std::fs::create_dir_all(mapper_path.parent().expect("mapper parent")).expect("mkdirs");
    let mapper_text = r#"package com.example;

import org.apache.ibatis.annotations.Mapper;

@Mapper
public interface MyMapper {}
"#;
    std::fs::write(&mapper_path, mapper_text).expect("write mapper java");

    let mut db = InMemoryFileStore::new();
    let mybatis_mapper_file = db.file_id_for_path(&mybatis_mapper_path);
    db.set_file_text(mybatis_mapper_file, mybatis_mapper_text.to_string());
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_text.to_string());

    let db = Arc::new(db);
    let ide = ide_with_default_registry(Arc::clone(&db));

    let diags = ide.all_diagnostics(CancellationToken::new(), mapper_file);
    assert!(
        diags.iter().all(|d| !d.code.starts_with("MAPSTRUCT_")),
        "expected no MapStruct diagnostics for a non-MapStruct @Mapper; got {diags:#?}"
    );
}

