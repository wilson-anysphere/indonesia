use std::collections::BTreeMap;

use nova_index::{Index, ReferenceKind, SymbolKind};

#[test]
fn reference_kind_classification_is_more_informative() {
    // `implements Foo` should classify `Foo` as `Implements`.
    let mut files = BTreeMap::new();
    files.insert(
        "Implements.java".to_string(),
        "class A implements Foo {}\n".to_string(),
    );
    let index = Index::new(files);
    let candidates = index.find_name_candidates("Foo");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].kind, ReferenceKind::Implements);

    // `new Foo(` should classify `Foo` as `TypeUsage` (not `Call`).
    let mut files = BTreeMap::new();
    files.insert(
        "NewExpr.java".to_string(),
        r#"class A {
    void test() {
        new Foo().m();
    }
}
"#
        .to_string(),
    );
    let index = Index::new(files);
    let candidates = index.find_name_candidates("Foo");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].kind, ReferenceKind::TypeUsage);

    // `@Override void m() {}` declaration should classify the declaration-site `m` as `Override`,
    // while call sites should remain `Call`.
    let mut files = BTreeMap::new();
    files.insert(
        "Override.java".to_string(),
        r#"class A {
    @Override
    void m() {}

    void test() {
        m();
    }
}
"#
        .to_string(),
    );
    let index = Index::new(files);

    let m_sym = index
        .symbols()
        .iter()
        .find(|sym| sym.kind == SymbolKind::Method && sym.name == "m")
        .expect("expected method symbol `m` to be indexed");
    assert!(
        m_sym.is_override,
        "expected method `m` to be marked `@Override`"
    );

    let candidates = index.find_name_candidates("m");
    let decl_candidate = candidates
        .iter()
        .find(|c| c.file == "Override.java" && c.range == m_sym.name_range)
        .expect("expected a candidate at the `@Override` declaration name token");
    assert_eq!(decl_candidate.kind, ReferenceKind::Override);

    assert!(
        candidates.iter().any(|c| {
            c.file == "Override.java"
                && c.range != m_sym.name_range
                && c.kind == ReferenceKind::Call
        }),
        "expected at least one call-site of `m()` to still be classified as `Call`"
    );
}
