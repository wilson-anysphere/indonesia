use std::collections::BTreeMap;

use nova_index::Index;

#[test]
fn find_overrides_finds_derived_without_override_annotation() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class Base {
    void foo(int x) {}
}

class Derived extends Base {
    void foo(int x) {}
}
"#
        .to_string(),
    );

    let index = Index::new(files);

    let base_id = index.method_symbol_id("Base", "foo").expect("Base.foo");
    let derived_id = index
        .method_symbol_id("Derived", "foo")
        .expect("Derived.foo");

    let overrides = index.find_overrides(base_id);
    assert_eq!(overrides, vec![derived_id]);

    assert_eq!(index.find_overridden(derived_id), Some(base_id));
}

#[test]
fn find_overrides_is_transitive_across_multiple_levels() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class Base {
    void foo(int x) {}
}

class Mid extends Base {
    void foo(int x) {}
}

class Leaf extends Mid {
    void foo(int x) {}
}
"#
        .to_string(),
    );

    let index = Index::new(files);

    let base_id = index.method_symbol_id("Base", "foo").expect("Base.foo");
    let mid_id = index.method_symbol_id("Mid", "foo").expect("Mid.foo");
    let leaf_id = index.method_symbol_id("Leaf", "foo").expect("Leaf.foo");

    let overrides = index.find_overrides(base_id);
    assert_eq!(overrides, vec![mid_id, leaf_id]);

    assert_eq!(index.find_overridden(mid_id), Some(base_id));
    assert_eq!(index.find_overridden(leaf_id), Some(mid_id));
}

#[test]
fn interface_method_implementations_are_reported_as_overrides() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
interface I {
    void foo(int x);
}

class Impl implements I {
    public void foo(int x) {}
}
"#
        .to_string(),
    );

    let index = Index::new(files);

    let iface_id = index.method_symbol_id("I", "foo").expect("I.foo");
    let impl_id = index.method_symbol_id("Impl", "foo").expect("Impl.foo");

    let overrides = index.find_overrides(iface_id);
    assert_eq!(overrides, vec![impl_id]);

    assert_eq!(index.find_overridden(impl_id), Some(iface_id));
}

#[test]
fn all_subtypes_is_transitive_and_deterministic() {
    let mut files = BTreeMap::new();
    files.insert(
        "A.java".to_string(),
        r#"
class Base {}
class Mid extends Base {}
class Leaf extends Mid {}
"#
        .to_string(),
    );

    let index = Index::new(files);
    assert_eq!(
        index.all_subtypes("Base"),
        vec!["Mid".to_string(), "Leaf".to_string()]
    );
}
