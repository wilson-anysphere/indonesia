use nova_framework_mapstruct::goto_definition;
use std::path::PathBuf;

fn fixture_project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/simple")
}

fn fixture_custom_impl_project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/custom")
}

fn fixture_overload_project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/overload")
}

fn fixture_update_project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/update")
}

fn line_containing_span(text: &str, start: usize) -> &str {
    let start = start.min(text.len());
    let line_start = text[..start].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let line_end = text[start..]
        .find('\n')
        .map(|idx| start + idx)
        .unwrap_or(text.len());
    &text[line_start..line_end]
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

#[test]
fn goto_definition_overloaded_methods_select_correct_impl() {
    let root = fixture_overload_project_root();
    let mapper_file = root.join("src/main/java/com/example/VehicleMapper.java");
    let mapper_text = std::fs::read_to_string(&mapper_file).unwrap();

    let car_offset = mapper_text.find("map(Car").expect("car overload");
    let bike_offset = mapper_text.find("map(Bike").expect("bike overload");

    let car_targets = goto_definition(&root, &mapper_file, car_offset + 1).unwrap();
    assert_eq!(car_targets.len(), 1);
    let car_target = &car_targets[0];
    assert!(car_target.file.ends_with("VehicleMapperImpl.java"));
    let impl_text = std::fs::read_to_string(&car_target.file).unwrap();
    let line = line_containing_span(&impl_text, car_target.span.start);
    assert!(
        line.contains("map(Car car)"),
        "expected Car overload, got line: {line}"
    );

    let bike_targets = goto_definition(&root, &mapper_file, bike_offset + 1).unwrap();
    assert_eq!(bike_targets.len(), 1);
    let bike_target = &bike_targets[0];
    assert!(bike_target.file.ends_with("VehicleMapperImpl.java"));
    let line = line_containing_span(&impl_text, bike_target.span.start);
    assert!(
        line.contains("map(Bike bike)"),
        "expected Bike overload, got line: {line}"
    );
}

#[test]
fn goto_definition_update_method_to_generated_impl() {
    let root = fixture_update_project_root();
    let mapper_file = root.join("src/main/java/com/example/CarMapper.java");
    let mapper_text = std::fs::read_to_string(&mapper_file).unwrap();
    let offset = mapper_text
        .find("updateCarDto")
        .expect("method name in mapper file");

    let targets = goto_definition(&root, &mapper_file, offset + 1).unwrap();
    assert_eq!(targets.len(), 1);
    let target = &targets[0];
    assert!(target.file.ends_with("CarMapperImpl.java"));

    let impl_text = std::fs::read_to_string(&target.file).unwrap();
    let line = line_containing_span(&impl_text, target.span.start);
    assert!(
        line.contains("updateCarDto(Car car, CarDto carDto)"),
        "expected update method signature, got line: {line}"
    );
}
