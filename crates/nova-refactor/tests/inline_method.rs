use std::collections::BTreeMap;

use nova_refactor::{apply_edits, inline_method, InlineMethodError, InlineMethodOptions};
use pretty_assertions::assert_eq;

fn extract_caret(input: &str) -> (String, usize) {
    let marker = "/*caret*/";
    let idx = input.find(marker).expect("caret marker not found");
    let mut out = input.to_string();
    out.replace_range(idx..idx + marker.len(), "");
    (out, idx)
}

#[test]
fn inline_expression_bodied_method() {
    let (src, caret) = extract_caret(
        r#"class A {
  private int addOne(int x) { return x + 1; }

  int test() {
    return /*caret*/addOne(41);
  }
}
"#,
    );

    let edits = inline_method("A.java", &src, caret, InlineMethodOptions { inline_all: false }).unwrap();
    let mut files = BTreeMap::new();
    files.insert("A.java".to_string(), src);

    let updated = apply_edits(&files, &edits);
    assert_eq!(
        updated.get("A.java").unwrap(),
        r#"class A {
  private int addOne(int x) { return x + 1; }

  int test() {
    int x_arg = 41;
    return x_arg + 1;
  }
}
"#
    );
}

#[test]
fn inline_method_with_local_temp_renames_to_avoid_collision() {
    let (src, caret) = extract_caret(
        r#"class A {
  private int inc(int x) {
    int tmp = x + 1;
    return tmp;
  }

  int test() {
    int tmp = 10;
    return /*caret*/inc(tmp);
  }
}
"#,
    );

    let edits = inline_method("A.java", &src, caret, InlineMethodOptions::default()).unwrap();
    let mut files = BTreeMap::new();
    files.insert("A.java".to_string(), src);

    let updated = apply_edits(&files, &edits);
    assert_eq!(
        updated.get("A.java").unwrap(),
        r#"class A {
  private int inc(int x) {
    int tmp = x + 1;
    return tmp;
  }

  int test() {
    int tmp = 10;
    int x_arg = tmp;
    int tmp_inlined = x_arg + 1;
    return tmp_inlined;
  }
}
"#
    );
}

#[test]
fn rejects_recursive_method() {
    let (src, caret) = extract_caret(
        r#"class A {
  private int foo(int x) {
    return foo(x - 1);
  }

  int test() {
    return /*caret*/foo(1);
  }
}
"#,
    );

    let err = inline_method("A.java", &src, caret, InlineMethodOptions::default()).unwrap_err();
    assert_eq!(err, InlineMethodError::RecursiveMethod);
}

#[test]
fn preserves_argument_evaluation_order_with_parameter_temps() {
    let (src, caret) = extract_caret(
        r#"class A {
  private int swap(int a, int b) {
    return b + a;
  }

  int test() {
    return /*caret*/swap(f(), g());
  }

  int f() { return 1; }
  int g() { return 2; }
}
"#,
    );

    let edits = inline_method("A.java", &src, caret, InlineMethodOptions::default()).unwrap();
    let mut files = BTreeMap::new();
    files.insert("A.java".to_string(), src);

    let updated = apply_edits(&files, &edits);
    assert_eq!(
        updated.get("A.java").unwrap(),
        r#"class A {
  private int swap(int a, int b) {
    return b + a;
  }

  int test() {
    int a_arg = f();
    int b_arg = g();
    return b_arg + a_arg;
  }

  int f() { return 1; }
  int g() { return 2; }
}
"#
    );
}

