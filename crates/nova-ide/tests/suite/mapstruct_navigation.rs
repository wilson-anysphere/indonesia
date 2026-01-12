use std::path::Path;
use std::str::FromStr;

use lsp_types::Uri;
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::InMemoryFileStore;
use nova_ide::{declaration, implementation, Database as IdeDatabase};
use tempfile::TempDir;

use crate::text_fixture::offset_to_position;

#[test]
fn mapstruct_implementation_falls_back_to_generated_impl_method() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    // Ensure `framework_cache::project_root_for_path` can discover a stable project root.
    std::fs::write(root.join("pom.xml"), "<project />\n").expect("write pom.xml");

    let mapper_path = root.join("src/main/java/com/example/CarMapper.java");
    let dto_path = root.join("src/main/java/com/example/CarDto.java");
    let impl_path =
        root.join("target/generated-sources/annotations/com/example/CarMapperImpl.java");

    let mapper_source = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
    @Mapping(target = "make", source = "make")
    CarDto toDto(Car car);
}
"#;

    let dto_source = r#"package com.example;

public class CarDto {
    public String make;
}
"#;

    let impl_source = r#"package com.example;

public class CarMapperImpl implements CarMapper {
    @Override
    public CarDto toDto(Car car) {
        return new CarDto();
    }
}
"#;

    write_file(&mapper_path, mapper_source);
    write_file(&dto_path, dto_source);
    write_file(&impl_path, impl_source);

    let mut db = InMemoryFileStore::new();
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_source.to_string());

    let method_offset = mapper_source
        .find("toDto")
        .expect("method name in mapper source");
    let pos = offset_to_position(mapper_source, method_offset + 1);

    let got = implementation(&db, mapper_file, pos);
    assert_eq!(
        got.len(),
        1,
        "expected one implementation location; got {got:#?}"
    );

    let expected_uri = uri_for_path(&impl_path);
    assert_eq!(got[0].uri, expected_uri);

    let impl_offset = impl_source
        .find("toDto")
        .expect("method name in generated impl source");
    assert_eq!(
        got[0].range.start,
        offset_to_position(impl_source, impl_offset)
    );
    assert_eq!(
        got[0].range.end,
        offset_to_position(impl_source, impl_offset + "toDto".len())
    );
}

#[test]
fn mapstruct_declaration_falls_back_to_target_property_definition() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    std::fs::write(root.join("pom.xml"), "<project />\n").expect("write pom.xml");

    let mapper_path = root.join("src/main/java/com/example/CarMapper.java");
    let dto_path = root.join("src/main/java/com/example/CarDto.java");
    let impl_path =
        root.join("target/generated-sources/annotations/com/example/CarMapperImpl.java");

    let mapper_source = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
    @Mapping(target = "make", source = "make")
    CarDto toDto(Car car);
}
"#;

    let dto_source = r#"package com.example;

public class CarDto {
    public String make;
}
"#;

    // The declaration fallback is driven by filesystem probing; the impl isn't required, but having
    // it present makes the fixture closer to real-world projects.
    let impl_source = r#"package com.example;

public class CarMapperImpl implements CarMapper {
    @Override
    public CarDto toDto(Car car) {
        return new CarDto();
    }
}
"#;

    write_file(&mapper_path, mapper_source);
    write_file(&dto_path, dto_source);
    write_file(&impl_path, impl_source);

    let mut db = InMemoryFileStore::new();
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_source.to_string());

    let target_literal_offset = mapper_source
        .find("target = \"make\"")
        .expect("@Mapping target string in mapper source");
    let make_offset = target_literal_offset + "target = \"".len();
    let pos = offset_to_position(mapper_source, make_offset + 1);

    let got = declaration(&db, mapper_file, pos).expect("expected declaration location");
    assert_eq!(got.uri, uri_for_path(&dto_path));

    let dto_offset = dto_source.find("make").expect("field name in DTO source");
    assert_eq!(got.range.start, offset_to_position(dto_source, dto_offset));
    assert_eq!(
        got.range.end,
        offset_to_position(dto_source, dto_offset + "make".len())
    );
}

#[test]
fn mapstruct_declaration_uses_in_memory_mapper_text() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    std::fs::write(root.join("pom.xml"), "<project />\n").expect("write pom.xml");

    let mapper_path = root.join("src/main/java/com/example/CarMapper.java");
    let dto_path = root.join("src/main/java/com/example/CarDto.java");
    let impl_path =
        root.join("target/generated-sources/annotations/com/example/CarMapperImpl.java");

    let mapper_source_on_disk = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
    @Mapping(target = "seatCount", source = "seatCount")
    CarDto toDto(Car car);
}
"#;

    let mapper_source_in_memory = mapper_source_on_disk.replace("seatCount", "make");

    let dto_source = r#"package com.example;

public class CarDto {
    public int seatCount;
    public String make;
}
"#;

    let impl_source = r#"package com.example;

