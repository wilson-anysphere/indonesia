use std::collections::BTreeMap;
use std::path::PathBuf;

use nova_refactor::extract_method::{ExtractMethod, InsertionStrategy, Visibility};
use nova_refactor::{
    apply_workspace_edit, extract_constant, extract_variable, inline_variable,
    move_class_workspace_edit, organize_imports, rename, ExtractOptions, ExtractVariableParams,
    FileId, InlineVariableParams, MoveClassParams, OrganizeImportsParams, RefactorJavaDatabase,
    RenameParams, TextRange,
};
use nova_test_utils::javac::{javac_available, run_javac_files};

fn assert_javac_ok(files: &BTreeMap<FileId, String>, label: &str) {
    let owned: Vec<(String, String)> = files
        .iter()
        .map(|(file, src)| (file.0.clone(), src.clone()))
        .collect();
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(file, src)| (file.as_str(), src.as_str()))
        .collect();

    let out = run_javac_files(&refs).expect("failed to invoke javac");
    assert!(
        out.success(),
        "javac failed ({label})\nstdout:\n{}\nstderr:\n{}",
        out.stdout,
        out.stderr
    );
}

#[test]
#[ignore]
fn javac_refactor_rename_local_variable_compiles_before_after() {
    if !javac_available() {
        eprintln!("javac not available; skipping test");
        return;
    }

    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int foo = 1;
    System.out.println(foo);
  }
}
"#;

    let files = BTreeMap::from([(file.clone(), src.to_string())]);
    assert_javac_ok(&files, "before rename");

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("foo = 1").expect("foo declaration");
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "bar".into(),
        },
    )
    .expect("rename should succeed");

    let updated = apply_workspace_edit(&files, &edit).expect("apply workspace edit");
    assert_javac_ok(&updated, "after rename");
}

#[test]
#[ignore]
fn javac_refactor_extract_constant_compiles_before_after() {
    if !javac_available() {
        eprintln!("javac not available; skipping test");
        return;
    }

    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
    System.out.println(x);
  }
}
"#;

    let files = BTreeMap::from([(file.clone(), src.to_string())]);
    assert_javac_ok(&files, "before extract constant");

    let start = src.find("1 + 2").expect("selection");
    let end = start + "1 + 2".len();
    let selection = TextRange::new(start, end);

    let outcome = extract_constant(
        &file.0,
        src,
        selection,
        ExtractOptions {
            name: Some("SUM".into()),
            replace_all: false,
        },
    )
    .expect("extract constant should succeed");

    let updated = apply_workspace_edit(&files, &outcome.edit).expect("apply workspace edit");
    assert_javac_ok(&updated, "after extract constant");
}

#[test]
#[ignore]
fn javac_refactor_extract_method_compiles_before_after() {
    if !javac_available() {
        eprintln!("javac not available; skipping test");
        return;
    }

    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
    System.out.println(x);
  }
}
"#;

    let files = BTreeMap::from([(file.clone(), src.to_string())]);
    assert_javac_ok(&files, "before extract method");

    let stmt = "System.out.println(x);";
    let start = src.find(stmt).expect("statement selection");
    let end = start + stmt.len();

    let extractor = ExtractMethod {
        file: file.0.clone(),
        selection: TextRange::new(start, end),
        name: "extracted".into(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let edit = extractor.apply(src).expect("extract method should succeed");
    let updated = apply_workspace_edit(&files, &edit).expect("apply workspace edit");
    assert_javac_ok(&updated, "after extract method");
}

#[test]
#[ignore]
fn javac_refactor_move_class_compiles_before_after() {
    if !javac_available() {
        eprintln!("javac not available; skipping test");
        return;
    }

    let a_path = PathBuf::from("src/main/java/com/example/A.java");
    let b_path = PathBuf::from("src/main/java/com/example/B.java");

    let a_src = r#"package com.example;

public class A {
  public static int value() {
    return 1;
  }
}
"#;
    let b_src = r#"package com.example;

public class B {
  public int v = A.value();
}
"#;

    let path_files: BTreeMap<PathBuf, String> = BTreeMap::from([
        (a_path.clone(), a_src.to_string()),
        (b_path.clone(), b_src.to_string()),
    ]);

    let workspace_files: BTreeMap<FileId, String> = path_files
        .iter()
        .map(|(path, src)| {
            (
                FileId::new(path.to_string_lossy().into_owned()),
                src.clone(),
            )
        })
        .collect();

    assert_javac_ok(&workspace_files, "before move class");

    let edit = move_class_workspace_edit(
        &path_files,
        MoveClassParams {
            source_path: a_path.clone(),
            class_name: "A".into(),
            target_package: "com.other".into(),
        },
    )
    .expect("move class should succeed");

    let updated = apply_workspace_edit(&workspace_files, &edit).expect("apply workspace edit");
    assert_javac_ok(&updated, "after move class");
}

#[test]
#[ignore]
fn javac_refactor_extract_variable_compiles_before_after() {
    if !javac_available() {
        eprintln!("javac not available; skipping test");
        return;
    }

    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int x = 1 + 2;
    System.out.println(x);
  }
}
"#;

    let files = BTreeMap::from([(file.clone(), src.to_string())]);
    assert_javac_ok(&files, "before extract variable");

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let start = src.find("1 + 2").expect("selection");
    let end = start + "1 + 2".len();

    let edit = extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range: TextRange::new(start, end),
            name: "sum".into(),
            // Use an explicit type rather than `var` so this stays compatible with older JDKs.
            use_var: false,
            replace_all: false,
        },
    )
    .expect("extract variable should succeed");

    let updated = apply_workspace_edit(&files, &edit).expect("apply workspace edit");
    assert_javac_ok(&updated, "after extract variable");
}

