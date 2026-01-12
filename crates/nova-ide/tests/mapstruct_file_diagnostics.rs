use nova_db::InMemoryFileStore;
use nova_ide::file_diagnostics;
use tempfile::TempDir;

#[test]
fn file_diagnostics_include_mapstruct_diagnostics() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    std::fs::write(
        root.join("pom.xml"),
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
    <dependency>
      <groupId>org.mapstruct</groupId>
      <artifactId>mapstruct-processor</artifactId>
      <version>1.5.5.Final</version>
      <scope>provided</scope>
    </dependency>
  </dependencies>
</project>"#,
    )
    .expect("write pom.xml");

    let pkg_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).expect("create java package dir");

    let mapper_path = pkg_dir.join("MyMapper.java");
    let source_path = pkg_dir.join("SourceDto.java");
    let target_path = pkg_dir.join("TargetDto.java");

    let mapper_text = r#"package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface MyMapper {
  TargetDto map(SourceDto source);
  TargetDto map2(SourceDto source);
}
"#;

    let source_text = r#"package com.example;

public class SourceDto {
  public String name;
}
"#;

    let target_text = r#"package com.example;

public class TargetDto {
  public String name;
  public String extra;
}
"#;

    std::fs::write(&mapper_path, mapper_text).expect("write mapper");
    std::fs::write(&source_path, source_text).expect("write source dto");
    std::fs::write(&target_path, target_text).expect("write target dto");

    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path(&mapper_path);
    db.set_file_text(file_id, mapper_text.to_string());

    let diags = file_diagnostics(&db, file_id);

    assert!(
        diags
            .iter()
            .any(|d| d.code == "MAPSTRUCT_AMBIGUOUS_MAPPING_METHOD"),
        "expected MapStruct ambiguous mapping method diagnostic; got {diags:#?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code == "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES"),
        "expected MapStruct unmapped target properties diagnostic; got {diags:#?}"
    );
}

