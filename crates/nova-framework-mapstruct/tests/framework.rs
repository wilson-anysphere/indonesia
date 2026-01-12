use nova_framework::{AnalyzerRegistry, CompletionContext, MemoryDatabase};
use nova_framework_mapstruct::MapStructAnalyzer;
use nova_types::Span;

#[test]
fn missing_dependency_diagnostic_when_mapper_present() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    // Make the analyzer applicable via classpath-based detection, but don't add
    // any org.mapstruct dependency coordinates.
    db.add_classpath_class(project, "org.mapstruct.Mapper");

    let mapper = r#"
package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface FooMapper {}
"#;

    let mapper_file = db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/FooMapper.java",
        mapper,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, mapper_file);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code.as_ref(), "MAPSTRUCT_MISSING_DEPENDENCY");
}

#[test]
fn completion_in_mapping_target_suggests_target_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.mapstruct.Mapper");

    let mapper_with_cursor = r#"
package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target="se<cursor>", source="name")
  CarDto carToCarDto(Car car);
}
"#;

    let cursor_offset = mapper_with_cursor
        .find("<cursor>")
        .expect("cursor marker");
    let mapper = mapper_with_cursor.replace("<cursor>", "");

    let mapper_file = db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarMapper.java",
        mapper.clone(),
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarDto.java",
        r#"
package com.example;

public class CarDto {
  public int seatCount;
}
"#,
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/Car.java",
        r#"
package com.example;

public class Car {
  public String name;
}
"#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: mapper_file,
        offset: cursor_offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    assert!(
        items.iter().any(|i| i.label == "seatCount"),
        "expected `seatCount` completion, got: {items:?}"
    );

    let seat = items.iter().find(|i| i.label == "seatCount").unwrap();
    let span = seat.replace_span.expect("replace_span");
    assert_eq!(&mapper[span.start..span.end], "se");
    assert_eq!(span, Span::new(cursor_offset - 2, cursor_offset));
}

#[test]
fn completion_in_mapping_source_suggests_source_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.mapstruct.Mapper");

    let mapper_with_cursor = r#"
package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target="seatCount", source="na<cursor>")
  CarDto carToCarDto(Car car);
}
"#;

    let cursor_offset = mapper_with_cursor
        .find("<cursor>")
        .expect("cursor marker");
    let mapper = mapper_with_cursor.replace("<cursor>", "");

    let mapper_file = db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarMapper.java",
        mapper.clone(),
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarDto.java",
        r#"
package com.example;

public class CarDto {
  public int seatCount;
}
"#,
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/Car.java",
        r#"
package com.example;

public class Car {
  public String name;
}
"#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: mapper_file,
        offset: cursor_offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    assert!(
        items.iter().any(|i| i.label == "name"),
        "expected `name` completion, got: {items:?}"
    );

    let name = items.iter().find(|i| i.label == "name").unwrap();
    let span = name.replace_span.expect("replace_span");
    assert_eq!(&mapper[span.start..span.end], "na");
    assert_eq!(span, Span::new(cursor_offset - 2, cursor_offset));
}

#[test]
fn completion_in_mapping_target_suggests_record_components() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.mapstruct.Mapper");

    let mapper_with_cursor = r#"
package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target="se<cursor>", source="name")
  CarDto carToCarDto(Car car);
}
"#;

    let cursor_offset = mapper_with_cursor
        .find("<cursor>")
        .expect("cursor marker");
    let mapper = mapper_with_cursor.replace("<cursor>", "");

    let mapper_file = db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarMapper.java",
        mapper.clone(),
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarDto.java",
        r#"
package com.example;

public record CarDto(int seatCount, String name) {}
"#,
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/Car.java",
        r#"
package com.example;

public class Car {
  public String name;
}
"#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: mapper_file,
        offset: cursor_offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    assert!(
        items.iter().any(|i| i.label == "seatCount"),
        "expected `seatCount` completion for record target, got: {items:?}"
    );

    let seat = items.iter().find(|i| i.label == "seatCount").unwrap();
    let span = seat.replace_span.expect("replace_span");
    assert_eq!(&mapper[span.start..span.end], "se");
    assert_eq!(span, Span::new(cursor_offset - 2, cursor_offset));
}

#[test]
fn completion_resolves_explicit_imported_types() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.mapstruct.Mapper");

    // Two `CarDto` types exist; only the explicitly imported one should be used.
    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/aaa/CarDto.java",
        r#"
package com.aaa;

public class CarDto {
  public int otherProp;
}
"#,
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/dto/CarDto.java",
        r#"
package com.example.dto;

public class CarDto {
  public int seatCount;
}
"#,
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/Car.java",
        r#"
package com.example;

public class Car {
  public String name;
}
"#,
    );

    let mapper_with_cursor = r#"
package com.example;

import com.example.dto.CarDto;
import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target="se<cursor>", source="name")
  CarDto carToCarDto(Car car);
}
"#;

    let cursor_offset = mapper_with_cursor
        .find("<cursor>")
        .expect("cursor marker");
    let mapper = mapper_with_cursor.replace("<cursor>", "");

    let mapper_file = db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarMapper.java",
        mapper.clone(),
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: mapper_file,
        offset: cursor_offset,
    };
    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|i| i.label == "seatCount"),
        "expected `seatCount` completion from imported CarDto, got: {items:?}"
    );

    let seat = items.iter().find(|i| i.label == "seatCount").unwrap();
    let span = seat.replace_span.expect("replace_span");
    assert_eq!(&mapper[span.start..span.end], "se");
}

