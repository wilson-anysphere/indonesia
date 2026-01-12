use insta::assert_snapshot;
use nova_core::{apply_text_edits, LineIndex, Position, Range};
use nova_format::{
    edits_for_document_formatting, edits_for_document_formatting_with_strategy,
    edits_for_formatting, edits_for_formatting_ast, edits_for_on_type_formatting,
    edits_for_range_formatting, format_java, format_java_ast, format_member_insertion_with_newline,
    FormatConfig, FormatStrategy, IndentStyle, NewlineStyle,
};
use nova_syntax::{parse, parse_java};
use pretty_assertions::assert_eq;

fn assert_crlf_only(text: &str) {
    let bytes = text.as_bytes();
    for (idx, b) in bytes.iter().enumerate() {
        if *b == b'\r' {
            assert!(
                bytes.get(idx + 1) == Some(&b'\n'),
                "found bare CR at byte index {idx}"
            );
        }
        if *b == b'\n' {
            assert!(
                idx > 0 && bytes[idx - 1] == b'\r',
                "found bare LF at byte index {idx}"
            );
        }
    }
}

fn assert_cr_only(text: &str) {
    let bytes = text.as_bytes();
    for (idx, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            panic!("found LF at byte index {idx}");
        }
        if *b == b'\r' {
            assert!(
                bytes.get(idx + 1) != Some(&b'\n'),
                "found CRLF at byte index {idx}"
            );
        }
    }
}

fn assert_ast_idempotent(input: &str) {
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());
    let reparsed = parse_java(&formatted);
    let formatted_again = format_java_ast(&reparsed, &formatted, &FormatConfig::default());
    assert_eq!(formatted_again, formatted);
}

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
fn canonical_document_formatting_matches_expected_output() {
    let input = r#"
 class  Foo{
public static void main(String[]args){
System.out.println("hi"); // comment
if(true){System.out.println("x");}
}
}
"#;

    let edits = edits_for_document_formatting(input, &FormatConfig::default());
    let formatted = apply_text_edits(input, &edits).unwrap();

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
fn canonical_document_formatting_is_idempotent_on_generics_fixture() {
    let input = include_str!("fixtures/generics.java");
    let config = FormatConfig::default();
    let edits = edits_for_document_formatting(input, &config);
    let formatted = apply_text_edits(input, &edits).unwrap();

    let edits_again = edits_for_document_formatting(&formatted, &config);
    let formatted_again = apply_text_edits(&formatted, &edits_again).unwrap();

    assert_eq!(formatted_again, formatted);
}

#[test]
fn canonical_document_formatting_is_idempotent_on_broken_fixture() {
    let input = include_str!("fixtures/broken_code.java");
    let config = FormatConfig::default();
    let edits = edits_for_document_formatting(input, &config);
    let formatted = apply_text_edits(input, &edits).unwrap();

    let edits_again = edits_for_document_formatting(&formatted, &config);
    let formatted_again = apply_text_edits(&formatted, &edits_again).unwrap();

    assert_eq!(formatted_again, formatted);
}

#[test]
fn lsp_cli_parity_for_canonical_document_formatting() {
    let input = "class Foo{String s=\"\"\"\nhello\n\"\"\";}\n";
    let config = FormatConfig::default();

    // "LSP": uses the canonical entrypoint directly.
    let lsp_edits = edits_for_document_formatting(input, &config);
    let lsp_formatted = apply_text_edits(input, &lsp_edits).unwrap();

    // "CLI": uses the same formatter but performs additional normalization for deterministic
    // JSON output (e.g. stable edit ordering).
    let mut cli_edits =
        edits_for_document_formatting_with_strategy(input, &config, FormatStrategy::default());
    cli_edits.retain(|edit| {
        let start = u32::from(edit.range.start()) as usize;
        let end = u32::from(edit.range.end()) as usize;
        input
            .get(start..end)
            .map(|slice| slice != edit.replacement)
            .unwrap_or(true)
    });
    cli_edits.sort_by(|a, b| {
        a.range
            .start()
            .cmp(&b.range.start())
            .then_with(|| a.range.end().cmp(&b.range.end()))
            .then_with(|| a.replacement.cmp(&b.replacement))
    });
    let cli_formatted = apply_text_edits(input, &cli_edits).unwrap();

    assert_eq!(lsp_formatted, cli_formatted);
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
    let input =
        "package  foo.bar;\nimport java.util.List;import java.util.Map;\npublic class  Foo{}\n";
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
fn formats_generic_spacing_and_disambiguation() {
    let input = r#"
class Foo{
java.util.List<String>xs;
java.util.Map<String,java.util.List<Integer>>map;
java.util.List<java.util.List<java.util.List<Integer>>>deep;
java.util.List<?extends Number>numbers;
java.util.List<?super Integer>supers;
void m(){
java.util.Collections.<String> emptyList();
java.util.function.Function<String,String>f=this::<String> id;
new java.util.ArrayList<> ();
}
<T> T id(T t){return t;}
}
"#;

    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    // Smoke check: make sure generic closes don't run into declaration identifiers.
    assert!(
        !formatted.contains(">xs"),
        "expected space after generic close: {formatted}"
    );
    assert!(
        !formatted.contains(">>map"),
        "expected space after generic close: {formatted}"
    );
    assert!(
        !formatted.contains(">>>deep"),
        "expected space after generic close: {formatted}"
    );

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    java.util.List<String> xs;
    java.util.Map<String, java.util.List<Integer>> map;
    java.util.List<java.util.List<java.util.List<Integer>>> deep;
    java.util.List<? extends Number> numbers;
    java.util.List<? super Integer> supers;
    void m() {
        java.util.Collections.<String>emptyList();
        java.util.function.Function<String, String> f = this::<String>id;
        new java.util.ArrayList<>();
    }
    <T> T id(T t) {
        return t;
    }
}
"###
    );
}

