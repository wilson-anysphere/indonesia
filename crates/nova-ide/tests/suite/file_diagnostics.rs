use std::path::PathBuf;

use nova_db::InMemoryFileStore;
use nova_ide::file_diagnostics;
use nova_types::Severity;

fn fixture_file(text: &str) -> (InMemoryFileStore, nova_db::FileId) {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text.to_string());
    (db, file)
}

#[test]
fn file_diagnostics_includes_unresolved_import_code() {
    let (db, file) = fixture_file(
        r#"
import does.not.Exist;
class A {}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected unresolved-import diagnostic; got {diags:#?}"
    );
}

#[test]
fn file_diagnostics_includes_unresolved_import() {
    let (db, file) = fixture_file(
        r#"
import foo.Bar;
class A {}
"#,
    );

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| {
            d.code == "unresolved-import"
                && d.severity == Severity::Error
                && d.message.contains("foo.Bar")
        }),
        "expected unresolved-import diagnostic; got {diags:#?}"
    );
}

#[test]
fn file_diagnostics_include_language_level_feature_gate() {
    use tempfile::TempDir;

    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(root.join("src/main/java")).expect("create source root");

    // Configure a Java language level below records (Java 16).
    std::fs::write(
        root.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>example</artifactId>
  <version>0.1.0</version>
  <properties>
    <maven.compiler.source>11</maven.compiler.source>
    <maven.compiler.target>11</maven.compiler.target>
  </properties>
</project>
"#,
    )
    .expect("write pom.xml");

    let file_path = root.join("src/main/java/Main.java");
    std::fs::write(&file_path, "").expect("touch Main.java");

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&file_path);
    db.set_file_text(file, "record Point(int x, int y) {}\n".to_string());

    let diags = file_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code == "JAVA_FEATURE_RECORDS"),
        "expected JAVA_FEATURE_RECORDS diagnostic; got {diags:#?}"
    );
}
