use nova_framework::{AnalyzerRegistry, CompletionContext, FrameworkAnalyzer, MemoryDatabase};
use nova_framework_mapstruct::MapStructAnalyzer;
use nova_types::Span;
use std::path::Path;
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

struct NoAllFilesDb {
    inner: MemoryDatabase,
}

impl nova_framework::Database for NoAllFilesDb {
    fn class(&self, class: nova_types::ClassId) -> &nova_hir::framework::ClassData {
        nova_framework::Database::class(&self.inner, class)
    }

    fn project_of_class(&self, class: nova_types::ClassId) -> nova_core::ProjectId {
        nova_framework::Database::project_of_class(&self.inner, class)
    }

    fn project_of_file(&self, file: nova_vfs::FileId) -> nova_core::ProjectId {
        nova_framework::Database::project_of_file(&self.inner, file)
    }

    fn file_text(&self, file: nova_vfs::FileId) -> Option<&str> {
        nova_framework::Database::file_text(&self.inner, file)
    }

    fn file_path(&self, file: nova_vfs::FileId) -> Option<&std::path::Path> {
        nova_framework::Database::file_path(&self.inner, file)
    }

    fn file_id(&self, path: &std::path::Path) -> Option<nova_vfs::FileId> {
        nova_framework::Database::file_id(&self.inner, path)
    }

    fn all_files(&self, _project: nova_core::ProjectId) -> Vec<nova_vfs::FileId> {
        Vec::new()
    }

    fn has_dependency(&self, project: nova_core::ProjectId, group: &str, artifact: &str) -> bool {
        nova_framework::Database::has_dependency(&self.inner, project, group, artifact)
    }

    fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
        nova_framework::Database::has_class_on_classpath(&self.inner, project, binary_name)
    }

    fn has_class_on_classpath_prefix(
        &self,
        project: nova_core::ProjectId,
        prefix: &str,
    ) -> bool {
        nova_framework::Database::has_class_on_classpath_prefix(&self.inner, project, prefix)
    }
}

#[test]
fn missing_dependency_diagnostic_when_mapper_present() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Ensure `nova_project::workspace_root` can find the project root.
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    let java_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&java_dir).expect("mkdir java dir");

    let mapper = r#"
package com.example;

 import org.mapstruct.Mapper;
 
 @Mapper
  public interface FooMapper {}
 "#;

    let mapper_path = java_dir.join("FooMapper.java");
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    write_file(&mapper_path, mapper);
    let mapper_file = db.add_file_with_path_and_text(project, mapper_path.clone(), mapper);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, mapper_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "MAPSTRUCT_MISSING_DEPENDENCY"),
        "expected MAPSTRUCT_MISSING_DEPENDENCY diagnostic, got: {diags:#?}"
    );

    // Adding an explicit MapStruct dependency should suppress the missing-dependency diagnostic.
    db.add_dependency(project, "org.mapstruct", "mapstruct");
    let diags = registry.framework_diagnostics(&db, mapper_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "MAPSTRUCT_MISSING_DEPENDENCY"),
        "expected missing dependency diagnostic to disappear, got: {diags:#?}"
    );
}

#[test]
fn missing_dependency_diagnostic_when_mapper_present_with_trivia_in_import() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Ensure `nova_project::workspace_root` can find the project root.
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    let java_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&java_dir).expect("mkdir java dir");

    // `org.mapstruct` is split by trivia so naive substring checks fail.
    let mapper = r#"
package com.example;

import org . mapstruct . Mapper;

@Mapper
public interface FooMapper {}
"#;

    let mapper_path = java_dir.join("FooMapper.java");
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    write_file(&mapper_path, mapper);
    let mapper_file = db.add_file_with_path_and_text(project, mapper_path.clone(), mapper);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, mapper_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "MAPSTRUCT_MISSING_DEPENDENCY"),
        "expected MAPSTRUCT_MISSING_DEPENDENCY diagnostic, got: {diags:#?}"
    );
}

#[test]
fn diagnostics_do_not_require_db_file_enumeration() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();

    // Make the analyzer applicable via classpath-based detection, but don't add
    // any org.mapstruct dependency coordinates.
    inner.add_classpath_class(project, "org.mapstruct.Mapper");

    let mapper = r#"
package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface FooMapper {}
"#;
    let mapper_path = root.join("src/main/java/com/example/FooMapper.java");
    write_file(&mapper_path, mapper);
    let mapper_file = inner.add_file_with_path_and_text(project, mapper_path, mapper);

    // Wrap the DB so the analyzer cannot enumerate project files via `all_files`.
    let db = NoAllFilesDb { inner };

    let analyzer = MapStructAnalyzer::new();
    let diags = analyzer.diagnostics(&db, mapper_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "MAPSTRUCT_MISSING_DEPENDENCY"),
        "expected missing-dependency diagnostic even without db.all_files(), got: {diags:?}"
    );
}

