use std::collections::BTreeMap;

use nova_index::Index;
use nova_refactor::{
    safe_delete, FileId, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteTarget, UsageKind,
    WorkspaceEdit,
};
use pretty_assertions::assert_eq;

fn apply_workspace_edit(
    files: &BTreeMap<String, String>,
    edit: &WorkspaceEdit,
) -> BTreeMap<String, String> {
    let input: BTreeMap<FileId, String> = files
        .iter()
        .map(|(file, text)| (FileId::new(file.clone()), text.clone()))
        .collect();
    let out = nova_refactor::apply_workspace_edit(&input, edit).expect("apply workspace edit");
    out.into_iter().map(|(file, text)| (file.0, text)).collect()
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

#[test]
fn safe_delete_detects_usage_on_new_expression_in_other_file() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    public void used() {
    }
}
"#
        .to_string(),
    );
    files.insert(
        "B.java".to_string(),
        r#"
class B {
    public void entry() {
        new A().used();
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
    assert_eq!(report.usages[0].file, "B.java");
    assert_eq!(report.usages[0].kind, UsageKind::Call);
}

#[test]
fn safe_delete_delete_anyway_removes_overrides() {
    let mut files = BTreeMap::new();
    files.insert(
        "Base.java".to_string(),
        r#"
class Base {
    public void used() {
    }
}
"#
        .to_string(),
    );
    files.insert(
        "Derived.java".to_string(),
        r#"
class Derived extends Base {
    @Override
    public void used() {
    }

    public void other() {
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files.clone());
    let target = index.find_method("Base", "used").expect("method exists").id;

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
    assert_eq!(report.usages[0].file, "Derived.java");
    assert_eq!(report.usages[0].kind, UsageKind::Override);

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(target),
        SafeDeleteMode::DeleteAnyway,
    )
    .expect("safe delete runs");
    let edit = match outcome {
        SafeDeleteOutcome::Applied { edit } => edit,
        SafeDeleteOutcome::Preview { .. } => panic!("expected applied edits"),
    };

    let updated = apply_workspace_edit(&files, &edit);

    let base = updated.get("Base.java").unwrap();
    assert!(!base.contains("used()"), "base method should be removed");

    let derived = updated.get("Derived.java").unwrap();
    assert!(
        !derived.contains("@Override"),
        "override annotation should be removed"
    );
    assert!(
        !derived.contains("used()"),
        "overriding method should be removed"
    );
    assert!(
        !derived.contains("void ()"),
        "should not leave an empty method name"
    );
}

#[test]
fn safe_delete_ignores_other_overload_calls() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    public void foo() {
    }

    public void foo(int x) {
    }

    public void entry() {
        foo(1);
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files.clone());
    let target = index
        .method_overloads_by_arity("A", "foo", 0)
        .into_iter()
        .next()
        .expect("foo() exists");

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(target),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");

    let edit = match outcome {
        SafeDeleteOutcome::Applied { edit } => edit,
        SafeDeleteOutcome::Preview { report } => {
            panic!("expected direct application (no usages), got: {report:?}")
        }
    };

    let updated = apply_workspace_edit(&files, &edit);
    let a = updated.get("A.java").unwrap();
    assert!(
        !a.contains("void foo()"),
        "foo() declaration should be removed"
    );
    assert!(
        a.contains("void foo(int x)"),
        "other overload should remain: {a}"
    );
    assert!(a.contains("foo(1)"), "call site should remain: {a}");
}

#[test]
fn safe_delete_ignores_overload_declarations_as_usages() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    public void foo(int x) {
    }

    public void foo(String s) {
    }

    public void entry() {
        foo(1);
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files.clone());
    let target = index
        .method_overload_by_param_types("A", "foo", &[String::from("String")])
        .expect("foo(String) exists");

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(target),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");

    let edit = match outcome {
        SafeDeleteOutcome::Applied { edit } => edit,
        SafeDeleteOutcome::Preview { report } => {
            panic!("expected direct application (no usages), got: {report:?}")
        }
    };

    let updated = apply_workspace_edit(&files, &edit);
    let a = updated.get("A.java").unwrap();
    assert!(
        !a.contains("foo(String"),
        "foo(String) declaration should be removed: {a}"
    );
    assert!(
        a.contains("foo(int x)"),
        "other overload should remain: {a}"
    );
    assert!(a.contains("foo(1)"), "call site should remain: {a}");
}

#[test]
fn safe_delete_blocks_when_target_overload_called() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    public void foo() {
    }

    public void foo(int x) {
    }

    public void entry() {
        foo(1);
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files);
    let target = index
        .method_overloads_by_arity("A", "foo", 1)
        .into_iter()
        .next()
        .expect("foo(int) exists");

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

    assert_eq!(
        report.usages.len(),
        1,
        "expected one call usage: {report:?}"
    );
    let usage = &report.usages[0];
    assert_eq!(usage.file, "A.java");
    assert_eq!(usage.kind, UsageKind::Call);
    let text = index.file_text(&usage.file).unwrap();
    assert_eq!(&text[usage.range.start..usage.range.end], "foo");
}

