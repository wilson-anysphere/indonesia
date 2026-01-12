use nova_framework_mapstruct::{diagnostics_for_file, goto_definition, goto_definition_in_source};
use nova_types::{Severity, Span};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn fixture_project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/simple")
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[test]
fn goto_definition_in_source_matches_goto_definition_fixture() {
    let root = fixture_project_root();
    // Ensure a generated mapper implementation exists on disk so navigation can
    // jump into annotation-processor output.
    let generated_dir = root.join("target/generated-sources/annotations/com/example");
    std::fs::create_dir_all(&generated_dir).unwrap();
    let generated_file = generated_dir.join("CarMapperImpl.java");
    if !generated_file.exists() {
        std::fs::write(
            &generated_file,
            r#"package com.example;

public class CarMapperImpl implements CarMapper {
  @Override
  public CarDto carToCarDto(Car car) {
    return new CarDto();
  }
}
"#,
        )
        .unwrap();
    }

    let mapper_file = root.join("src/main/java/com/example/CarMapper.java");
    let mapper_text = std::fs::read_to_string(&mapper_file).unwrap();
    let offset = mapper_text
        .find("carToCarDto")
        .expect("method name in mapper file");

    let fs_targets = goto_definition(&root, &mapper_file, offset + 1).unwrap();
    let mem_targets =
        goto_definition_in_source(&root, &mapper_file, &mapper_text, offset + 1).unwrap();

    assert_eq!(mem_targets, fs_targets);
}

#[test]
fn diagnostics_for_file_reports_ambiguous_mapping_method() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let mapper_file = root.join("src/main/java/com/example/TestMapper.java");
    let mapper_source = r#"package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface TestMapper {
    Target map(Source source);
    Target map2(Source source);
}
"#;
    write_file(&mapper_file, mapper_source);

    let diags = diagnostics_for_file(root, &mapper_file, mapper_source, true).unwrap();
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "MAPSTRUCT_AMBIGUOUS_MAPPING_METHOD")
        .expect("expected MAPSTRUCT_AMBIGUOUS_MAPPING_METHOD diagnostic");

    assert_eq!(diag.severity, Severity::Error);

    let start = mapper_source.find("map2").unwrap();
    let expected_span = Span::new(start, start + "map2".len());
    assert_eq!(diag.span, Some(expected_span));
}

#[test]
fn diagnostics_for_file_reports_unmapped_target_properties() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let src_dir = root.join("src/main/java/com/example");
    write_file(
        &src_dir.join("Source.java"),
        r#"package com.example;

public class Source {
    private String a;
}
"#,
    );
    write_file(
        &src_dir.join("Target.java"),
        r#"package com.example;

public class Target {
    private String a;
    private String b;
}
"#,
    );

    let mapper_file = src_dir.join("TestMapper.java");
    let mapper_source = r#"package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface TestMapper {
    Target map(Source source);
}
"#;
    write_file(&mapper_file, mapper_source);

    let diags = diagnostics_for_file(root, &mapper_file, mapper_source, true).unwrap();
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES")
        .expect("expected MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES diagnostic");

    assert_eq!(diag.severity, Severity::Warning);
    assert!(
        diag.message
            .contains("Potentially unmapped target properties"),
        "unexpected message: {}",
        diag.message
    );
    assert!(
        diag.message.contains("b"),
        "expected message to mention unmapped property `b`, got: {}",
        diag.message
    );

    let start = mapper_source.find("map(").unwrap();
    let expected_span = Span::new(start, start + "map".len());
    assert_eq!(diag.span, Some(expected_span));
}

#[test]
fn diagnostics_for_file_nested_target_path_does_not_trigger_unmapped_properties() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let src_dir = root.join("src/main/java/com/example");
    write_file(
        &src_dir.join("Source.java"),
        r#"package com.example;

public class Source {
    private String a;
}
"#,
    );
    write_file(
        &src_dir.join("Target.java"),
        r#"package com.example;

public class Target {
    private Nested nested;
}
"#,
    );

    let mapper_file = src_dir.join("TestMapper.java");
    let mapper_source = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface TestMapper {
    @Mapping(target = "nested.value", source = "a")
    Target map(Source source);
}
"#;
    write_file(&mapper_file, mapper_source);

    let diags = diagnostics_for_file(root, &mapper_file, mapper_source, true).unwrap();
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES"),
        "did not expect MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES, got: {diags:?}"
    );
}

#[test]
fn diagnostics_for_file_reports_missing_dependency() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let mapper_file = root.join("src/main/java/com/example/TestMapper.java");
    let mapper_source = r#"package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface TestMapper {
    Target map(Source source);
}
"#;
    write_file(&mapper_file, mapper_source);

    let diags = diagnostics_for_file(root, &mapper_file, mapper_source, false).unwrap();
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "MAPSTRUCT_MISSING_DEPENDENCY")
        .expect("expected MAPSTRUCT_MISSING_DEPENDENCY diagnostic");

    assert_eq!(diag.severity, Severity::Error);
    assert!(
        diag.message
            .contains("no org.mapstruct dependency was detected"),
        "unexpected message: {}",
        diag.message
    );

    let start = mapper_source.find("TestMapper").unwrap();
    let expected_span = Span::new(start, start + "TestMapper".len());
    assert_eq!(diag.span, Some(expected_span));
}

#[test]
fn diagnostics_for_file_ignores_non_mapstruct_mapper_annotation() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let mapper_file = root.join("src/main/java/com/example/TestMapper.java");
    let mapper_source = r#"package com.example;

import org.apache.ibatis.annotations.Mapper;

@Mapper
public interface TestMapper {
    Target map(Source source);
}
"#;
    write_file(&mapper_file, mapper_source);

    let diags = diagnostics_for_file(root, &mapper_file, mapper_source, false).unwrap();
    assert!(
        diags.is_empty(),
        "expected no MapStruct diagnostics for a non-MapStruct @Mapper, got: {diags:?}"
    );
}

#[test]
fn diagnostics_for_file_detects_wildcard_import_mapper() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let mapper_file = root.join("src/main/java/com/example/TestMapper.java");
    let mapper_source = r#"package com.example;

import org.mapstruct.*;

@Mapper
public interface TestMapper {
    Target map(Source source);
}
"#;
    write_file(&mapper_file, mapper_source);

    let diags = diagnostics_for_file(root, &mapper_file, mapper_source, false).unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "MAPSTRUCT_MISSING_DEPENDENCY"),
        "expected MAPSTRUCT_MISSING_DEPENDENCY, got: {diags:?}"
    );
}
