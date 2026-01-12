use std::sync::Arc;

use crate::framework_harness::{ide_with_default_registry, offset_to_position, CARET};
use lsp_types::CompletionTextEdit;
use nova_db::InMemoryFileStore;
use nova_scheduler::CancellationToken;
use tempfile::TempDir;

#[test]
fn mapstruct_mapping_target_completions_are_surfaced_via_ide_extensions() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    let pkg_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).unwrap();

    // Provide a minimal build config so `nova_project::load_project` can surface a `ProjectConfig`,
    // enabling `nova-framework` analyzer applicability checks.
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
    .unwrap();

    // Target DTO type with a `seatCount` property.
    let dto_path = pkg_dir.join("CarDto.java");
    let dto_text = r#"package com.example;
 public class CarDto {
   private int seatCount;
 }
 "#;
    std::fs::write(&dto_path, dto_text).unwrap();

    // A tiny source type (not used for this completion test, but keeps the mapper realistic).
    let car_path = pkg_dir.join("Car.java");
    let car_text = r#"package com.example;
 public class Car {}
 "#;
    std::fs::write(&car_path, car_text).unwrap();

    let mapper_path = pkg_dir.join("CarMapper.java");
    let mapper_text_with_caret = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target = "sea<|>")
  CarDto map(Car car);
}
"#;

    let caret_offset = mapper_text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let mapper_text = mapper_text_with_caret.replace(CARET, "");
    let position = offset_to_position(&mapper_text, caret_offset);

    // Write the mapper to disk so MapStruct's filesystem-based discovery can locate it if needed.
    std::fs::write(&mapper_path, &mapper_text).unwrap();

    let mut db = InMemoryFileStore::new();
    let dto_file = db.file_id_for_path(&dto_path);
    db.set_file_text(dto_file, dto_text.to_string());
    let car_file = db.file_id_for_path(&car_path);
    db.set_file_text(car_file, car_text.to_string());

    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_text.clone());

    let db = Arc::new(db);
    let ide = ide_with_default_registry(Arc::clone(&db));

    let items = ide.completions_lsp(CancellationToken::new(), mapper_file, position);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"seatCount"),
        "expected MapStruct completions to include `seatCount`; got {labels:?}"
    );

    let item = items
        .iter()
        .find(|item| item.label == "seatCount")
        .expect("expected MapStruct completion item");
    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    let prefix_start = mapper_text
        .find("sea")
        .expect("expected mapping prefix in fixture");
    assert_eq!(
        edit.range.start,
        offset_to_position(&mapper_text, prefix_start)
    );
    assert_eq!(edit.range.end, position);
}

#[test]
fn mapstruct_mapping_target_nested_path_completions_use_nested_property_type() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    let pkg_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).unwrap();

    // Nested target DTO type.
    let dto_path = pkg_dir.join("CarDto.java");
    std::fs::write(
        &dto_path,
        r#"package com.example;
public class CarDto {
  public Inner inner;
}
"#,
    )
    .unwrap();

    let inner_path = pkg_dir.join("Inner.java");
    std::fs::write(
        &inner_path,
        r#"package com.example;
public class Inner {
  public int seatCount;
}
"#,
    )
    .unwrap();

    // Source type.
    let car_path = pkg_dir.join("Car.java");
    std::fs::write(
        &car_path,
        r#"package com.example;
public class Car {}
"#,
    )
    .unwrap();

    let mapper_path = pkg_dir.join("CarMapper.java");
    let mapper_text_with_caret = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target = "inner.sea<|>")
  CarDto map(Car car);
}
"#;

    let caret_offset = mapper_text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let mapper_text = mapper_text_with_caret.replace(CARET, "");
    let position = offset_to_position(&mapper_text, caret_offset);

    std::fs::write(&mapper_path, &mapper_text).unwrap();

    let mut db = InMemoryFileStore::new();
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_text.clone());

    let db = Arc::new(db);
    let ide = ide_with_default_registry(Arc::clone(&db));

    let items = ide.completions_lsp(CancellationToken::new(), mapper_file, position);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"seatCount"),
        "expected MapStruct nested completions to include `seatCount`; got {labels:?}"
    );

    let item = items
        .iter()
        .find(|item| item.label == "seatCount")
        .expect("expected MapStruct completion item");
    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    let prefix_start = mapper_text
        .find("sea")
        .expect("expected mapping prefix in fixture");
    assert_eq!(
        edit.range.start,
        offset_to_position(&mapper_text, prefix_start)
    );
    assert_eq!(edit.range.end, position);
}
