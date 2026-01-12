use lsp_types::NumberOrString;
use nova_db::InMemoryFileStore;
use std::path::Path;
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

#[test]
fn pull_diagnostics_include_mapstruct_diagnostics_without_extensions() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Ensure `framework_cache::project_root_for_path` can find the workspace root and that
    // `framework_cache::project_config` sees MapStruct dependencies.
    write_file(
        &root.join("pom.xml"),
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
</project>
"#,
    );

    let src_dir = root.join("src/main/java/com/example");
    let mapper_path = src_dir.join("MyMapper.java");
    let source_path = src_dir.join("SourceDto.java");
    let target_path = src_dir.join("TargetDto.java");

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

    write_file(&mapper_path, mapper_text);
    write_file(&source_path, source_text);
    write_file(&target_path, target_text);

    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path(&mapper_path);
    db.set_file_text(file_id, mapper_text.to_string());

    let diagnostics = nova_lsp::diagnostics(&db, file_id);

    assert!(
        diagnostics.iter().any(|d| {
            matches!(
                d.code.as_ref(),
                Some(NumberOrString::String(code)) if code == "MAPSTRUCT_AMBIGUOUS_MAPPING_METHOD"
            )
        }),
        "expected MapStruct ambiguous mapping method diagnostic; got {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().any(|d| {
            matches!(
                d.code.as_ref(),
                Some(NumberOrString::String(code)) if code == "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES"
            )
        }),
        "expected MapStruct unmapped target properties diagnostic; got {diagnostics:#?}"
    );
}