#[test]
fn formats_varargs_spacing() {
    let input = "class Foo{void m(String...args){}}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    void m(String... args) {
    }
}
"###
    );
}

#[test]
fn formats_parameterized_varargs_spacing() {
    let input = "class Foo{void m(java.util.List<String>...args){}}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    void m(java.util.List<String>... args) {
    }
}
"###
    );
}

#[test]
fn formats_instanceof_pattern_with_generic_type() {
    let input = "class Foo{void m(Object x){if(x instanceof java.util.List<String>xs){}}}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    void m(Object x) {
        if (x instanceof java.util.List<String> xs) {
        }
    }
}
"###
    );
}

#[test]
fn preserves_trailing_line_comment_after_closing_brace_in_ast_formatter() {
    let input = "class Foo{void m(){if(true){} // c\n}}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    void m() {
        if (true) {
        } // c
    }
}
"###
    );
}

#[test]
fn formats_annotated_type_arguments() {
    let input = "class Foo{java.util.List<@Deprecated String>xs;}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    java.util.List<@Deprecated String> xs;
}
"###
    );
}

#[test]
fn ast_formatting_avoids_punctuation_token_merges() {
    let input = "class Foo{void m(){int a=1 / / 2;int b=1 / * 2;int c=1: :2;int d=1- >2;boolean e=1> >2;boolean f=1> >>2;boolean g=1>> >2;boolean h=1> >=2;boolean i=1> =2;boolean j=1= =2;boolean k=1! =2;int l=non - sealed;int m=1+ ++n;int o=1+ +=2;boolean p=true& &&false;boolean q=true& &=false;boolean r=true| ||false;boolean s=true| |=false;boolean t=true< <<false;boolean u=true/ /=false;}}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert!(
        !formatted.contains("//"),
        "formatter should not synthesize line comments: {formatted}"
    );
    assert!(
        !formatted.contains("/*"),
        "formatter should not synthesize block comments: {formatted}"
    );
    assert!(
        !formatted.contains("::"),
        "formatter should not synthesize method reference tokens: {formatted}"
    );
    assert!(
        !formatted.contains("&&&"),
        "formatter should not synthesize `&&&`: {formatted}"
    );
    assert!(
        !formatted.contains("|||"),
        "formatter should not synthesize `|||`: {formatted}"
    );
    assert!(
        !formatted.contains("+++"),
        "formatter should not synthesize `+++`: {formatted}"
    );
    assert!(
        !formatted.contains("++="),
        "formatter should not synthesize `++=`: {formatted}"
    );
    assert!(
        !formatted.contains("<<<<<"),
        "formatter should not synthesize `<<<<<`: {formatted}"
    );
    assert!(
        formatted.contains("1/ /2"),
        "expected `/ /` tokens to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("1/ *2"),
        "expected `/ *` tokens to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("1: :2"),
        "expected `: :` tokens to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("1- >2"),
        "expected `- >` tokens to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("1> >2"),
        "expected `> >` tokens to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("1> >>2"),
        "expected `> >>` tokens to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("1>> >2"),
        "expected `>> >` tokens to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("1> >=2"),
        "expected `> >=` tokens to remain separated: {formatted}"
    );
    assert!(
        !formatted.contains("non-sealed"),
        "formatter should not synthesize `non-sealed`: {formatted}"
    );
    assert!(
        formatted.contains("non -"),
        "expected whitespace before `-` in `non - sealed`: {formatted}"
    );
    assert!(
        formatted.contains("+ ++"),
        "expected `+ ++` to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("+ +="),
        "expected `+ +=` to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("& &&"),
        "expected `& &&` to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("& &="),
        "expected `& &= ` to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("| ||"),
        "expected `| ||` to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("| |="),
        "expected `| |=` to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("< <<"),
        "expected `< <<` to remain separated: {formatted}"
    );
    assert!(
        formatted.contains("/ /="),
        "expected `/ /=` to remain separated: {formatted}"
    );

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    void m() {
        int a = 1/ /2;
        int b = 1/ *2;
        int c = 1: :2;
        int d = 1- >2;
        boolean e = 1> >2;
        boolean f = 1> >>2;
        boolean g = 1>> >2;
        boolean h = 1> >=2;
        boolean i = 1> = 2;
        boolean j = 1 = = 2;
        boolean k = 1! = 2;
        int l = non -sealed;
        int m = 1+ ++n;
        int o = 1+ +=2;
        boolean p = true& &&false;
        boolean q = true& &=false;
        boolean r = true| ||false;
        boolean s = true| |=false;
        boolean t = true< <<false;
        boolean u = true/ /=false;
    }
}
"###
    );
}