public class CarMapperImpl implements CarMapper {
    @Override
    public CarDto toDto(Car car) {
        return new CarDto();
    }
}
"#;

    // Disk file still has `seatCount`.
    write_file(&mapper_path, mapper_source_on_disk);
    write_file(&dto_path, dto_source);
    write_file(&impl_path, impl_source);

    // In-memory overlay changes the mapping target to `make`.
    let mut db = InMemoryFileStore::new();
    let mapper_file = db.file_id_for_path(&mapper_path);
    db.set_file_text(mapper_file, mapper_source_in_memory.clone());

    let target_literal_offset = mapper_source_in_memory
        .find("target = \"make\"")
        .expect("@Mapping target string in mapper source");
    let make_offset = target_literal_offset + "target = \"".len();
    let pos = offset_to_position(&mapper_source_in_memory, make_offset + 1);

    let got = declaration(&db, mapper_file, pos).expect("expected declaration location");
    assert_eq!(got.uri, uri_for_path(&dto_path));

    let dto_offset = dto_source.find("make").expect("field name in DTO source");
    assert_eq!(got.range.start, offset_to_position(dto_source, dto_offset));
    assert_eq!(
        got.range.end,
        offset_to_position(dto_source, dto_offset + "make".len())
    );
}
fn write_file(path: &Path, text: &str) {
    let Some(parent) = path.parent() else {
        panic!("path should have a parent: {}", path.display());
    };
    std::fs::create_dir_all(parent).expect("create parent dirs");
    std::fs::write(path, text).expect("write fixture file");
}

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::new(path.to_path_buf()).expect("fixture paths should be absolute");
    let uri = path_to_file_uri(&abs).expect("path should convert to a file URI");
    Uri::from_str(&uri).expect("URI should parse")
}

#[test]
fn mapstruct_snapshot_implementation_falls_back_to_generated_impl_method() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    std::fs::write(root.join("pom.xml"), "<project />\n").expect("write pom.xml");

    let mapper_path = root.join("src/main/java/com/example/CarMapper.java");
    let dto_path = root.join("src/main/java/com/example/CarDto.java");
    let impl_path =
        root.join("target/generated-sources/annotations/com/example/CarMapperImpl.java");

    let mapper_source = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
    @Mapping(target = "make", source = "make")
    CarDto toDto(Car car);
}
"#;

    let dto_source = r#"package com.example;

public class CarDto {
    public String make;
}
"#;

    let impl_source = r#"package com.example;

public class CarMapperImpl implements CarMapper {
    @Override
    public CarDto toDto(Car car) {
        return new CarDto();
    }
}
"#;

    write_file(&mapper_path, mapper_source);
    write_file(&dto_path, dto_source);
    write_file(&impl_path, impl_source);

    let mapper_uri = uri_for_path(&mapper_path);
    let mut db = IdeDatabase::new();
    db.set_file_content(mapper_uri.clone(), mapper_source.to_string());
    let snap = db.snapshot();

    let method_offset = mapper_source
        .find("toDto")
        .expect("method name in mapper source");
    let pos = offset_to_position(mapper_source, method_offset + 1);

    let got = snap.implementation(&mapper_uri, pos);
    assert_eq!(
        got.len(),
        1,
        "expected one implementation location; got {got:#?}"
    );
    assert_eq!(got[0].uri, uri_for_path(&impl_path));

    let impl_offset = impl_source
        .find("toDto")
        .expect("method name in generated impl source");
    assert_eq!(
        got[0].range.start,
        offset_to_position(impl_source, impl_offset)
    );
}

#[test]
fn mapstruct_snapshot_declaration_falls_back_to_target_property_definition() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    std::fs::write(root.join("pom.xml"), "<project />\n").expect("write pom.xml");

    let mapper_path = root.join("src/main/java/com/example/CarMapper.java");
    let dto_path = root.join("src/main/java/com/example/CarDto.java");
    let impl_path =
        root.join("target/generated-sources/annotations/com/example/CarMapperImpl.java");

    let mapper_source = r#"package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
    @Mapping(target = "make", source = "make")
    CarDto toDto(Car car);
}
"#;

    let dto_source = r#"package com.example;

public class CarDto {
    public String make;
}
"#;

    let impl_source = r#"package com.example;

public class CarMapperImpl implements CarMapper {
    @Override
    public CarDto toDto(Car car) {
        return new CarDto();
    }
}
"#;

    write_file(&mapper_path, mapper_source);
    write_file(&dto_path, dto_source);
    write_file(&impl_path, impl_source);

    let mapper_uri = uri_for_path(&mapper_path);
    let mut db = IdeDatabase::new();
    db.set_file_content(mapper_uri.clone(), mapper_source.to_string());
    let snap = db.snapshot();

    let target_literal_offset = mapper_source
        .find("target = \"make\"")
        .expect("@Mapping target string in mapper source");
    let make_offset = target_literal_offset + "target = \"".len();
    let pos = offset_to_position(mapper_source, make_offset + 1);

    let got = snap
        .declaration(&mapper_uri, pos)
        .expect("expected declaration location");
    assert_eq!(got.uri, uri_for_path(&dto_path));

    let dto_offset = dto_source.find("make").expect("field name in DTO source");
    assert_eq!(got.range.start, offset_to_position(dto_source, dto_offset));
}
