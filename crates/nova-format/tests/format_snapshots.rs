use insta::assert_snapshot;
use nova_core::{apply_text_edits, LineIndex, Position, Range};
use nova_format::{edits_for_range_formatting, format_java, FormatConfig};
use nova_syntax::parse;
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

    let tree = parse(input);
    let formatted = format_java(&tree, input, &FormatConfig::default());

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
    let tree = parse(input);
    let formatted = format_java(&tree, input, &FormatConfig::default());

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
    let tree = parse(input);
    let formatted = format_java(&tree, input, &FormatConfig::default());

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
    assert_eq!(edits.len(), 1);
    let out = apply_text_edits(input, &edits).unwrap();

    assert_eq!(
        out,
        "class Foo {\n    void a() { int x=1; }\n    void b() {\n        int y = 2;\n    }\n}\n"
    );
}