#[test]
fn ast_formatting_disambiguates_shift_operators_from_generic_closes() {
    // When formatting `MAX < MIN >> 1`, the `>>` token is a shift operator. If we mistakenly treat
    // `<` as starting generics, we'd format it like a generic close and insert a space (`>> 1`).
    let input = "class Foo{boolean m(int MAX,int MIN){return MAX<MIN>>1;}boolean n(int MAX,int MIN){return MAX<MIN>>>1;}}\n";
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert!(
        !formatted.contains(">> 1"),
        "expected shift operator: {formatted}"
    );
    assert!(
        !formatted.contains(">>> 1"),
        "expected unsigned shift operator: {formatted}"
    );

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    boolean m(int MAX, int MIN) {
        return MAX<MIN>>1;
    }
    boolean n(int MAX, int MIN) {
        return MAX<MIN>>>1;
    }
}
"###
    );
}

#[test]
fn ast_formatting_is_idempotent_on_selected_fixtures() {
    // These fixtures exercise generics-heavy real-world patterns. The AST formatter is still
    // best-effort, but it must be stable across repeated passes.
    let fixtures = [
        include_str!("fixtures/generics.java"),
        include_str!("fixtures/method_reference_type_args.java"),
        include_str!("fixtures/wildcards_and_varargs.java"),
        include_str!("fixtures/qualified_generics.java"),
        include_str!("fixtures/diamond_operator.java"),
    ];

    for fixture in fixtures {
        assert_ast_idempotent(fixture);
    }
}

