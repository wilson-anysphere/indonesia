use insta::assert_snapshot;
use nova_core::{apply_text_edits, LineIndex, Position, Range};
use nova_format::{
    edits_for_range_formatting, format_java, format_java_ast, FormatConfig, IndentStyle,
};
use nova_syntax::{parse, parse_java};
use pretty_assertions::assert_eq;

#[test]
fn formats_basic_class() {
    let input = r#"
 class  Foo{
public static void main(String[]args){
System.out.println("hi"); // comment
if(true){System.out.println("x");}
}
}
"#;

    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    public static void main(String[] args) {
        System.out.println("hi"); // comment
        if (true) {
            System.out.println("x");
        }
    }
}
"###
    );
}

#[test]
fn formats_broken_syntax_best_effort() {
    let input = "class A{void m(){if(true){System.out.println(\"x\"); // oops\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
class A {
    void m() {
        if (true) {
            System.out.println("x"); // oops
"###
    );
}

#[test]
fn formats_doc_comments() {
    let input = "class Foo{\n/** docs */void m(){}\n}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    /** docs */
    void m() {
    }
}
"###
    );
}

#[test]
fn formats_package_imports_and_class() {
    let input = "package  foo.bar;\nimport java.util.List;import java.util.Map;\npublic class  Foo{}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
package foo.bar;

import java.util.List;
import java.util.Map;

public class Foo {
}
"###
    );
}

#[test]
fn formats_static_import_grouping() {
    let input = "import java.util.List;\nimport java.util.Map;\nimport static java.util.Collections.emptyList;\nimport static java.util.Collections.singletonList;\nclass Foo{}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
import java.util.List;
import java.util.Map;

import static java.util.Collections.emptyList;
import static java.util.Collections.singletonList;

class Foo {
}
"###
    );
}

#[test]
fn formats_record_declaration() {
    let input = "package foo;\npublic record  Point( int x,int y){}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
package foo;

public record Point(int x, int y) {
}
"###
    );
}

#[test]
fn formats_enum_with_constants_and_members() {
    let input = "enum  Color{RED,GREEN,BLUE;int code; void m(){}}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
enum Color {
    RED, GREEN, BLUE;
    int code;
    void m() {
    }
}
"###
    );
}

#[test]
fn formats_annotation_type_declaration() {
    let input = "public @interface  MyAnno{String value();}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
public @interface MyAnno {
    String value();
}
"###
    );
}

#[test]
fn range_formatting_preserves_outside_text() {
    let input = "class Foo {\n    void a() { int x=1; }\n    void b(){int y=2;}\n}\n";
    let tree = parse(input);
    let index = LineIndex::new(input);

    // Select only the `void b(){int y=2;}` line.
    let start = Position::new(2, 0);
    let end_offset = index.line_end(2).unwrap();
    let end = index.position(input, end_offset);
    let range = Range::new(start, end);

    let edits = edits_for_range_formatting(&tree, input, range, &FormatConfig::default()).unwrap();
    let byte_range = index.text_range(input, range).unwrap();
    for edit in &edits {
        assert!(
            edit.range.start() >= byte_range.start() && edit.range.end() <= byte_range.end(),
            "edit {:?} escaped requested range {:?}",
            edit.range,
            byte_range
        );
    }
    let out = apply_text_edits(input, &edits).unwrap();

    assert_eq!(
        out,
        "class Foo {\n    void a() { int x=1; }\n    void b() {\n        int y = 2;\n    }\n}\n"
    );
}

#[test]
fn range_formatting_returns_minimal_edits_within_range() {
    let input = "class Foo {\n    void a() {\n        foo(1,2);\n        baz(7,8);\n    }\n}\n";
    let tree = parse(input);
    let index = LineIndex::new(input);

    // Select two adjacent lines; both need formatting.
    let start = Position::new(2, 0);
    let end_offset = index.line_end(3).unwrap();
    let end = index.position(input, end_offset);
    let range = Range::new(start, end);

    let edits = edits_for_range_formatting(&tree, input, range, &FormatConfig::default()).unwrap();
    assert!(edits.len() > 1, "expected multiple edits, got {edits:?}");

    let byte_range = index.text_range(input, range).unwrap();
    for edit in &edits {
        assert!(
            edit.range.start() >= byte_range.start() && edit.range.end() <= byte_range.end(),
            "edit {:?} escaped requested range {:?}",
            edit.range,
            byte_range
        );
    }

    let out = apply_text_edits(input, &edits).unwrap();
    assert_eq!(
        out,
        "class Foo {\n    void a() {\n        foo(1, 2);\n        baz(7, 8);\n    }\n}\n"
    );
}

#[test]
fn range_formatting_inside_switch_case_uses_case_indent() {
    let input = "class Foo {\n    void m(int x) {\n        switch(x){\n        case 1:\n        int y=2;\n        break;\n        default:\n        break;\n        }\n    }\n}\n";
    let tree = parse(input);
    let index = LineIndex::new(input);

    // Select only the `int y=2;` line.
    let start = Position::new(4, 0);
    let end_offset = index.line_end(4).unwrap();
    let end = index.position(input, end_offset);
    let range = Range::new(start, end);

    let edits = edits_for_range_formatting(&tree, input, range, &FormatConfig::default()).unwrap();
    let out = apply_text_edits(input, &edits).unwrap();

    assert_eq!(
        out,
        "class Foo {\n    void m(int x) {\n        switch(x){\n        case 1:\n                int y = 2;\n        break;\n        default:\n        break;\n        }\n    }\n}\n"
    );
}

#[test]
fn range_formatting_switch_label_does_not_include_case_body_indent() {
    let input = "class Foo {\n    void m(int x) {\n        switch(x){\n        case 1:\n            foo();\n        break;\n        default:\n            bar();\n        break;\n        }\n    }\n}\n";
    let tree = parse(input);
    let index = LineIndex::new(input);

    // Select only the `default:` label line.
    let start = Position::new(6, 0);
    let end_offset = index.line_end(6).unwrap();
    let end = index.position(input, end_offset);
    let range = Range::new(start, end);

    let edits = edits_for_range_formatting(&tree, input, range, &FormatConfig::default()).unwrap();
    let out = apply_text_edits(input, &edits).unwrap();

    assert_eq!(
        out,
        "class Foo {\n    void m(int x) {\n        switch(x){\n        case 1:\n            foo();\n        break;\n            default:\n            bar();\n        break;\n        }\n    }\n}\n"
    );
}

#[test]
fn formats_with_tabs_indentation() {
    let input = "class Foo{void m(){int x=1;}}\n";
    let tree = parse(input);
    let config = FormatConfig {
        indent_style: IndentStyle::Tabs,
        ..FormatConfig::default()
    };
    let formatted = format_java(&tree, input, &config);

    assert_eq!(
        formatted,
        "class Foo {\n\tvoid m() {\n\t\tint x = 1;\n\t}\n}\n"
    );
}

#[test]
fn respects_final_newline_policies() {
    let input_no_newline = "class Foo{}";
    let tree = parse(input_no_newline);
    let config = FormatConfig {
        insert_final_newline: Some(true),
        ..FormatConfig::default()
    };
    let formatted = format_java(&tree, input_no_newline, &config);
    assert_eq!(formatted, "class Foo {\n}\n");

    let input_extra_newlines = "class Foo{/* oops\n\n";
    let tree = parse(input_extra_newlines);
    let config = FormatConfig {
        trim_final_newlines: Some(true),
        ..FormatConfig::default()
    };
    let formatted = format_java(&tree, input_extra_newlines, &config);
    assert_eq!(formatted, "class Foo {\n    /* oops\n");
}
