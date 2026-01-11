use nova_testing::schema::{TestDiscoverRequest, TestFramework, TestKind};
use nova_testing::{discover_tests, SCHEMA_VERSION};
use pretty_assertions::assert_eq;
use std::path::PathBuf;

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn discovers_junit5_tests_in_maven_fixture() {
    let root = fixture_root("maven-junit5");
    let resp = discover_tests(&TestDiscoverRequest {
        project_root: root.to_string_lossy().to_string(),
    })
    .unwrap();

    assert_eq!(resp.schema_version, SCHEMA_VERSION);

    let calculator = resp
        .tests
        .iter()
        .find(|t| t.id == "com.example.CalculatorTest")
        .unwrap();

    assert_eq!(calculator.kind, TestKind::Class);
    assert_eq!(calculator.framework, TestFramework::Junit5);

    let child_ids: Vec<_> = calculator.children.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        child_ids,
        vec![
            "com.example.CalculatorTest#adds",
            "com.example.CalculatorTest#parameterizedAdds",
        ]
    );

    let empty = resp
        .tests
        .iter()
        .find(|t| t.id == "com.example.EmptyTest")
        .unwrap();
    assert_eq!(empty.kind, TestKind::Class);
    assert!(empty.children.is_empty());

    let nested = resp
        .tests
        .iter()
        .find(|t| t.id == "com.example.NestedCalculatorTest")
        .unwrap();
    assert_eq!(nested.framework, TestFramework::Junit5);

    let addition = nested
        .children
        .iter()
        .find(|t| t.id == "com.example.NestedCalculatorTest$Addition")
        .unwrap();
    assert_eq!(addition.kind, TestKind::Class);

    let nested_child_ids: Vec<_> = addition.children.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        nested_child_ids,
        vec!["com.example.NestedCalculatorTest$Addition#adds"]
    );
}

#[test]
fn discovers_junit4_tests_in_gradle_fixture() {
    let root = fixture_root("gradle-junit4");
    let resp = discover_tests(&TestDiscoverRequest {
        project_root: root.to_string_lossy().to_string(),
    })
    .unwrap();

    let legacy = resp
        .tests
        .iter()
        .find(|t| t.id == "com.example.LegacyCalculatorTest")
        .unwrap();

    assert_eq!(legacy.framework, TestFramework::Junit4);
    let child_ids: Vec<_> = legacy.children.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        child_ids,
        vec!["com.example.LegacyCalculatorTest#legacyAdds"]
    );
}

#[test]
fn discovers_junit5_tests_in_simple_fixture() {
    let root = fixture_root("simple-junit5");
    let resp = discover_tests(&TestDiscoverRequest {
        project_root: root.to_string_lossy().to_string(),
    })
    .unwrap();

    let simple = resp
        .tests
        .iter()
        .find(|t| t.id == "com.example.SimpleTest")
        .unwrap();

    assert_eq!(simple.framework, TestFramework::Junit5);
    let child_ids: Vec<_> = simple.children.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(child_ids, vec!["com.example.SimpleTest#itWorks"]);
}

#[test]
fn handles_wildcard_imports_and_utf16_ranges() {
    let root = fixture_root("edgecases-junit5");
    let resp = discover_tests(&TestDiscoverRequest {
        project_root: root.to_string_lossy().to_string(),
    })
    .unwrap();

    let wildcard = resp
        .tests
        .iter()
        .find(|t| t.id == "com.example.WildcardImportTest")
        .unwrap();
    assert_eq!(wildcard.framework, TestFramework::Junit5);

    let child_ids: Vec<_> = wildcard.children.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        child_ids,
        vec![
            "com.example.WildcardImportTest#factory",
            "com.example.WildcardImportTest#repeats",
            "com.example.WildcardImportTest#template",
            "com.example.WildcardImportTest#works",
        ]
    );

    let utf16 = resp
        .tests
        .iter()
        .find(|t| t.id == "com.example.Utf16RangeTest")
        .unwrap();
    assert_eq!(utf16.framework, TestFramework::Junit5);

    let it_works = utf16
        .children
        .iter()
        .find(|t| t.id == "com.example.Utf16RangeTest#itWorks")
        .unwrap();

    assert_eq!(it_works.range.start.line, 6);
    assert_eq!(it_works.range.start.character, 24);
    assert_eq!(it_works.range.end.line, 6);
    assert_eq!(it_works.range.end.character, 31);
}