#[test]
fn ast_formatting_is_idempotent_on_broken_fixture() {
    assert_ast_idempotent(include_str!("fixtures/broken_code.java"));
}

#[test]
fn ast_formatting_is_idempotent_on_unterminated_block_comment() {
    // Unterminated block comments are lexed as a single `Error` token that consumes the remainder
    // of the file. Formatting must preserve the remainder verbatim to avoid changing the comment
    // extent across passes.
    assert_ast_idempotent("class Foo{void m(){int x=1; /* unterminated\n");
}

#[test]
fn ast_formatting_is_idempotent_on_unterminated_string_literal() {
    assert_ast_idempotent("class Foo{void m(){System.out.println(\"unterminated\n");
}

#[test]
fn ast_formatting_is_idempotent_on_unterminated_char_literal() {
    assert_ast_idempotent("class Foo{void m(){char c='x\n");
}

#[test]
fn ast_formatting_is_idempotent_on_unterminated_text_block() {
    assert_ast_idempotent("class Foo{String s=\"\"\"\nhello\n");
}

#[test]
fn ast_formatting_is_idempotent() {
    assert_ast_idempotent(
        r#"
 class  Foo{
public static void main(String[]args){
System.out.println("hi"); // comment
if(true){System.out.println("x");}
}
}
"#,
    );

    assert_ast_idempotent(
        r#"
class Foo{
java.util.List<String>xs;
java.util.Map<String,java.util.List<Integer>>map;
java.util.List<?extends Number>numbers;
void m(){
java.util.Collections.<String> emptyList();
}
}
"#,
    );

    assert_ast_idempotent(
        r#"
class Foo{void m(){int a=1 / / 2;int b=1 / * 2;int c=1: :2;int d=1- >2;boolean e=1> >2;boolean f=1> =2;boolean g=1= =2;boolean h=1! =2;}}
"#,
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
    let input = "class Foo {\n    void a() {\n        foo(1,2,3);\n    }\n}\n";
    let tree = parse(input);
    let index = LineIndex::new(input);

    // Select only the line with multiple comma spacing issues.
    let start = Position::new(2, 0);
    let end_offset = index.line_end(2).unwrap();
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
        "class Foo {\n    void a() {\n        foo(1, 2, 3);\n    }\n}\n"
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

#[test]
fn preserves_crlf_line_endings_for_full_formatting() {
    let input = concat!(
        "class  Foo{\r\n",
        "public static void main(String[]args){\r\n",
        "System.out.println(\"hi\"); // comment\r\n",
        "if(true){System.out.println(\"x\");}\r\n",
        "}\r\n",
        "}\r\n",
    );
    let tree = parse(input);
    let formatted = format_java(&tree, input, &FormatConfig::default());

    assert_crlf_only(&formatted);

    let edits = edits_for_formatting(&tree, input, &FormatConfig::default());
    let out = apply_text_edits(input, &edits).unwrap();
    assert_eq!(out, formatted);
    assert_crlf_only(&out);
}

#[test]
fn preserves_crlf_line_endings_for_range_formatting() {
    let input = "class Foo {\r\n    void a() { int x=1; }\r\n    void b(){int y=2;}\r\n}\r\n";
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

    assert_crlf_only(&out);
    assert_eq!(
        out,
        "class Foo {\r\n    void a() { int x=1; }\r\n    void b() {\r\n        int y = 2;\r\n    }\r\n}\r\n"
    );
}

#[test]
fn preserves_crlf_line_endings_for_full_ast_formatting() {
    let input = concat!(
        "class  Foo{\r\n",
        "public static void main(String[]args){\r\n",
        "System.out.println(\"hi\"); // comment\r\n",
        "if(true){System.out.println(\"x\");}\r\n",
        "}\r\n",
        "}\r\n",
    );
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_crlf_only(&formatted);

    let edits = edits_for_formatting_ast(&parse, input, &FormatConfig::default());
    let out = apply_text_edits(input, &edits).unwrap();
    assert_eq!(out, formatted);
    assert_crlf_only(&out);
}

#[test]
fn preserves_cr_line_endings_for_full_ast_formatting() {
    let input = concat!(
        "class  Foo{\r",
        "public static void main(String[]args){\r",
        "System.out.println(\"hi\"); // comment\r",
        "if(true){System.out.println(\"x\");}\r",
        "}\r",
        "}\r",
    );
    let parse = parse_java(input);
    let formatted = format_java_ast(&parse, input, &FormatConfig::default());

    assert_cr_only(&formatted);

    let edits = edits_for_formatting_ast(&parse, input, &FormatConfig::default());
    let out = apply_text_edits(input, &edits).unwrap();
    assert_eq!(out, formatted);
    assert_cr_only(&out);
}

#[test]
fn pretty_formats_package_and_import_comments() {
    let input = r#"package  foo.bar; // pkg
import java.util.List; // list
// static group comment
import static java.util.Collections.emptyList; // empty
class Foo{}
"#;
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
package foo.bar; // pkg

import java.util.List; // list

// static group comment
import static java.util.Collections.emptyList; // empty

class Foo {
}
"###
    );
}

#[test]
fn pretty_formats_module_declaration() {
    let input = "module  foo.bar{requires  java.base;exports foo.bar.api  to  other.mod;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
module foo.bar {
    requires java.base;
    exports foo.bar.api to other.mod;
}
"###
    );
}