#[test]
#[ignore]
fn javac_refactor_inline_variable_compiles_before_after() {
    if !javac_available() {
        eprintln!("javac not available; skipping test");
        return;
    }

    let file = FileId::new("Test.java");
    let src = r#"class Test {
  void m() {
    int foo = 1 + 2;
    int x = foo * 3;
    System.out.println(x);
  }
}
"#;

    let files = BTreeMap::from([(file.clone(), src.to_string())]);
    assert_javac_ok(&files, "before inline variable");

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let offset = src.find("foo = 1 + 2").expect("foo declaration");
    let symbol = db.symbol_at(&file, offset).expect("symbol at foo");

    let edit = inline_variable(
        &db,
        InlineVariableParams {
            symbol,
            inline_all: true,
            usage_range: None,
        },
    )
    .expect("inline variable should succeed");

    let updated = apply_workspace_edit(&files, &edit).expect("apply workspace edit");
    assert_javac_ok(&updated, "after inline variable");
}

#[test]
#[ignore]
fn javac_refactor_rename_type_compiles_before_after() {
    if !javac_available() {
        eprintln!("javac not available; skipping test");
        return;
    }

    let foo_file = FileId::new("Foo.java");
    let use_file = FileId::new("UseFoo.java");

    let foo_src = r#"class Foo {
  Foo() {}

  int value() {
    return 1;
  }
}
"#;

    let use_src = r#"class UseFoo {
  int v() {
    return new Foo().value();
  }
}
"#;

    let files = BTreeMap::from([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);
    assert_javac_ok(&files, "before rename type");

    let db = RefactorJavaDatabase::new([
        (foo_file.clone(), foo_src.to_string()),
        (use_file.clone(), use_src.to_string()),
    ]);
    let offset = foo_src.find("class Foo").expect("class declaration") + "class ".len();
    let symbol = db.symbol_at(&foo_file, offset).expect("symbol at Foo");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Baz".into(),
        },
    )
    .expect("rename should succeed");

    let updated = apply_workspace_edit(&files, &edit).expect("apply workspace edit");
    assert_javac_ok(&updated, "after rename type");
}

#[test]
#[ignore]
fn javac_refactor_organize_imports_compiles_before_after() {
    if !javac_available() {
        eprintln!("javac not available; skipping test");
        return;
    }

    let file = FileId::new("Test.java");
    let src = r#"import java.util.HashMap;
import java.util.List;
import java.util.ArrayList;

class Test {
  List<String> xs = new ArrayList<>();
}
"#;

    let files = BTreeMap::from([(file.clone(), src.to_string())]);
    assert_javac_ok(&files, "before organize imports");

    let db = RefactorJavaDatabase::new([(file.clone(), src.to_string())]);
    let edit = organize_imports(&db, OrganizeImportsParams { file: file.clone() })
        .expect("organize imports should succeed");

    let updated = apply_workspace_edit(&files, &edit).expect("apply workspace edit");
    assert_javac_ok(&updated, "after organize imports");
}
