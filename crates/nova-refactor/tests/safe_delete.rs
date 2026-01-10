use std::collections::BTreeMap;

use nova_index::Index;
use nova_refactor::{
    apply_edits, safe_delete, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteTarget, UsageKind,
};
use pretty_assertions::assert_eq;

#[test]
fn safe_delete_succeeds_for_unused_private_method() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    private void unused() {
    }

    public void entry() {
    }
}
"#
        .to_string(),
    );
    let index = Index::new(files.clone());
    let target = index.find_method("A", "unused").expect("method exists").id;

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(target),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");
    let edits = match outcome {
        SafeDeleteOutcome::Applied { edits } => edits,
        SafeDeleteOutcome::Preview { .. } => panic!("expected direct application"),
    };

    let updated = apply_edits(&files, &edits);
    let a = updated.get("A.java").unwrap();
    assert!(
        !a.contains("unused()"),
        "method declaration should be removed"
    );
}

#[test]
fn safe_delete_blocks_for_used_method_and_reports_usage_locations() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    public void used() {
    }

    public void entry() {
        used();
    }
}
"#
        .to_string(),
    );
    let index = Index::new(files);
    let target = index.find_method("A", "used").expect("method exists").id;

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(target),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");
    let report = match outcome {
        SafeDeleteOutcome::Preview { report } => report,
        SafeDeleteOutcome::Applied { .. } => panic!("expected preview"),
    };

    assert_eq!(report.usages.len(), 1);
    let usage = &report.usages[0];
    assert_eq!(usage.file, "A.java");
    assert_eq!(usage.kind, UsageKind::Call);
    let text = index.file_text(&usage.file).unwrap();
    assert_eq!(&text[usage.range.start..usage.range.end], "used");
}
