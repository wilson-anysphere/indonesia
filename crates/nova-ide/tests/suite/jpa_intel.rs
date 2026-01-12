use std::path::PathBuf;

use lsp_types::Position;
use nova_db::InMemoryFileStore;
use nova_ide::{completions, file_diagnostics, goto_definition};

use crate::framework_harness::{offset_to_position, CARET};

fn fixture_multi(
    primary_path: PathBuf,
    primary_text_with_caret: &str,
    extra_files: Vec<(PathBuf, String)>,
) -> (InMemoryFileStore, nova_db::FileId, Position, String) {
    let caret_offset = primary_text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let primary_text = primary_text_with_caret.replace(CARET, "");
    let pos = offset_to_position(&primary_text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let primary_file = db.file_id_for_path(&primary_path);
    db.set_file_text(primary_file, primary_text.clone());
    for (path, text) in extra_files {
        let id = db.file_id_for_path(&path);
        db.set_file_text(id, text);
    }

    (db, primary_file, pos, primary_text)
}

fn user_entity_source() -> String {
    r#"package com.example;
import jakarta.persistence.Entity;
import jakarta.persistence.Id;

@Entity
class User {
  @Id
  Long id;
  String name;
}
"#
    .to_string()
}

fn post_entity_source() -> String {
    r#"package com.example;
import jakarta.persistence.Entity;
import jakarta.persistence.Id;

@Entity
class Post {
  @Id
  Long id;
  String title;
}
"#
    .to_string()
}

#[test]
fn jpa_missing_id_diagnostic_spans_entity_name() {
    let entity_path = PathBuf::from("/workspace/src/main/java/com/example/NoId.java");

    let src = r#"package com.example;
import jakarta.persistence.Entity;

@Entity
class NoId {
  String name;
}
"#;

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&entity_path);
    db.set_file_text(file, src.to_string());

    let diags = file_diagnostics(&db, file);
    let missing_id = diags
        .iter()
        .find(|d| d.code == "JPA_MISSING_ID")
        .expect("expected JPA_MISSING_ID diagnostic");
    let span = missing_id.span.expect("expected diagnostic span");
    assert_eq!(
        &src[span.start..span.end],
        "NoId",
        "expected diagnostic span to cover entity name; got {span:?}"
    );
}

#[test]
fn jpql_completions_include_entity_fields() {
    let user_path = PathBuf::from("/workspace/src/main/java/com/example/User.java");
    let post_path = PathBuf::from("/workspace/src/main/java/com/example/Post.java");
    let repo_path = PathBuf::from("/workspace/src/main/java/com/example/UserRepository.java");

    let repo_text = r#"package com.example;
import org.springframework.data.jpa.repository.Query;

class UserRepository {
  @Query("SELECT u FROM User u WHERE u.<|>")
  void m();
}
"#;

    let (db, file, pos, _repo_text) = fixture_multi(
        repo_path,
        repo_text,
        vec![
            (user_path, user_entity_source()),
            (post_path, post_entity_source()),
        ],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"id"),
        "expected JPQL completion list to contain `id`; got {labels:?}"
    );
    assert!(
        labels.contains(&"name"),
        "expected JPQL completion list to contain `name`; got {labels:?}"
    );
}

#[test]
fn jpql_completions_work_in_text_block() {
    let user_path = PathBuf::from("/workspace/src/main/java/com/example/User.java");
    let post_path = PathBuf::from("/workspace/src/main/java/com/example/Post.java");
    let repo_path = PathBuf::from("/workspace/src/main/java/com/example/UserRepository.java");

    let repo_text = r#"package com.example;
import org.springframework.data.jpa.repository.Query;

class UserRepository {
  @Query("""
    SELECT u FROM User u WHERE u.<|>
    """)
  void m();
}
"#;

    let (db, file, pos, _repo_text) = fixture_multi(
        repo_path,
        repo_text,
        vec![
            (user_path, user_entity_source()),
            (post_path, post_entity_source()),
        ],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"id"),
        "expected JPQL completion list to contain `id`; got {labels:?}"
    );
    assert!(
        labels.contains(&"name"),
        "expected JPQL completion list to contain `name`; got {labels:?}"
    );
}

#[test]
fn jpql_completions_include_entity_names_in_from_clause() {
    let user_path = PathBuf::from("/workspace/src/main/java/com/example/User.java");
    let post_path = PathBuf::from("/workspace/src/main/java/com/example/Post.java");
    let repo_path = PathBuf::from("/workspace/src/main/java/com/example/UserRepository.java");

    let repo_text = r#"package com.example;
import org.springframework.data.jpa.repository.Query;

class UserRepository {
  @Query("SELECT u FROM <|>")
  void m();
}
"#;

    let (db, file, pos, _repo_text) = fixture_multi(
        repo_path,
        repo_text,
        vec![
            (user_path, user_entity_source()),
            (post_path, post_entity_source()),
        ],
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"User"),
        "expected JPQL completion list to contain entity name `User`; got {labels:?}"
    );
}

