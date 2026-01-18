use std::fs;
use std::path::Path;

use nova_vfs::VfsPath;
use nova_workspace::{Workspace, WorkspaceEvent};
use tokio::time::{timeout, Duration};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

fn pom_java11() -> &'static str {
    r#"
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId>
          <artifactId>demo</artifactId>
          <version>0.0.1</version>
          <properties>
            <maven.compiler.source>11</maven.compiler.source>
            <maven.compiler.target>11</maven.compiler.target>
          </properties>
        </project>
    "#
}

async fn wait_for_diagnostics(
    events: &async_channel::Receiver<std::sync::Arc<WorkspaceEvent>>,
    file: &VfsPath,
) -> std::sync::Arc<Vec<nova_types::Diagnostic>> {
    loop {
        let ev = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("diagnostics event")
            .expect("channel open");
        if let WorkspaceEvent::DiagnosticsUpdated {
            file: got,
            diagnostics,
        } = ev.as_ref()
        {
            if got == file {
                return std::sync::Arc::clone(diagnostics);
            }
        }
    }
}

#[tokio::test]
async fn workspace_diagnostics_use_workspace_salsa_db_for_imports_and_language_level() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize temp root");

    // Configure language level below records (Java 11).
    write(&root.join("pom.xml"), pom_java11());

    // Local import across files.
    write(&root.join("src/p/A.java"), "package p; public class A {}");
    write(
        &root.join("src/q/B.java"),
        "package q; import p.A; class B { A a; }",
    );

    // Records are a Java 16 feature.
    write(
        &root.join("src/r/R.java"),
        "package r; public record R(int x) {}",
    );

    let ws = Workspace::open(&root).expect("workspace open");
    let events = ws.subscribe();

    let b_path = VfsPath::local(root.join("src/q/B.java"));
    let b_text = fs::read_to_string(root.join("src/q/B.java")).expect("read B.java");
    ws.open_document(b_path.clone(), b_text, 1);

    let b_diags = wait_for_diagnostics(&events, &b_path).await;
    assert!(
        !b_diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-import"),
        "expected local import to resolve; got diagnostics: {b_diags:#?}"
    );

    let record_path = VfsPath::local(root.join("src/r/R.java"));
    let record_text = fs::read_to_string(root.join("src/r/R.java")).expect("read R.java");
    ws.open_document(record_path.clone(), record_text, 1);

    let record_diags = wait_for_diagnostics(&events, &record_path).await;
    assert!(
        record_diags
            .iter()
            .any(|d| d.code.as_ref() == "JAVA_FEATURE_RECORDS"),
        "expected JAVA_FEATURE_RECORDS diagnostic under Java 11; got: {record_diags:#?}"
    );
}
