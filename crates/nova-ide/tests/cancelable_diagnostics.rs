mod framework_harness;
mod text_fixture;

use framework_harness::fixture_multi;
use nova_scheduler::CancellationToken;
use nova_types::Severity;
use std::path::PathBuf;

#[test]
fn all_diagnostics_returns_empty_when_cancelled() {
    let java_path = PathBuf::from("/workspace/src/main/java/A.java");
    let java_text = r#"
class A {
  void m() {
    baz();
  }
}
"#;

    let fixture = fixture_multi(java_path, java_text, vec![]);

    let cancel = CancellationToken::new();
    cancel.cancel();

    let diags = fixture.ide.all_diagnostics(cancel, fixture.file);
    assert!(
        diags.is_empty(),
        "expected no diagnostics when cancelled; got {diags:#?}"
    );
}

#[test]
fn all_diagnostics_returns_core_diagnostics_when_not_cancelled() {
    let java_path = PathBuf::from("/workspace/src/main/java/A.java");
    let java_text = r#"
class A {
  void m() {
    baz();
  }
}
"#;

    let fixture = fixture_multi(java_path, java_text, vec![]);
    let diags = fixture
        .ide
        .all_diagnostics(CancellationToken::new(), fixture.file);

    assert!(
        diags.iter().any(|d| {
            d.severity == Severity::Error && d.code.as_ref() == "UNRESOLVED_REFERENCE"
        }),
        "expected unresolved reference diagnostic; got {diags:#?}"
    );
}
