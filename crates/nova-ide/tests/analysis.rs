mod framework_harness;
mod suite;

use nova_ide::analysis;

#[test]
fn diagnostics_are_provided_by_code_intelligence_layer() {
    let diags = analysis::diagnostics("class A {\n error \n}");

    assert!(
        !diags.is_empty(),
        "expected at least one diagnostic from code_intelligence; got {diags:#?}"
    );
    assert!(
        diags.iter().all(|d| d.code != "E0001"),
        "analysis::diagnostics should no longer use the legacy substring matcher; got {diags:#?}"
    );
}

#[test]
fn completions_delegate_to_code_intelligence_layer() {
    let src = r#"class A { void m() { "x". } }"#;
    let offset = src.find('.').expect("expected dot in fixture") + 1;

    let items = analysis::completions(src, offset);
    assert!(
        !items.is_empty(),
        "expected non-empty completion list; got {items:#?}"
    );

    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"length"),
        "expected completion list to contain String.length; got {labels:?}"
    );
}
