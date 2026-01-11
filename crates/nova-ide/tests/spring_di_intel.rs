use std::path::PathBuf;

use nova_db::RootDatabase;
use nova_framework_spring::{SPRING_AMBIGUOUS_BEAN, SPRING_NO_BEAN};
use nova_ide::{completions, file_diagnostics, goto_definition};
use nova_types::Severity;

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let mut line = 0u32;
    let mut col = 0u32;
    for (idx, ch) in text.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    lsp_types::Position::new(line, col)
}

fn fixture_multi(
    primary_path: PathBuf,
    primary_text_with_caret: &str,
    extra_files: Vec<(PathBuf, String)>,
) -> (RootDatabase, nova_db::FileId, lsp_types::Position) {
    let caret = "<|>";
    let caret_offset = primary_text_with_caret
        .find(caret)
        .expect("fixture must contain <|> caret marker");
    let primary_text = primary_text_with_caret.replace(caret, "");
    let pos = offset_to_position(&primary_text, caret_offset);

    let mut db = RootDatabase::new();
    let primary_file = db.file_id_for_path(&primary_path);
    db.set_file_text(primary_file, primary_text);
    for (path, text) in extra_files {
        let id = db.file_id_for_path(&path);
        db.set_file_text(id, text);
    }

    (db, primary_file, pos)
}

#[test]
fn spring_di_diagnostics_report_missing_bean() {
    let java_path = PathBuf::from("/spring-missing/src/main/java/A.java");
    let java_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

@Component
class A {
  @Autowired Missing missing;
}

class Missing {}
"#;

    let (db, file, _) = fixture_multi(java_path, java_text, vec![]);
    let diags = file_diagnostics(&db, file);

    assert!(
        diags
            .iter()
            .any(|d| d.code == SPRING_NO_BEAN && d.severity == Severity::Error),
        "expected missing-bean diagnostic; got {diags:#?}"
    );
}

#[test]
fn spring_di_diagnostics_report_ambiguous_beans() {
    let consumer_path = PathBuf::from("/spring-ambiguous/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

interface Foo {}

@Component
class Consumer {
  @Autowired Foo foo;
}
"#;

    let foo_impl_1 = (
        PathBuf::from("/spring-ambiguous/src/main/java/FooImpl1.java"),
        r#"import org.springframework.stereotype.Component;

@Component
class FooImpl1 implements Foo {}
"#
        .to_string(),
    );

    let foo_impl_2 = (
        PathBuf::from("/spring-ambiguous/src/main/java/FooImpl2.java"),
        r#"import org.springframework.stereotype.Component;

@Component
class FooImpl2 implements Foo {}
"#
        .to_string(),
    );

    let (db, file, _) = fixture_multi(consumer_path, consumer_text, vec![foo_impl_1, foo_impl_2]);
    let diags = file_diagnostics(&db, file);

    assert!(
        diags.iter().any(|d| d.code == SPRING_AMBIGUOUS_BEAN),
        "expected ambiguous-bean diagnostic; got {diags:#?}"
    );
}

#[test]
fn spring_qualifier_completion_returns_bean_names() {
    let consumer_path = PathBuf::from("/spring-qualifier/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Qualifier;
import org.springframework.stereotype.Component;

interface Foo {}

@Component
class Consumer {
  @Autowired @Qualifier("<|>") Foo foo;
}
"#;

    let foo_impl_1 = (
        PathBuf::from("/spring-qualifier/src/main/java/FooImpl1.java"),
        r#"import org.springframework.stereotype.Component;

@Component
class FooImpl1 implements Foo {}
"#
        .to_string(),
    );

    let foo_impl_2 = (
        PathBuf::from("/spring-qualifier/src/main/java/FooImpl2.java"),
        r#"import org.springframework.stereotype.Component;

@Component
class FooImpl2 implements Foo {}
"#
        .to_string(),
    );

    let (db, file, pos) = fixture_multi(consumer_path, consumer_text, vec![foo_impl_1, foo_impl_2]);
    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"fooImpl1") && labels.contains(&"fooImpl2"),
        "expected qualifier completions to include bean names; got {labels:?}"
    );
}

#[test]
fn spring_goto_definition_from_injection_jumps_to_component() {
    let consumer_path = PathBuf::from("/spring-nav/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

@Component
class Consumer {
  @Autowired FooService foo<|>Service;
}
"#;

    let bean_path = PathBuf::from("/spring-nav/src/main/java/FooService.java");
    let bean_text = r#"import org.springframework.stereotype.Component;
@Component
class FooService {}
"#;

    let (db, file, pos) = fixture_multi(
        consumer_path,
        consumer_text,
        vec![(bean_path, bean_text.to_string())],
    );

    let loc = goto_definition(&db, file, pos).expect("expected bean definition location");
    assert!(
        loc.uri.as_str().contains("FooService.java"),
        "expected definition URI to point at FooService; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 2);
    assert_eq!(loc.range.start.character, 6);
}
