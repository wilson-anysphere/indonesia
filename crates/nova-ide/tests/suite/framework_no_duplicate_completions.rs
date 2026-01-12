use std::sync::Arc;

use nova_db::InMemoryFileStore;
use nova_scheduler::CancellationToken;
use tempfile::TempDir;

use crate::framework_harness::{ide_with_default_registry, CARET};

#[test]
fn framework_completions_not_duplicated_when_build_metadata_is_available() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("project");

    // Maven build metadata marker.
    std::fs::create_dir_all(&root).expect("mkdir root");
    std::fs::write(
        root.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
</project>"#,
    )
    .expect("write pom.xml");

    let pkg_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).expect("mkdir java package dir");

    // Target DTO type with a `seatCount` property.
    let dto_path = pkg_dir.join("CarDto.java");
    let dto_text = r#"package com.example;
public class CarDto {
  private int seatCount;
}
"#;
    std::fs::write(&dto_path, dto_text).expect("write CarDto.java");

    // A tiny source type (not used for this completion test, but keeps the mapper realistic).
    let car_path = pkg_dir.join("Car.java");
    let car_text = r#"package com.example;
public class Car {}
"#;
    std::fs::write(&car_path, car_text).expect("write Car.java");

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

    // Write the mapper to disk so the legacy framework-cache completion path would be able to
    // compute completions if it ran (this test asserts it does *not* run in Maven workspaces).
    std::fs::write(&mapper_path, &mapper_text).expect("write CarMapper.java");

    // Load all relevant files into the in-memory DB so the AnalyzerRegistry-backed MapStruct
    // analyzer can compute completions without relying on filesystem scanning.
    let mut db = InMemoryFileStore::new();
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_text);
    let dto_file = db.file_id_for_path(&dto_path);
    db.set_file_text(dto_file, dto_text.to_string());
    let car_file = db.file_id_for_path(&car_path);
    db.set_file_text(car_file, car_text.to_string());

    let db = Arc::new(db);
    let ide = ide_with_default_registry(Arc::clone(&db));

    let items = ide.completions(CancellationToken::new(), mapper_file, caret_offset);
    let count = items
        .iter()
        .filter(|item| item.label == "seatCount")
        .count();

    assert_eq!(
        count, 1,
        "expected exactly one seatCount completion item; got {items:#?}"
    );
}
