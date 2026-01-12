use lsp_types::{Position, Range};
use nova_db::InMemoryFileStore;
use std::path::Path;
use tempfile::TempDir;

use crate::text_fixture::{offset_to_position, position_to_offset};

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn range_text<'a>(text: &'a str, range: Range) -> &'a str {
    let start = position_to_offset(text, range.start).unwrap();
    let end = position_to_offset(text, range.end).unwrap();
    &text[start..end]
}

#[test]
fn goto_definition_on_mapstruct_mapper_method_returns_generated_impl() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let src_dir = root.join("src/main/java/com/example");
    let generated_dir = root.join("target/generated-sources/annotations/com/example");

    let mapper_path = src_dir.join("CarMapper.java");
    write_file(
        &mapper_path,
        r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
    @Mapping(source = "numberOfSeats", target = "seatCount")
    CarDto carToCarDto(Car car);
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

    write_file(
        &src_dir.join("CarDto.java"),
        r#"package com.example;

public class CarDto {
    public int seatCount;
}
"#,
    );

    let impl_path = generated_dir.join("CarMapperImpl.java");
    write_file(
        &impl_path,
        r#"package com.example;

public class CarMapperImpl implements CarMapper {
  @Override
  public CarDto carToCarDto(Car car) {
    return new CarDto();
  }
}
"#,
    );

    let mapper_text = std::fs::read_to_string(&mapper_path).unwrap();
    let offset = mapper_text.find("carToCarDto").unwrap() + 1;
    let position = offset_to_position(&mapper_text, offset);

    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path(&mapper_path);
    db.set_file_text(file_id, mapper_text);

    let location = nova_ide::goto_definition(&db, file_id, position).expect("definition");
    assert!(location.uri.to_string().ends_with("CarMapperImpl.java"));

    let impl_text = std::fs::read_to_string(&impl_path).unwrap();
    assert_eq!(range_text(&impl_text, location.range), "carToCarDto");
}

#[test]
fn goto_definition_on_mapstruct_mapping_target_returns_target_property() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let src_dir = root.join("src/main/java/com/example");
    let generated_dir = root.join("target/generated-sources/annotations/com/example");

    let mapper_path = src_dir.join("CarMapper.java");
    write_file(
        &mapper_path,
        r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
    @Mapping(source = "numberOfSeats", target = "seatCount")
    CarDto carToCarDto(Car car);
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

    let dto_path = src_dir.join("CarDto.java");
    write_file(
        &dto_path,
        r#"package com.example;

public class CarDto {
    public int seatCount;
}
"#,
    );

    // Create a generated impl so MapStruct navigation is "fully enabled" for this fixture.
    write_file(
        &generated_dir.join("CarMapperImpl.java"),
        r#"package com.example;

public class CarMapperImpl implements CarMapper {
  @Override
  public CarDto carToCarDto(Car car) {
    return new CarDto();
  }
}
"#,
    );

    let mapper_text = std::fs::read_to_string(&mapper_path).unwrap();
    let offset = mapper_text.find("seatCount").unwrap() + 1;
    let position = offset_to_position(&mapper_text, offset);

    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path(&mapper_path);
    db.set_file_text(file_id, mapper_text);

    let location = nova_ide::goto_definition(&db, file_id, position).expect("definition");
    assert!(location.uri.to_string().ends_with("CarDto.java"));

    let dto_text = std::fs::read_to_string(&dto_path).unwrap();
    assert_eq!(range_text(&dto_text, location.range), "seatCount");
}