#[test]
fn safe_delete_override_detection_respects_overload_signature() {
    let mut files = BTreeMap::new();
    files.insert(
        "Base.java".to_string(),
        r#"
class Base {
    public void foo() {
    }

    public void foo(int x) {
    }
}
"#
        .to_string(),
    );
    files.insert(
        "Derived.java".to_string(),
        r#"
class Derived extends Base {
    @Override
    public void foo(int x) {
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files.clone());
    let base_foo0 = index
        .method_overloads_by_arity("Base", "foo", 0)
        .into_iter()
        .next()
        .expect("foo() exists");
    let base_foo1 = index
        .method_overloads_by_arity("Base", "foo", 1)
        .into_iter()
        .next()
        .expect("foo(int) exists");

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(base_foo0),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");
    match outcome {
        SafeDeleteOutcome::Applied { .. } => {}
        SafeDeleteOutcome::Preview { report } => {
            panic!("expected foo() to be deleted safely, got: {report:?}");
        }
    }

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(base_foo1),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");
    let report = match outcome {
        SafeDeleteOutcome::Preview { report } => report,
        SafeDeleteOutcome::Applied { .. } => panic!("expected preview for foo(int)"),
    };

    assert!(
        report
            .usages
            .iter()
            .any(|usage| usage.kind == UsageKind::Override && usage.file == "Derived.java"),
        "expected override usage for foo(int): {report:?}"
    );
}

#[test]
fn safe_delete_ignores_other_overload_calls_with_same_arity() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    public void foo(int x) {
    }

    public void foo(String s) {
    }

    public void entry() {
        foo("hi");
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files.clone());
    let target = index
        .method_overload_by_param_types("A", "foo", &[String::from("int")])
        .expect("foo(int) exists");

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(target),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");

    let edit = match outcome {
        SafeDeleteOutcome::Applied { edit } => edit,
        SafeDeleteOutcome::Preview { report } => {
            panic!("expected direct application (no usages), got: {report:?}")
        }
    };

    let updated = apply_workspace_edit(&files, &edit);
    let a = updated.get("A.java").unwrap();
    assert!(
        !a.contains("void foo(int x)"),
        "foo(int) declaration should be removed"
    );
    assert!(
        a.contains("void foo(String s)"),
        "other overload should remain: {a}"
    );
    assert!(a.contains(r#"foo("hi")"#), "call site should remain: {a}");
}

#[test]
fn safe_delete_ignores_other_overload_calls_with_same_arity_for_generic_var_args() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
import java.util.HashMap;
import java.util.Map;

class A {
    public void foo(int x) {
    }

    public void foo(Map<String, Integer> m) {
    }

    public void entry() {
        Map<String, Integer> m = new HashMap<String, Integer>();
        foo(m);
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files.clone());
    let target = index
        .method_overload_by_param_types("A", "foo", &[String::from("int")])
        .expect("foo(int) exists");

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(target),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");

    let edit = match outcome {
        SafeDeleteOutcome::Applied { edit } => edit,
        SafeDeleteOutcome::Preview { report } => {
            panic!("expected direct application (no usages), got: {report:?}")
        }
    };

    let updated = apply_workspace_edit(&files, &edit);
    let a = updated.get("A.java").unwrap();
    assert!(
        !a.contains("void foo(int x)"),
        "foo(int) declaration should be removed"
    );
    assert!(
        a.contains("void foo(Map<String, Integer> m)"),
        "other overload should remain: {a}"
    );
    assert!(a.contains("foo(m)"), "call site should remain: {a}");
}

#[test]
fn safe_delete_blocks_when_target_overload_called_with_same_arity() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class A {
    public void foo(int x) {
    }

    public void foo(String s) {
    }

    public void entry() {
        foo(1);
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files);
    let target = index
        .method_overload_by_param_types("A", "foo", &[String::from("int")])
        .expect("foo(int) exists");

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

    assert_eq!(
        report.usages.len(),
        1,
        "expected one call usage: {report:?}"
    );
    assert_eq!(report.usages[0].file, "A.java");
    assert!(
        matches!(report.usages[0].kind, UsageKind::Call | UsageKind::Unknown),
        "expected call/unknown usage kind: {report:?}"
    );
}

#[test]
fn safe_delete_reports_overrides_with_overloads_without_override_annotation() {
    let mut files = BTreeMap::new();
    files.insert(
        "Base.java".to_string(),
        r#"
class Base {
    public void foo(int x) {
    }

    public void foo(String s) {
    }
}
"#
        .to_string(),
    );
    files.insert(
        "Derived.java".to_string(),
        r#"
class Derived extends Base {
    public void foo(int x) {
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files);
    let target = index
        .method_overload_by_param_types("Base", "foo", &[String::from("int")])
        .expect("foo(int) exists");

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

    assert_eq!(
        report.usages.len(),
        1,
        "expected one override usage: {report:?}"
    );
    assert_eq!(report.usages[0].file, "Derived.java");
    assert_eq!(report.usages[0].kind, UsageKind::Override);
}

#[test]
fn safe_delete_blocks_when_call_arg_contains_generic_commas() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
import java.util.HashMap;
import java.util.Map;

class A {
    public void foo(Map<String, Integer> m) {
    }

    public void entry() {
        foo(new HashMap<String, Integer>());
    }
}
"#
        .to_string(),
    );

    let index = Index::new(files);
    let target = index.find_method("A", "foo").expect("method exists").id;

    let outcome = safe_delete(
        &index,
        SafeDeleteTarget::Symbol(target),
        SafeDeleteMode::Safe,
    )
    .expect("safe delete runs");

    let report = match outcome {
        SafeDeleteOutcome::Preview { report } => report,
        SafeDeleteOutcome::Applied { .. } => panic!("expected preview (call usage)"),
    };

    assert_eq!(
        report.usages.len(),
        1,
        "expected one call usage; got: {report:?}"
    );
    assert_eq!(report.usages[0].file, "A.java");
    assert_eq!(report.usages[0].kind, UsageKind::Call);
}