#[test]
fn pretty_formats_top_level_types() {
    let input = r#"class  Foo{}
interface  Bar{}
enum  Color{}
record  Point( int x,int y){}
public @interface  MyAnno{}
"#;
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
}
interface Bar {
}
enum Color {
}
record Point(int x, int y) {
}
public @interface MyAnno {
}
"###
    );
}

#[test]
fn pretty_formats_trivial_class_block() {
    let input = "class Foo{int x;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    int x;
}
"###
    );
}

#[test]
fn pretty_normalizes_header_whitespace() {
    let input = "class  Foo{int x;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(formatted, "class Foo {\n    int x;\n}\n");
}

#[test]
fn pretty_respects_tabs_indentation() {
    let input = "class Foo{int x;}\n";
    let config = FormatConfig {
        indent_style: IndentStyle::Tabs,
        ..FormatConfig::default()
    };
    let edits =
        edits_for_document_formatting_with_strategy(input, &config, FormatStrategy::JavaPrettyAst);
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(formatted, "class Foo {\n\tint x;\n}\n");
}

#[test]
fn pretty_tabs_indentation_does_not_rewrite_text_block_contents() {
    let input = "class Foo{String s = \"\"\"\n    hi\n    \"\"\";}\n";
    let config = FormatConfig {
        indent_style: IndentStyle::Tabs,
        ..FormatConfig::default()
    };
    let edits =
        edits_for_document_formatting_with_strategy(input, &config, FormatStrategy::JavaPrettyAst);
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(
        formatted,
        "class Foo {\n\tString s = \"\"\"\n    hi\n    \"\"\";\n}\n"
    );
}

#[test]
fn pretty_indents_after_existing_newlines_inside_block() {
    let input = "class Foo{int x;\nint y;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(
        formatted, "class Foo {\n    int x;\n    int y;\n}\n",
        "pretty formatter should indent lines that were separated by real newlines"
    );
}

#[test]
fn pretty_formats_doc_comment_before_class() {
    let input = "/**\n   * docs\n   */class Foo{int x;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
/**
 * docs
*/
class Foo {
    int x;
}
"###
    );
}

#[test]
fn pretty_normalizes_doc_comment_inside_class_body() {
    let input = "class Foo{/**\n   * docs\n   */void m() {}}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
    /**
     * docs
    */
    void m() {}
}
"###
    );
}

