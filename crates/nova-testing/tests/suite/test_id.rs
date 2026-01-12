use nova_testing::{parse_qualified_test_id, QualifiedTestId};
use pretty_assertions::assert_eq;

#[test]
fn parses_legacy_test_ids() {
    assert_eq!(
        parse_qualified_test_id("com.example.Test#ok"),
        QualifiedTestId {
            module: None,
            test: "com.example.Test#ok".to_string()
        }
    );
}

#[test]
fn parses_module_qualified_test_ids() {
    assert_eq!(
        parse_qualified_test_id("service-a::com.example.Test#ok"),
        QualifiedTestId {
            module: Some("service-a".to_string()),
            test: "com.example.Test#ok".to_string()
        }
    );
}
