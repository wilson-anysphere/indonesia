use lsp_types::{Position, Range};
use nova_db::InMemoryFileStore;
use std::path::Path;
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut cur: usize = 0;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    Position::new(line, col_utf16)
}

fn position_to_offset(text: &str, position: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut offset: usize = 0;

    for ch in text.chars() {
        if line == position.line && col_utf16 == position.character {
            return Some(offset);
        }

        offset += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    if line == position.line && col_utf16 == position.character {
        Some(offset)
    } else {
        None
    }
}

fn range_text<'a>(text: &'a str, range: Range) -> &'a str {
    let start = position_to_offset(text, range.start).unwrap();
    let end = position_to_offset(text, range.end).unwrap();
    &text[start..end]
}

#[test]
fn implementation_on_mapstruct_mapper_method_returns_generated_impl() {
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

    let locations = nova_lsp::implementation(&db, file_id, position);
    assert_eq!(locations.len(), 1);
    let location = &locations[0];
    assert!(location.uri.to_string().ends_with("CarMapperImpl.java"));

    let impl_text = std::fs::read_to_string(&impl_path).unwrap();
    assert_eq!(range_text(&impl_text, location.range), "carToCarDto");
}

#[test]
fn implementation_on_mapstruct_mapper_method_supports_custom_implementation_name() {
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

@Mapper(implementationName = "<CLASS_NAME>Generated")
public interface CarMapper {
    @Mapping(source = "numberOfSeats", target = "seatCount")
    CarDto carToCarDto(Car car);
}
"#,
    );

    let impl_path = generated_dir.join("CarMapperGenerated.java");
    write_file(
        &impl_path,
        r#"package com.example;

public class CarMapperGenerated implements CarMapper {
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

    let locations = nova_lsp::implementation(&db, file_id, position);
    assert_eq!(locations.len(), 1);
    let location = &locations[0];
    assert!(location.uri.to_string().ends_with("CarMapperGenerated.java"));

    let impl_text = std::fs::read_to_string(&impl_path).unwrap();
    assert_eq!(range_text(&impl_text, location.range), "carToCarDto");
}
