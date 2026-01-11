use nova_refactor::{
    apply_text_edits, organize_imports, FileId, InMemoryJavaDatabase, OrganizeImportsParams,
};
use pretty_assertions::assert_eq;

fn apply_organize_imports(src: &str) -> (String, nova_refactor::WorkspaceEdit) {
    let file = FileId::new("Test.java");
    let db = InMemoryJavaDatabase::new([(file.clone(), src.to_string())]);
    let edit =
        organize_imports(&db, OrganizeImportsParams { file }).expect("organize_imports runs");
    let after = apply_text_edits(src, &edit.text_edits).expect("apply edits");
    (after, edit)
}

fn assert_idempotent(src: &str, expected: &str) {
    let (after, edit) = apply_organize_imports(src);
    assert_eq!(after, expected);
    // A second pass should result in no changes (stable output).
    let (after2, edit2) = apply_organize_imports(&after);
    assert_eq!(after2, after);
    assert!(
        edit2.is_empty(),
        "second pass should be a no-op, got edit: {edit2:?} (first pass was: {edit:?})"
    );
}

#[test]
fn license_header_package_imports() {
    let before = r#"/*
 * Copyright (c) Example.
 */
package com.example;

import java.util.List;
import java.util.ArrayList;

public class Test {
  List<String> xs = new ArrayList<>();
}
"#;

    let expected = r#"/*
 * Copyright (c) Example.
 */
package com.example;

import java.util.ArrayList;
import java.util.List;

public class Test {
  List<String> xs = new ArrayList<>();
}
"#;

    assert_idempotent(before, expected);
}

#[test]
fn comments_between_imports() {
    let before = r#"package p;

import b.B;
// comment between
import a.A;

class Test {
  A a;
  B b;
}
"#;

    let expected = r#"package p;

import a.A;
import b.B;

class Test {
  A a;
  B b;
}
"#;

    assert_idempotent(before, expected);
}

#[test]
fn static_and_normal_imports_are_grouped_and_sorted() {
    let before = r#"import static java.util.Collections.emptyList;
import java.util.List;
import static java.util.Collections.singletonList;
import java.util.ArrayList;

class Test {
  List<String> xs = emptyList();
  List<String> ys = singletonList("x");
  ArrayList<String> zs = new ArrayList<>();
}
"#;

    let expected = r#"import java.util.ArrayList;
import java.util.List;

import static java.util.Collections.emptyList;
import static java.util.Collections.singletonList;

class Test {
  List<String> xs = emptyList();
  List<String> ys = singletonList("x");
  ArrayList<String> zs = new ArrayList<>();
}
"#;

    assert_idempotent(before, expected);
}

#[test]
fn preserves_trailing_comments_on_import_lines() {
    let before = r#"import b.B; // b
import a.A; // a

class Test {
  A a;
  B b;
}
"#;

    let expected = r#"import a.A; // a
import b.B; // b

class Test {
  A a;
  B b;
}
"#;

    assert_idempotent(before, expected);
}

#[test]
fn import_in_string_or_comment_is_not_treated_as_import() {
    let before = r#"package p;

/* import foo.Bar; */

class Test {
  String s = "import baz.Qux;";
}
"#;

    // No real imports -> no changes.
    let (after, edit) = apply_organize_imports(before);
    assert_eq!(after, before);
    assert!(edit.text_edits.is_empty());
}
