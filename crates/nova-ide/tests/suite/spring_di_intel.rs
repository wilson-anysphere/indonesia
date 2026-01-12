use std::path::PathBuf;

use nova_db::InMemoryFileStore;
use nova_framework_spring::{SPRING_AMBIGUOUS_BEAN, SPRING_CIRCULAR_DEP, SPRING_NO_BEAN};
use nova_ide::{completions, file_diagnostics, find_references, goto_definition};
use nova_types::Severity;

use crate::framework_harness::{offset_to_position, CARET};

fn fixture_multi(
    primary_path: PathBuf,
    primary_text_with_caret: &str,
    extra_files: Vec<(PathBuf, String)>,
) -> (InMemoryFileStore, nova_db::FileId, lsp_types::Position) {
    let (primary_text, pos) = match primary_text_with_caret.find(CARET) {
        Some(caret_offset) => {
            let primary_text = primary_text_with_caret.replace(CARET, "");
            let pos = offset_to_position(&primary_text, caret_offset);
            (primary_text, pos)
        }
        None => (
            primary_text_with_caret.to_string(),
            lsp_types::Position::new(0, 0),
        ),
    };

    let mut db = InMemoryFileStore::new();
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
<|>
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
<|>
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
fn spring_qualifier_completion_includes_explicit_qualifier_values() {
    let consumer_path = PathBuf::from("/spring-qualifier-explicit/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Qualifier;
import org.springframework.stereotype.Component;

interface Foo {}

@Component
class Consumer {
  @Autowired @Qualifier("<|>") Foo foo;
}
"#;

    let foo_impl = (
        PathBuf::from("/spring-qualifier-explicit/src/main/java/FooImpl.java"),
        r#"import org.springframework.beans.factory.annotation.Qualifier;
import org.springframework.stereotype.Component;

@Component
@Qualifier("specialFoo")
class FooImpl implements Foo {}
"#
        .to_string(),
    );

    let (db, file, pos) = fixture_multi(consumer_path, consumer_text, vec![foo_impl]);
    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"specialFoo"),
        "expected qualifier completions to include explicit qualifier values; got {labels:?}"
    );
}