#[test]
fn pretty_preserves_inline_block_comment_spacing() {
    let input = "/* header */class Foo{int x;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
/* header */ class Foo {
    int x;
}
"###
    );
}

#[test]
fn pretty_inserts_space_after_inline_block_comment_in_body() {
    let input = "class Foo{/* header */int x;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(formatted, "class Foo {\n    /* header */ int x;\n}\n");
}

#[test]
fn pretty_inserts_space_before_trailing_line_comment_in_body() {
    let input = "class Foo{int x;// c\n}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(formatted, "class Foo {\n    int x; // c\n}\n");
}

#[test]
fn pretty_inserts_space_before_trailing_block_comment_in_body() {
    let input = "class Foo{int x;/* c */}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(formatted, "class Foo {\n    int x; /* c */\n}\n");
}

#[test]
fn pretty_does_not_insert_space_before_standalone_line_comment_in_body() {
    let input = "class Foo{// a\n// b\nint x;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(
        formatted,
        "class Foo {\n    // a\n    // b\n    int x;\n}\n"
    );
}

#[test]
fn pretty_preserves_trailing_line_comment_after_class() {
    let input = "class Foo{} // c\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
} // c
"###
    );
}

#[test]
fn pretty_inserts_space_before_trailing_block_comment_after_class() {
    let input = "class Foo{}/* c */\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
class Foo {
} /* c */
"###
    );
}

#[test]
fn pretty_preserves_trailing_line_comment_after_import() {
    let input = "import java.util.List; // c\nclass Foo{int x;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(
        formatted,
        @r###"
import java.util.List; // c

class Foo {
    int x;
}
"###
    );
}

#[test]
fn pretty_indents_multiline_block_comments() {
    let input = "class Foo{/*\n        first\n          second\n        */ int x;}\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_eq!(
        formatted,
        "class Foo {\n    /*\n    first\n      second\n    */ int x;\n}\n"
    );
}

#[test]
fn pretty_preserves_newline_style_and_final_newline() {
    let input = "class Foo{int x;}\r\n";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_crlf_only(&formatted);
    assert_eq!(formatted, "class Foo {\r\n    int x;\r\n}\r\n");

    let input_no_newline = "class Foo{int x;}";
    let edits = edits_for_document_formatting_with_strategy(
        input_no_newline,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input_no_newline, &edits).unwrap();
    assert_eq!(formatted, "class Foo {\n    int x;\n}");
}

#[test]
fn pretty_formats_broken_syntax_without_panicking() {
    let input = "class A{void m(){";
    let edits = edits_for_document_formatting_with_strategy(
        input,
        &FormatConfig::default(),
        FormatStrategy::JavaPrettyAst,
    );
    let formatted = apply_text_edits(input, &edits).unwrap();

    assert_snapshot!(formatted, @"class A{void m(){");
}

#[test]
fn on_type_formatting_preserves_crlf_line_endings() {
    let input = "class Foo {\r\nvoid m(){\r\nint x=1;\r\n}\r\n}\r\n";
    let tree = parse(input);
    let index = LineIndex::new(input);

    let line_end = index.line_end(2).unwrap();
    let position = index.position(input, line_end);

    let edits = edits_for_on_type_formatting(&tree, input, position, ';', &FormatConfig::default())
        .unwrap();
    let out = apply_text_edits(input, &edits).unwrap();

    assert_crlf_only(&out);
    assert_eq!(
        out,
        "class Foo {\r\nvoid m(){\r\n        int x=1;\r\n}\r\n}\r\n"
    );
}

#[test]
fn member_insertion_preserves_requested_newline_style() {
    let out = format_member_insertion_with_newline(
        "    ",
        "private static final int X = 1;",
        true,
        NewlineStyle::CrLf,
    );

    assert_crlf_only(&out);
    assert_eq!(out, "    private static final int X = 1;\r\n\r\n");
}