#[test]
fn completions_do_not_require_db_file_enumeration() {
    struct NoAllFilesDb {
        inner: MemoryDatabase,
    }

    impl nova_framework::Database for NoAllFilesDb {
        fn class(&self, class: nova_types::ClassId) -> &nova_hir::framework::ClassData {
            nova_framework::Database::class(&self.inner, class)
        }

        fn project_of_class(&self, class: nova_types::ClassId) -> nova_core::ProjectId {
            nova_framework::Database::project_of_class(&self.inner, class)
        }

        fn project_of_file(&self, file: nova_vfs::FileId) -> nova_core::ProjectId {
            nova_framework::Database::project_of_file(&self.inner, file)
        }

        fn file_text(&self, file: nova_vfs::FileId) -> Option<&str> {
            nova_framework::Database::file_text(&self.inner, file)
        }

        fn file_path(&self, file: nova_vfs::FileId) -> Option<&std::path::Path> {
            nova_framework::Database::file_path(&self.inner, file)
        }

        fn file_id(&self, path: &std::path::Path) -> Option<nova_vfs::FileId> {
            nova_framework::Database::file_id(&self.inner, path)
        }

        fn all_files(&self, _project: nova_core::ProjectId) -> Vec<nova_vfs::FileId> {
            Vec::new()
        }

        fn has_dependency(
            &self,
            project: nova_core::ProjectId,
            group: &str,
            artifact: &str,
        ) -> bool {
            nova_framework::Database::has_dependency(&self.inner, project, group, artifact)
        }

        fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
            nova_framework::Database::has_class_on_classpath(&self.inner, project, binary_name)
        }

        fn has_class_on_classpath_prefix(
            &self,
            project: nova_core::ProjectId,
            prefix: &str,
        ) -> bool {
            nova_framework::Database::has_class_on_classpath_prefix(&self.inner, project, prefix)
        }
    }

    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Ensure `nova_project::workspace_root` can discover the project root.
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    let java_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&java_dir).expect("mkdir java dir");

    let target = r#"
package com.example;

public class CarDto {
  public String make;
  public String model;
}
"#;
    let target_path = java_dir.join("CarDto.java");
    write_file(&target_path, target);

    let mapper_with_cursor = r#"
package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target="ma<cursor>", source="ignored")
  CarDto toDto(Car car);
}
"#;

    let cursor_offset = mapper_with_cursor.find("<cursor>").expect("cursor marker");
    let mapper = mapper_with_cursor.replace("<cursor>", "");

    let mapper_path = java_dir.join("CarMapper.java");
    write_file(&mapper_path, &mapper);

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_classpath_class(project, "org.mapstruct.Mapper");
    let mapper_file = inner.add_file_with_path_and_text(project, mapper_path, mapper.clone());

    // Wrap the DB so the analyzer cannot enumerate project files via `all_files`.
    let db = NoAllFilesDb { inner };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: mapper_file,
        offset: cursor_offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    assert!(
        items.iter().any(|i| i.label == "make"),
        "expected `make` completion even without db.all_files(), got: {items:?}"
    );

    let make = items.iter().find(|i| i.label == "make").unwrap();
    let span = make.replace_span.expect("replace_span");
    assert_eq!(&mapper[span.start..span.end], "ma");
}

#[test]
fn navigation_links_mapper_to_generated_implementation_when_indexed() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.mapstruct.Mapper");

    let mapper = r#"
package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface CarMapper {}
"#;

    let mapper_path = root.join("src/main/java/com/example/CarMapper.java");
    write_file(&mapper_path, mapper);
    let mapper_file = db.add_file_with_path_and_text(project, mapper_path, mapper);

    let impl_src = r#"
package com.example;

public class CarMapperImpl implements CarMapper {}
"#;
    let impl_path =
        root.join("target/generated-sources/annotations/com/example/CarMapperImpl.java");
    write_file(&impl_path, impl_src);
    let impl_file = db.add_file_with_path_and_text(project, impl_path, impl_src);

    let analyzer = MapStructAnalyzer::new();
    let targets = analyzer.navigation(&db, &nova_framework::Symbol::File(mapper_file));
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].file, impl_file);
    assert!(targets[0].label.contains("CarMapperImpl"));
    assert!(targets[0].span.is_none());
}

#[test]
fn unmapped_target_properties_diagnostic_via_framework_analyzer() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.mapstruct", "mapstruct");

    let src_dir = root.join("src/main/java/com/example");
    let source = r#"package com.example;

public class Source {
  private String a;
}
"#;
    let target = r#"package com.example;

public class Target {
  private String a;
  private String b;
}
"#;

    let source_path = src_dir.join("Source.java");
    write_file(&source_path, source);
    db.add_file_with_path_and_text(project, source_path, source);

    let target_path = src_dir.join("Target.java");
    write_file(&target_path, target);
    db.add_file_with_path_and_text(project, target_path, target);

    let mapper = r#"