#[test]
fn spring_named_completion_returns_bean_names() {
    let consumer_path = PathBuf::from("/spring-named/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
 import org.springframework.stereotype.Component;
 import javax.inject.Named;
 
 interface Foo {}
 
 @Component
 class Consumer {
   @Autowired @Named("<|>") Foo foo;
 }
 "#;

    let foo_impl_1 = (
        PathBuf::from("/spring-named/src/main/java/FooImpl1.java"),
        r#"import org.springframework.stereotype.Component;
 
 @Component
 class FooImpl1 implements Foo {}
 "#
        .to_string(),
    );

    let foo_impl_2 = (
        PathBuf::from("/spring-named/src/main/java/FooImpl2.java"),
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
        "expected @Named completions to include bean names; got {labels:?}"
    );
}

#[test]
fn spring_goto_definition_from_named_string_jumps_to_matching_bean() {
    let consumer_path = PathBuf::from("/spring-named-nav/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
 import org.springframework.stereotype.Component;
 import javax.inject.Named;
 
 interface Foo {}
 
 @Component
 class Consumer {
   @Autowired @Named("specialFo<|>o") Foo foo;
 }
 "#;

    let bean_path = PathBuf::from("/spring-named-nav/src/main/java/FooImpl.java");
    let bean_text = r#"import org.springframework.stereotype.Component;
 import javax.inject.Named;
 
 @Component
 @Named("specialFoo")
 class FooImpl implements Foo {}
 "#;

    let (db, file, pos) = fixture_multi(
        consumer_path,
        consumer_text,
        vec![(bean_path, bean_text.to_string())],
    );

    let loc = goto_definition(&db, file, pos).expect("expected bean definition location");
    assert!(
        loc.uri.as_str().contains("FooImpl.java"),
        "expected definition URI to point at FooImpl; got {:?}",
        loc.uri
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

#[test]
fn spring_goto_definition_from_injection_type_jumps_to_component() {
    let consumer_path = PathBuf::from("/spring-nav-type/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

@Component
class Consumer {
  @Autowired Foo<|>Service fooService;
}
"#;

    let bean_path = PathBuf::from("/spring-nav-type/src/main/java/FooService.java");
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

#[test]
fn spring_goto_definition_returns_none_for_ambiguous_injection() {
    let consumer_path = PathBuf::from("/spring-nav-ambiguous/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

interface Foo {}

@Component
class Consumer {
  @Autowired Foo fo<|>o;
}
"#;

    let foo_impl_1 = (
        PathBuf::from("/spring-nav-ambiguous/src/main/java/FooImpl1.java"),
        r#"import org.springframework.stereotype.Component;

@Component
class FooImpl1 implements Foo {}
"#
        .to_string(),
    );

    let foo_impl_2 = (
        PathBuf::from("/spring-nav-ambiguous/src/main/java/FooImpl2.java"),
        r#"import org.springframework.stereotype.Component;

@Component
class FooImpl2 implements Foo {}
"#
        .to_string(),
    );

    let (db, file, pos) = fixture_multi(consumer_path, consumer_text, vec![foo_impl_1, foo_impl_2]);

    let loc = goto_definition(&db, file, pos);
    assert!(loc.is_none(), "expected no goto-definition; got {loc:?}");
}

#[test]
fn spring_goto_definition_from_qualifier_string_jumps_to_matching_bean() {
    let consumer_path = PathBuf::from("/spring-nav-qualifier/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Qualifier;
import org.springframework.stereotype.Component;

interface Foo {}

@Component
class Consumer {
  @Autowired @Qualifier("special<|>Foo") Foo foo;
}
"#;

    let bean_path = PathBuf::from("/spring-nav-qualifier/src/main/java/FooImpl.java");
    let bean_text = r#"import org.springframework.beans.factory.annotation.Qualifier;
import org.springframework.stereotype.Component;

@Component
@Qualifier("specialFoo")
class FooImpl implements Foo {}
"#;

    let (db, file, pos) = fixture_multi(
        consumer_path,
        consumer_text,
        vec![(bean_path, bean_text.to_string())],
    );

    let loc = goto_definition(&db, file, pos).expect("expected qualifier goto-definition");
    assert!(
        loc.uri.as_str().contains("FooImpl.java"),
        "expected definition URI to point at FooImpl; got {:?}",
        loc.uri
    );
}

#[test]
fn spring_profile_completion_returns_profiles() {
    let java_path = PathBuf::from("/spring-profile/src/main/java/A.java");
    let java_text = r#"import org.springframework.context.annotation.Profile;
import org.springframework.stereotype.Component;

@Profile("<|>")
@Component
class A {}
"#;

    let (db, file, pos) = fixture_multi(java_path, java_text, vec![]);
    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"dev") && labels.contains(&"test") && labels.contains(&"prod"),
        "expected profile completions; got {labels:?}"
    );
}

#[test]
fn spring_profile_completion_discovers_profiles_from_filenames() {
    let config_path =
        PathBuf::from("/spring-profile-discover/src/main/resources/application-staging.properties");
    let java_path = PathBuf::from("/spring-profile-discover/src/main/java/A.java");

    let config_text = "server.port=8080\n".to_string();
    let java_text = r#"import org.springframework.context.annotation.Profile;
import org.springframework.stereotype.Component;

@Profile("<|>")
@Component
class A {}
"#;

    let (db, file, pos) = fixture_multi(java_path, java_text, vec![(config_path, config_text)]);

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"staging"),
        "expected discovered profile completion; got {labels:?}"
    );
}

#[test]
fn spring_find_references_from_bean_definition_to_injection_site() {
    let bean_path = PathBuf::from("/spring-refs/src/main/java/FooService.java");
    let bean_text = r#"import org.springframework.stereotype.Component;

@Component
class Foo<|>Service {}
"#;

    let consumer_path = PathBuf::from("/spring-refs/src/main/java/Consumer.java");
    let consumer_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

@Component
class Consumer {
  @Autowired FooService fooService;
}
"#;

    let (db, file, pos) = fixture_multi(
        bean_path,
        bean_text,
        vec![(consumer_path, consumer_text.to_string())],
    );

    let refs = find_references(&db, file, pos, false);
    assert_eq!(refs.len(), 1);
    assert!(
        refs[0].uri.as_str().contains("Consumer.java"),
        "expected reference to point at Consumer.java; got {:?}",
        refs[0].uri
    );
}

#[test]
fn spring_di_diagnostics_report_circular_dependency() {
    let java_path = PathBuf::from("/spring-cycle/src/main/java/Cycle.java");
    let java_text = r#"import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

@Component
class A {
  @Autowired B b;
}

@Component
class B {
  @Autowired A a;
}
<|>
"#;

    let (db, file, _) = fixture_multi(java_path, java_text, vec![]);
    let diags = file_diagnostics(&db, file);
    assert!(
        diags
            .iter()
            .any(|d| d.code == SPRING_CIRCULAR_DEP && d.severity == Severity::Warning),
        "expected circular-dependency diagnostic; got {diags:#?}"
    );
}
