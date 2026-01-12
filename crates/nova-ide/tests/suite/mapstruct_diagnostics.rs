use std::sync::Arc;

use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::ProjectId;
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;
use nova_types::Severity;
use tempfile::tempdir;

#[test]
fn maven_mapper_without_mapstruct_dependency_emits_missing_dependency() {
    let dir = tempdir().expect("tempdir");

    std::fs::write(
        dir.path().join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
</project>"#,
    )
    .expect("write pom.xml");

    let mapper_path = dir.path().join("src/main/java/com/example/MyMapper.java");
    std::fs::create_dir_all(mapper_path.parent().expect("mapper parent")).expect("mkdirs");

    let mapper_text = r#"package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface MyMapper {
  Target map(Source source);
}

class Source {
  public int id;
}

class Target {
  public int id;
}
"#;
    std::fs::write(&mapper_path, mapper_text).expect("write mapper java");

    let mut db = InMemoryFileStore::new();
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_text.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::<dyn nova_db::Database + Send + Sync>::with_default_registry(
        Arc::clone(&db),
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
    );

    let diags = ide.all_diagnostics(CancellationToken::new(), mapper_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "MAPSTRUCT_MISSING_DEPENDENCY" && d.severity == Severity::Error),
        "expected MAPSTRUCT_MISSING_DEPENDENCY; got {diags:#?}"
    );
}

#[test]
fn maven_mapper_with_mapstruct_dependency_does_not_emit_missing_dependency() {
    let dir = tempdir().expect("tempdir");

    std::fs::write(
        dir.path().join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>org.mapstruct</groupId>
      <artifactId>mapstruct</artifactId>
      <version>1.5.5.Final</version>
    </dependency>
  </dependencies>
</project>"#,
    )
    .expect("write pom.xml");

    let mapper_path = dir.path().join("src/main/java/com/example/MyMapper.java");
    std::fs::create_dir_all(mapper_path.parent().expect("mapper parent")).expect("mkdirs");

    let mapper_text = r#"package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface MyMapper {
  Target map(Source source);
}

class Source {
  public int id;
}

class Target {
  public int id;
}
"#;
    std::fs::write(&mapper_path, mapper_text).expect("write mapper java");

    let mut db = InMemoryFileStore::new();
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_text.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::<dyn nova_db::Database + Send + Sync>::with_default_registry(
        Arc::clone(&db),
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
    );

    let diags = ide.all_diagnostics(CancellationToken::new(), mapper_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code == "MAPSTRUCT_MISSING_DEPENDENCY"),
        "expected MAPSTRUCT_MISSING_DEPENDENCY to be absent; got {diags:#?}"
    );
}

#[test]
fn mapstruct_ambiguous_and_unmapped_diagnostics_are_surfaced_via_ide_extensions() {
    let dir = tempdir().expect("tempdir");

    std::fs::write(
        dir.path().join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>org.mapstruct</groupId>
      <artifactId>mapstruct</artifactId>
      <version>1.5.5.Final</version>
    </dependency>
  </dependencies>
</project>"#,
    )
    .expect("write pom.xml");

    let src_path = dir.path().join("src/main/java/com/example/Source.java");
    let target_path = dir.path().join("src/main/java/com/example/Target.java");
    let mapper_path = dir.path().join("src/main/java/com/example/MyMapper.java");
    std::fs::create_dir_all(mapper_path.parent().expect("java parent")).expect("mkdirs");

    let source_text = r#"package com.example;

public class Source {
  public int id;
}
"#;
    let target_text = r#"package com.example;

public class Target {
  public int id;
  public String name;
}
"#;
    let mapper_text = r#"package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface MyMapper {
  Target map(Source source);
  Target map2(Source source);
}
"#;

    std::fs::write(&src_path, source_text).expect("write Source.java");
    std::fs::write(&target_path, target_text).expect("write Target.java");
    std::fs::write(&mapper_path, mapper_text).expect("write MyMapper.java");

    let mut db = InMemoryFileStore::new();
    let source_file = db.file_id_for_path(&src_path);
    db.set_file_text(source_file, source_text.to_string());
    let target_file = db.file_id_for_path(&target_path);
    db.set_file_text(target_file, target_text.to_string());
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_text.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::<dyn nova_db::Database + Send + Sync>::with_default_registry(
        Arc::clone(&db),
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
    );

    let diags = ide.all_diagnostics(CancellationToken::new(), mapper_file);

    assert!(
        diags.iter().any(
            |d| d.code == "MAPSTRUCT_AMBIGUOUS_MAPPING_METHOD" && d.severity == Severity::Error
        ),
        "expected MAPSTRUCT_AMBIGUOUS_MAPPING_METHOD; got {diags:#?}"
    );

    assert!(
        diags
            .iter()
            .any(|d| d.code == "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES"
                && d.severity == Severity::Warning),
        "expected MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES; got {diags:#?}"
    );
}
