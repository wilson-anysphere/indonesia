use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_db::InMemoryFileStore;
use nova_framework::{CompletionContext, FrameworkAnalyzer};
use nova_framework_spring::{SpringAnalyzer, SPRING_UNKNOWN_CONFIG_KEY};
use nova_scheduler::CancellationToken;
use tempfile::tempdir;
use zip::write::FileOptions;

fn write_metadata_jar(
    repo_root: &Path,
    group: &str,
    artifact: &str,
    version: &str,
    metadata_json: &str,
) -> PathBuf {
    let mut jar_path = repo_root.to_path_buf();
    for seg in group.split('.') {
        jar_path = jar_path.join(seg);
    }
    jar_path = jar_path
        .join(artifact)
        .join(version)
        .join(format!("{artifact}-{version}.jar"));

    std::fs::create_dir_all(jar_path.parent().expect("jar parent")).expect("create repo dirs");
    let mut jar = zip::ZipWriter::new(std::fs::File::create(&jar_path).expect("create jar"));
    jar.start_file(
        "META-INF/spring-configuration-metadata.json",
        FileOptions::<()>::default(),
    )
    .expect("start metadata file");
    write!(jar, "{metadata_json}").expect("write metadata");
    jar.finish().expect("finish jar");

    jar_path
}

#[test]
fn spring_analyzer_sees_dependency_metadata_via_framework_db_synthetic_files() {
    let workspace_dir = tempdir().expect("workspace tempdir");
    let repo_dir = tempdir().expect("repo tempdir");

    // Canonicalize temp paths for cross-platform stability (macOS `/var` → `/private/var`).
    let workspace_root = workspace_dir
        .path()
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.path().to_path_buf());
    let repo_root = repo_dir
        .path()
        .canonicalize()
        .unwrap_or_else(|_| repo_dir.path().to_path_buf());

    // Make `nova_project::load_project` resolve dependencies from our temp repo.
    let mvn_dir = workspace_root.join(".mvn");
    std::fs::create_dir_all(&mvn_dir).expect("create .mvn");
    std::fs::write(
        mvn_dir.join("maven.config"),
        format!("-Dmaven.repo.local={}", repo_root.display()),
    )
    .expect("write maven.config");

    // Fake Spring Boot dependency that contains configuration metadata.
    let group_id = "org.springframework.boot";
    let artifact_id = "spring-boot";
    let version = "1.0.0";
    let metadata_json = r#"{
  "properties": [
    { "name": "server.port", "type": "java.lang.Integer" }
  ]
}"#;
    let _jar_path = write_metadata_jar(&repo_root, group_id, artifact_id, version, metadata_json);

    // Minimal Maven project that declares the dependency.
    std::fs::write(
        workspace_root.join("pom.xml"),
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
    .expect("write pom.xml");

    // Ensure the config file path exists on disk so workspace root discovery prefers build markers.
    let config_path = workspace_root.join("src/main/resources/application.properties");
    std::fs::create_dir_all(config_path.parent().expect("config parent")).expect("mkdir -p");
    std::fs::write(&config_path, "").expect("touch application.properties");

    let cancel = CancellationToken::new();
    let analyzer = SpringAnalyzer::new();

    // Diagnostics: known key should NOT be flagged as unknown when dependency metadata is present.
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&config_path);
    db.set_file_text(file, "server.port=8080\n".to_string());
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);

    let fw_db =
        nova_ide::framework_db::framework_db_for_file(db, file, &cancel).expect("framework db");
    let project = fw_db.project_of_file(file);
    assert!(
        analyzer.applies_to(fw_db.as_ref(), project),
        "expected fake spring-boot dependency to make project applicable"
    );

    let diags = analyzer.diagnostics(fw_db.as_ref(), file);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == SPRING_UNKNOWN_CONFIG_KEY),
        "expected no unknown-key diagnostics for server.port; got {diags:#?}"
    );

    // Diagnostics: unknown key should be flagged when metadata from the dependency jar is available.
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&config_path);
    db.set_file_text(file, "unknown.key=foo\n".to_string());
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);

    let fw_db =
        nova_ide::framework_db::framework_db_for_file(db, file, &cancel).expect("framework db");
    let _project = fw_db.project_of_file(file);

    let diags = analyzer.diagnostics(fw_db.as_ref(), file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == SPRING_UNKNOWN_CONFIG_KEY),
        "expected unknown-key diagnostics when metadata is present; got {diags:#?}"
    );

    // Completions: metadata-backed keys should be offered.
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(&config_path);
    db.set_file_text(file, "serv".to_string());
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);

    let fw_db =
        nova_ide::framework_db::framework_db_for_file(db, file, &cancel).expect("framework db");
    let project = fw_db.project_of_file(file);
    let ctx = CompletionContext {
        project,
        file,
        offset: "serv".len(),
    };
    let items = analyzer.completions(fw_db.as_ref(), &ctx);
    assert!(
        items.iter().any(|i| i.label == "server.port"),
        "expected completion list to include server.port from dependency metadata; got {items:#?}"
    );
}
