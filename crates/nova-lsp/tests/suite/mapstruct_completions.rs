use nova_db::InMemoryFileStore;
use nova_test_utils::offset_to_position;
use std::path::Path;
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

#[test]
fn completion_in_mapstruct_mapping_target_suggests_target_properties() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Ensure `framework_cache::project_root_for_path` can find the workspace root.
    write_file(
        &root.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
</project>
"#,
    );

    let src_dir = root.join("src/main/java/com/example");

    let mapper_path = src_dir.join("CarMapper.java");
    let mapper_with_cursor = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

 @Mapper
 public interface CarMapper {
  @Mapping(target = "sea<|>", source = "numberOfSeats")
  CarDto carToCarDto(Car car);
 }
"#;

    let cursor_offset = mapper_with_cursor.find("<|>").expect("cursor marker `<|>`");
    let mapper_text = mapper_with_cursor.replace("<|>", "");
    write_file(&mapper_path, &mapper_text);

    write_file(
        &src_dir.join("CarDto.java"),
        r#"package com.example;

public class CarDto {
  public int seatCount;
}
"#,
    );

    write_file(
        &src_dir.join("Car.java"),
        r#"package com.example;

public class Car {
  public int numberOfSeats;
}
"#,
    );

    let position = offset_to_position(&mapper_text, cursor_offset);

    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path(&mapper_path);
    db.set_file_text(file_id, mapper_text);

    let items = nova_lsp::completion(&db, file_id, position);

    assert!(
        items.iter().any(|item| item.label == "seatCount"),
        "expected `seatCount` completion, got: {items:?}"
    );
}

#[test]
fn completion_in_mapstruct_mapping_source_suggests_source_properties() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Ensure `framework_cache::project_root_for_path` can find the workspace root.
    write_file(
        &root.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
</project>
"#,
    );

    let src_dir = root.join("src/main/java/com/example");

    let mapper_path = src_dir.join("CarMapper.java");
    let mapper_with_cursor = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

 @Mapper
 public interface CarMapper {
  @Mapping(target = "seatCount", source = "num<|>")
  CarDto carToCarDto(Car car);
 }
"#;

    let cursor_offset = mapper_with_cursor.find("<|>").expect("cursor marker `<|>`");
    let mapper_text = mapper_with_cursor.replace("<|>", "");
    write_file(&mapper_path, &mapper_text);

    write_file(
        &src_dir.join("CarDto.java"),
        r#"package com.example;

public class CarDto {
  public int seatCount;
}
"#,
    );

    write_file(
        &src_dir.join("Car.java"),
        r#"package com.example;

public class Car {
  public int numberOfSeats;
}
"#,
    );

    let position = offset_to_position(&mapper_text, cursor_offset);

    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path(&mapper_path);
    db.set_file_text(file_id, mapper_text);

    let items = nova_lsp::completion(&db, file_id, position);

    assert!(
        items.iter().any(|item| item.label == "numberOfSeats"),
        "expected `numberOfSeats` completion, got: {items:?}"
    );
}
