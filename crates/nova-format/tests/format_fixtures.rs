use insta::assert_snapshot;
use nova_core::{apply_text_edits, Position};
use nova_format::{edits_for_on_type_formatting, format_java, FormatConfig};
use nova_syntax::parse;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;

fn format_with_config(input: &str, config: &FormatConfig) -> String {
    let tree = parse(input);
    format_java(&tree, input, config)
}

fn assert_idempotent(name: &str, input: &str, config: &FormatConfig) {
    let formatted = format_with_config(input, config);
    let tree = parse(&formatted);
    let formatted_again = format_java(&tree, &formatted, config);
    assert_eq!(
        formatted, formatted_again,
        "fixture `{name}` is not idempotent"
    );
}

#[test]
fn idempotence_corpus_fixtures_default_config() {
    let config = FormatConfig::default();
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let entries = fs::read_dir(&fixtures_dir).expect("read fixtures directory");

    for entry in entries {
        let entry = entry.expect("read fixture entry");
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("java") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>");
        let input = fs::read_to_string(&path).expect("read fixture");
        assert_idempotent(name, &input, &config);
    }
}

#[test]
fn snapshot_generics() {
    let input = include_str!("fixtures/generics.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("generics", formatted);
    assert_idempotent("generics", input, &config);
}

#[test]
fn snapshot_annotations() {
    let input = include_str!("fixtures/annotations.java");
    let config = FormatConfig {
        max_line_length: 60,
        ..FormatConfig::default()
    };
    let formatted = format_with_config(input, &config);
    assert_snapshot!("annotations", formatted);
    assert_idempotent("annotations", input, &config);
}

#[test]
fn snapshot_lambdas_and_method_refs() {
    let input = include_str!("fixtures/lambdas_method_refs.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("lambdas_method_refs", formatted);
    assert_idempotent("lambdas_method_refs", input, &config);
}

#[test]
fn snapshot_switch_expressions() {
    let input = include_str!("fixtures/switch_expressions.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("switch_expressions", formatted);
    assert_idempotent("switch_expressions", input, &config);
}

#[test]
fn snapshot_try_with_resources() {
    let input = include_str!("fixtures/try_with_resources.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("try_with_resources", formatted);
    assert_idempotent("try_with_resources", input, &config);
}

#[test]
fn snapshot_chained_calls_wrapping() {
    let input = include_str!("fixtures/chained_calls.java");
    let config = FormatConfig {
        max_line_length: 60,
        ..FormatConfig::default()
    };
    let formatted = format_with_config(input, &config);
    assert_snapshot!("chained_calls", formatted);
    assert_idempotent("chained_calls", input, &config);
}

#[test]
fn snapshot_binary_expression_wrapping() {
    let input = include_str!("fixtures/binary_expressions.java");
    let config = FormatConfig {
        max_line_length: 40,
        ..FormatConfig::default()
    };
    let formatted = format_with_config(input, &config);
    assert_snapshot!("binary_expressions", formatted);
    assert_idempotent("binary_expressions", input, &config);
}

#[test]
fn snapshot_broken_code_is_panic_free() {
    let input = include_str!("fixtures/broken_code.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("broken_code", formatted);
    // The second pass must also be stable even with malformed input.
    assert_idempotent("broken_code", input, &config);
}

#[test]
fn snapshot_method_reference_type_args() {
    let input = include_str!("fixtures/method_reference_type_args.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("method_reference_type_args", formatted);
    assert_idempotent("method_reference_type_args", input, &config);
}

#[test]
fn snapshot_control_flow_constructs() {
    let input = include_str!("fixtures/do_while_synchronized_switch.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("control_flow_constructs", formatted);
    assert_idempotent("control_flow_constructs", input, &config);
}

#[test]
fn snapshot_wildcards_and_varargs() {
    let input = include_str!("fixtures/wildcards_and_varargs.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("wildcards_and_varargs", formatted);
    assert_idempotent("wildcards_and_varargs", input, &config);
}

#[test]
fn snapshot_switch_case_comments() {
    let input = include_str!("fixtures/switch_case_comment.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("switch_case_comment", formatted);
    assert_idempotent("switch_case_comment", input, &config);
}

#[test]
fn snapshot_diamond_operator() {
    let input = include_str!("fixtures/diamond_operator.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("diamond_operator", formatted);
    assert_idempotent("diamond_operator", input, &config);
}

#[test]
fn snapshot_qualified_generics() {
    let input = include_str!("fixtures/qualified_generics.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("qualified_generics", formatted);
    assert_idempotent("qualified_generics", input, &config);
}

#[test]
fn snapshot_comparison_uppercase_constants() {
    let input = include_str!("fixtures/comparison_uppercase_constants.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("comparison_uppercase_constants", formatted);
    assert_idempotent("comparison_uppercase_constants", input, &config);
}

#[test]
fn snapshot_array_initializers() {
    let input = include_str!("fixtures/array_initializers.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("array_initializers", formatted);
    assert_idempotent("array_initializers", input, &config);
}

#[test]
fn snapshot_annotation_array_values() {
    let input = include_str!("fixtures/annotation_array_values.java");
    let config = FormatConfig::default();
    let formatted = format_with_config(input, &config);
    assert_snapshot!("annotation_array_values", formatted);
    assert_idempotent("annotation_array_values", input, &config);
}

#[test]
fn on_type_formatting_triggers_inside_argument_lists() {
    let input = "class A {\n    void m() {\nfoo(1,2);\n    }\n}\n";
    let tree = parse(input);
    // Cursor after the comma in `foo(1,2);`
    let position = Position::new(2, 6);
    let edits = edits_for_on_type_formatting(&tree, input, position, ',', &FormatConfig::default())
        .unwrap();
    assert_eq!(edits.len(), 1);
    let out = apply_text_edits(input, &edits).unwrap();
    assert!(out.contains("        foo(1,2);"));
}

#[test]
#[ignore]
fn formats_large_file_regression() {
    let mut src = String::from("class Large {\n");
    for i in 0..20_000u32 {
        src.push_str("    void m");
        src.push_str(&i.to_string());
        src.push_str("(){int x=");
        src.push_str(&i.to_string());
        src.push_str(";if(x==");
        src.push_str(&i.to_string());
        src.push_str("){System.out.println(x);} }\n");
    }
    src.push_str("}\n");

    let tree = parse(&src);
    let formatted = format_java(&tree, &src, &FormatConfig::default());
    assert!(!formatted.is_empty());
}