#[test]
fn jpql_unknown_entity_diagnostic_has_span_on_identifier() {
    let user_path = PathBuf::from("/workspace/src/main/java/com/example/User.java");
    let repo_path = PathBuf::from("/workspace/src/main/java/com/example/UserRepository.java");

    let repo_text = r#"package com.example;
import org.springframework.data.jpa.repository.Query;

class UserRepository {
  @Query("SELECT u FROM Unknown u")
  void m();
  // <|>
}
"#;

    let (db, file, _pos, repo_text) = fixture_multi(
        repo_path,
        repo_text,
        vec![(user_path, user_entity_source())],
    );

    let diags = file_diagnostics(&db, file);
    let unknown = diags
        .iter()
        .find(|d| d.code == "JPQL_UNKNOWN_ENTITY")
        .expect("expected JPQL_UNKNOWN_ENTITY diagnostic");

    let span = unknown.span.expect("expected diagnostic span");
    assert_eq!(
        &repo_text[span.start..span.end],
        "Unknown",
        "expected diagnostic span to cover Unknown identifier; got {span:?}"
    );
}

#[test]
fn jpql_goto_definition_resolves_entity_and_field() {
    let user_path = PathBuf::from("/workspace/src/main/java/com/example/User.java");
    let post_path = PathBuf::from("/workspace/src/main/java/com/example/Post.java");
    let repo_path = PathBuf::from("/workspace/src/main/java/com/example/UserRepository.java");

    let repo_text = r#"package com.example;
import org.springframework.data.jpa.repository.Query;

class UserRepository {
  @Query("SELECT u FROM User u WHERE u.name = :name")
  void m();
  // <|>
}
"#;

    let (db, file, _pos, repo_text) = fixture_multi(
        repo_path,
        repo_text,
        vec![
            (user_path.clone(), user_entity_source()),
            (post_path, post_entity_source()),
        ],
    );

    // 1) Entity navigation: `User`
    let user_offset = repo_text
        .find("FROM User u")
        .expect("expected FROM User u in fixture")
        + "FROM ".len()
        + 1; // inside the identifier
    let user_pos = offset_to_position(&repo_text, user_offset);
    let loc = goto_definition(&db, file, user_pos).expect("expected definition for User");
    assert!(
        loc.uri.as_str().contains("User.java"),
        "expected goto-definition URI to point at User.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 5);
    assert_eq!(loc.range.start.character, 6);

    // 2) Field navigation: `u.name`
    let name_offset = repo_text
        .find("u.name")
        .expect("expected u.name in fixture")
        + "u.".len()
        + 1; // inside the identifier
    let name_pos = offset_to_position(&repo_text, name_offset);
    let loc = goto_definition(&db, file, name_pos).expect("expected definition for u.name");
    assert!(
        loc.uri.as_str().contains("User.java"),
        "expected goto-definition URI to point at User.java; got {:?}",
        loc.uri
    );
    assert_eq!(loc.range.start.line, 8);
    assert_eq!(loc.range.start.character, 9);
}

#[test]
fn jpql_goto_definition_resolves_in_text_block() {
    let user_path = PathBuf::from("/workspace/src/main/java/com/example/User.java");
    let post_path = PathBuf::from("/workspace/src/main/java/com/example/Post.java");
    let repo_path = PathBuf::from("/workspace/src/main/java/com/example/UserRepository.java");

    let repo_text = r#"package com.example;
import org.springframework.data.jpa.repository.Query;

class UserRepository {
  @Query("""
    SELECT u FROM User u WHERE u.name = :name
    """)
  void m();
  // <|>
}
"#;

    let (db, file, _pos, repo_text) = fixture_multi(
        repo_path,
        repo_text,
        vec![
            (user_path.clone(), user_entity_source()),
            (post_path, post_entity_source()),
        ],
    );

    let name_offset = repo_text
        .find("u.name")
        .expect("expected u.name in fixture")
        + "u.".len()
        + 1;
    let name_pos = offset_to_position(&repo_text, name_offset);
    let loc = goto_definition(&db, file, name_pos).expect("expected definition for u.name");
    assert!(loc.uri.as_str().contains("User.java"));
    assert_eq!(loc.range.start.line, 8);
    assert_eq!(loc.range.start.character, 9);
}
