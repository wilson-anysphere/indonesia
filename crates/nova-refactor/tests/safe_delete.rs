use std::collections::BTreeMap;

use nova_index::Index;
use nova_refactor::{
    apply_text_edits, safe_delete, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteTarget, UsageKind,
    WorkspaceEdit, WorkspaceTextEdit,
};
use pretty_assertions::assert_eq;

fn apply_workspace_edit(
    files: &BTreeMap<String, String>,
    edit: &WorkspaceEdit,
) -> BTreeMap<String, String> {
    let mut out = files.clone();

    for (file, edits) in edit.edits_by_file() {
        let Some(text) = out.get(file.0.as_str()).cloned() else {
            continue;
        };
        let edits: Vec<WorkspaceTextEdit> = edits.into_iter().cloned().collect();
        let updated = apply_text_edits(&text, &edits).expect("edits apply");
        out.insert(file.0.clone(), updated);
    }

    out
}

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
    let edit = match outcome {
        SafeDeleteOutcome::Applied { edit } => edit,
        SafeDeleteOutcome::Preview { .. } => panic!("expected direct application"),
    };

    let updated = apply_workspace_edit(&files, &edit);
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
