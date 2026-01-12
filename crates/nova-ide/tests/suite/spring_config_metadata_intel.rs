use std::io::Write;
use std::path::PathBuf;

use nova_db::InMemoryFileStore;
use nova_framework_spring::SPRING_UNKNOWN_CONFIG_KEY;
use nova_ide::{completions, file_diagnostics};
use tempfile::tempdir;
use zip::write::FileOptions;

use crate::framework_harness::{offset_to_position, CARET};

fn maven_repo_root() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(".m2/repository"))
        .unwrap_or_else(|| PathBuf::from(".m2/repository"))
}

fn write_metadata_jar(group: &str, artifact: &str, version: &str, metadata_json: &str) -> PathBuf {
    let group_path = group.split('.').collect::<Vec<_>>();
    let mut jar_path = maven_repo_root();
    for seg in group_path {
        jar_path = jar_path.join(seg);
    }
    jar_path = jar_path
        .join(artifact)
        .join(version)
        .join(format!("{artifact}-{version}.jar"));

    std::fs::create_dir_all(jar_path.parent().unwrap()).unwrap();

    let mut jar = zip::ZipWriter::new(std::fs::File::create(&jar_path).unwrap());
    jar.start_file(
        "META-INF/spring-configuration-metadata.json",
        FileOptions::<()>::default(),
    )
    .unwrap();
    write!(jar, "{metadata_json}").unwrap();
    jar.finish().unwrap();

    jar_path
}

fn fixture(
    path: PathBuf,
    text_with_caret: &str,
    extra_files: Vec<(PathBuf, String)>,
) -> (InMemoryFileStore, nova_db::FileId, lsp_types::Position) {
    let caret_offset = text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(CARET, "");
    let pos = offset_to_position(&text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text);
    for (path, text) in extra_files {
        let id = db.file_id_for_path(&path);
        db.set_file_text(id, text);
    }

    (db, file, pos)
}

#[test]
fn spring_config_intelligence_uses_spring_configuration_metadata() {
    let group_id = "com.example";
    let artifact_id = "test-metadata-intel";
    let version = "1.0.0";

    let metadata_json = r#"{
  "properties": [
    { "name": "server.port", "type": "java.lang.Integer" },
    { "name": "spring.main.banner-mode", "type": "java.lang.String" }
  ],
  "hints": [
    { "name": "spring.main.banner-mode", "values": [
      { "value": "off" },
      { "value": "console" }
    ] }
  ]
}"#;

    let _jar_path = write_metadata_jar(group_id, artifact_id, version, metadata_json);

    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("pom.xml"),
        format!(
            r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>{group_id}</groupId>
      <artifactId>{artifact_id}</artifactId>
      <version>{version}</version>
    </dependency>
  </dependencies>
</project>"#
        ),
    )
    .unwrap();

    let config_path = dir.path().join("src/main/resources/application.properties");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();

    let yaml_path = dir.path().join("src/main/resources/application.yml");
    std::fs::create_dir_all(yaml_path.parent().unwrap()).unwrap();

    let java_path = dir.path().join("src/main/java/C.java");
    std::fs::create_dir_all(java_path.parent().unwrap()).unwrap();

    // Ensure the real filesystem paths exist so root discovery prefers the build-marker logic.
    std::fs::write(&config_path, "").unwrap();
    std::fs::write(&yaml_path, "").unwrap();
    std::fs::write(&java_path, "").unwrap();

    // Diagnostics: known key should not be flagged as unknown.
    let mut db = InMemoryFileStore::new();
    let config_file = db.file_id_for_path(&config_path);
    db.set_file_text(config_file, "server.port=8080\n".to_string());

    let diags = file_diagnostics(&db, config_file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == SPRING_UNKNOWN_CONFIG_KEY),
        "expected no unknown-key diagnostics for server.port; got {diags:#?}"
    );

    // Diagnostics: unknown key should be flagged when metadata is available.
    db.set_file_text(config_file, "unknown.key=foo\n".to_string());
    let diags = file_diagnostics(&db, config_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == SPRING_UNKNOWN_CONFIG_KEY),
        "expected unknown-key diagnostics for unknown.key; got {diags:#?}"
    );

    // Completions in application.properties should be metadata-backed (no observed keys required).
    let (db, config_file, pos) = fixture(config_path.clone(), "serv<|>", vec![]);
    let items = completions(&db, config_file, pos);
    assert!(
        items.iter().any(|i| i.label == "server.port"),
        "expected completion list to contain server.port; got {items:#?}"
    );

    // YAML key completions should also be metadata-backed.
    let (db, yaml_file, pos) = fixture(yaml_path.clone(), "server:\n  p<|>", vec![]);
    let items = completions(&db, yaml_file, pos);
    assert!(
        items.iter().any(|i| i.label == "port"),
        "expected YAML completion list to contain 'port'; got {items:#?}"
    );

    // Value completions in application.properties should use metadata hints.
    let (db, config_file, pos) =
        fixture(config_path.clone(), "spring.main.banner-mode=c<|>", vec![]);
    let items = completions(&db, config_file, pos);
    assert!(
        items.iter().any(|i| i.label == "console"),
        "expected value completion list to contain 'console'; got {items:#?}"
    );

    // Completions inside `@Value("${...}")` should use metadata-backed keys.
    let java_text = r#"
import org.springframework.beans.factory.annotation.Value;
class C {
  @Value("${ser<|>}")
  String port;
}
"#;
    let config_text = "whatever=1\n".to_string();
    let (db, java_file, pos) = fixture(java_path, java_text, vec![(config_path, config_text)]);
    let items = completions(&db, java_file, pos);
    assert!(
        items.iter().any(|i| i.label == "server.port"),
        "expected @Value completion list to contain server.port; got {items:#?}"
    );
}
