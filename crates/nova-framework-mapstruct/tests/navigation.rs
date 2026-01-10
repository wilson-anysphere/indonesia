use nova_framework_mapstruct::goto_definition;
use std::path::PathBuf;

fn fixture_project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/simple")
}

fn fixture_custom_impl_project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/custom")
}

#[test]
fn goto_definition_mapper_method_to_generated_impl() {
    let root = fixture_project_root();
    // Fixture tests need a generated mapper implementation on disk to validate
    // navigation into annotation-processor output. We create a tiny stub under
    // the conventional Maven generated sources path.
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

    let targets = goto_definition(&root, &mapper_file, offset + 1).unwrap();
    assert_eq!(targets.len(), 1);
    let target = &targets[0];
    assert!(target.file.ends_with("CarMapperImpl.java"));

    let impl_text = std::fs::read_to_string(&target.file).unwrap();
    assert_eq!(
        &impl_text[target.span.start..target.span.end],
        "carToCarDto"
    );
}

#[test]
fn goto_definition_mapping_target_to_target_field() {
    let root = fixture_project_root();
    let mapper_file = root.join("src/main/java/com/example/CarMapper.java");
    let mapper_text = std::fs::read_to_string(&mapper_file).unwrap();
    let offset = mapper_text
        .find("seatCount")
        .expect("target property in @Mapping");

    let targets = goto_definition(&root, &mapper_file, offset + 1).unwrap();
    assert_eq!(targets.len(), 1);
    let target = &targets[0];
    assert!(target.file.ends_with("CarDto.java"));

    let dto_text = std::fs::read_to_string(&target.file).unwrap();
    assert_eq!(&dto_text[target.span.start..target.span.end], "seatCount");
}

#[test]
fn goto_definition_respects_impl_name_and_package() {
    let root = fixture_custom_impl_project_root();
    let mapper_file = root.join("src/main/java/com/example/CarMapper.java");
    let mapper_text = std::fs::read_to_string(&mapper_file).unwrap();
    let offset = mapper_text
        .find("carToCarDto")
        .expect("method name in mapper file");

    let targets = goto_definition(&root, &mapper_file, offset + 1).unwrap();
    assert_eq!(targets.len(), 1);
    let target = &targets[0];
    assert!(target.file.ends_with("CarMapperCustomImpl.java"));

    let impl_text = std::fs::read_to_string(&target.file).unwrap();
    assert_eq!(
        &impl_text[target.span.start..target.span.end],
        "carToCarDto"
    );
}