package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface TestMapper {
  Target map(Source source);
}
"#;

    let mapper_path = src_dir.join("TestMapper.java");
    write_file(&mapper_path, mapper);
    let mapper_file = db.add_file_with_path_and_text(project, mapper_path, mapper);

    let analyzer = MapStructAnalyzer::new();
    let diags = analyzer.diagnostics(&db, mapper_file);

    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES")
        .expect("expected MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES diagnostic");

    assert!(
        diag.message.contains("b"),
        "expected message to mention unmapped property `b`, got: {}",
        diag.message
    );
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

    let cursor_offset = mapper_with_cursor.find("<cursor>").expect("cursor marker");
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
fn completion_does_not_require_db_file_enumeration() {
    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_classpath_class(project, "org.mapstruct.Mapper");

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

    let cursor_offset = mapper_with_cursor.find("<cursor>").expect("cursor marker");
    let mapper = mapper_with_cursor.replace("<cursor>", "");

    let mapper_file = inner.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarMapper.java",
        mapper.clone(),
    );

    inner.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarDto.java",
        r#"
package com.example;

public class CarDto {
  public int seatCount;
}
"#,
    );

    inner.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/Car.java",
        r#"
package com.example;

public class Car {
  public String name;
}
"#,
    );

    let db = NoAllFilesDb { inner };

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
        "expected `seatCount` completion even without db.all_files(), got: {items:?}"
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

    let cursor_offset = mapper_with_cursor.find("<cursor>").expect("cursor marker");
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

    let cursor_offset = mapper_with_cursor.find("<cursor>").expect("cursor marker");
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

    let cursor_offset = mapper_with_cursor.find("<cursor>").expect("cursor marker");
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

#[test]
fn completion_in_mappings_container_suggests_target_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.mapstruct.Mapper");

    let mapper_with_cursor = r#"
package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;
import org.mapstruct.Mappings;

@Mapper
public interface CarMapper {
  @Mappings({
    @Mapping(target="se<cursor>", source="name")
  })
  CarDto carToCarDto(Car car);
}
"#;

    let cursor_offset = mapper_with_cursor.find("<cursor>").expect("cursor marker");
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
fn completion_in_nested_target_path_suggests_nested_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.mapstruct.Mapper");

    let mapper_with_cursor = r#"
package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target="engine.ho<cursor>", source="name")
  CarDto carToCarDto(Car car);
}
"#;

    let cursor_offset = mapper_with_cursor.find("<cursor>").expect("cursor marker");
    let mapper = mapper_with_cursor.replace("<cursor>", "");

    let mapper_file = db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarMapper.java",
        mapper.clone(),
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/EngineDto.java",
        r#"
package com.example;

public class EngineDto {
  public int horsepower;
}
"#,
    );

    db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/CarDto.java",
        r#"
package com.example;

public class CarDto {
  public EngineDto engine;
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
        items.iter().any(|i| i.label == "horsepower"),
        "expected `horsepower` completion, got: {items:?}"
    );

    let hp = items.iter().find(|i| i.label == "horsepower").unwrap();
    let span = hp.replace_span.expect("replace_span");
    assert_eq!(&mapper[span.start..span.end], "ho");
}

#[test]
fn nested_mapping_does_not_trigger_unmapped_target_properties_diagnostic() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("mkdir src dir");

    let mapper_source = r#"
package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;

@Mapper
public interface CarMapper {
  @Mapping(target="engine.horsepower", source="horsepower")
  CarDto carToCarDto(Car car);
}
"#;
    let mapper_path = src_dir.join("CarMapper.java");
    write_file(&mapper_path, mapper_source);

    let engine_source = r#"
package com.example;

public class EngineDto {
  public int horsepower;
}
"#;
    let engine_path = src_dir.join("EngineDto.java");
    write_file(&engine_path, engine_source);

    let cardto_source = r#"
package com.example;

public class CarDto {
  public EngineDto engine;
}
"#;
    let cardto_path = src_dir.join("CarDto.java");
    write_file(&cardto_path, cardto_source);

    let car_source = r#"
package com.example;

public class Car {
  public int horsepower;
}
"#;
    let car_path = src_dir.join("Car.java");
    write_file(&car_path, car_source);

    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    // Ensure the analyzer is applicable and treats the project as having MapStruct.
    db.add_dependency(project, "org.mapstruct", "mapstruct");

    let mapper_file = db.add_file_with_path_and_text(project, mapper_path, mapper_source);
    db.add_file_with_path_and_text(project, engine_path, engine_source);
    db.add_file_with_path_and_text(project, cardto_path, cardto_source);
    db.add_file_with_path_and_text(project, car_path, car_source);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(MapStructAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, mapper_file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES"),
        "expected no unmapped-target diagnostic, got: {diags:?}"
    );
}
