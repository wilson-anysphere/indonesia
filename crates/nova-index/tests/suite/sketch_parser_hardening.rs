use std::collections::BTreeMap;

use nova_index::{Index, SymbolKind};

#[test]
fn class_keyword_inside_string_is_ignored() {
    let source = r#"
String s = "class Fake { void x() {} }";

class Real {
    void m() {}
}
"#;

    let files = BTreeMap::from([("Test.java".to_string(), source.to_string())]);
    let index = Index::new(files);

    let class_names: Vec<String> = index
        .symbols()
        .iter()
        .filter(|sym| sym.kind == SymbolKind::Class)
        .map(|sym| sym.name.clone())
        .collect();

    assert_eq!(class_names, vec!["Real".to_string()]);
    assert!(index.find_method("Real", "m").is_some());
    assert!(index
        .symbols()
        .iter()
        .all(|sym| sym.kind != SymbolKind::Class || sym.name != "Fake"));
}

#[test]
fn brace_inside_char_literal_does_not_break_class_range() {
    // Intentionally put `'}'` before `'{'` so brace matching must ignore char literals to avoid
    // terminating the method/class early.
    let source = r#"
class Real {
    void m() {
        char d = '}';
        char c = '{';
    }
}
"#;

    let files = BTreeMap::from([("Test.java".to_string(), source.to_string())]);
    let index = Index::new(files);

    let class_sym = index
        .symbols()
        .iter()
        .find(|sym| sym.kind == SymbolKind::Class && sym.name == "Real")
        .expect("class Real should be indexed");

    let class_end = source.rfind('}').expect("class should have closing brace") + 1;
    assert_eq!(
        class_sym.decl_range.end, class_end,
        "class decl_range should span the full class body"
    );

    let method_sym = index
        .find_method("Real", "m")
        .expect("method m should be indexed");

    // Method decl_range should include both char assignments and end at the method's closing brace.
    let method_text = &source[method_sym.decl_range.start..method_sym.decl_range.end];
    assert!(
        method_text.contains("char d"),
        "method decl_range should include `char d` initializer"
    );
    assert!(
        method_text.contains("char c"),
        "method decl_range should include `char c` initializer"
    );

    let method_close_line = "\n    }\n";
    let method_close_brace = source
        .find(method_close_line)
        .expect("method closing brace line should exist")
        + method_close_line.len()
        - 2; // position of the `}` in `\n    }\n`
    let method_end = method_close_brace + 1;
    assert_eq!(
        method_sym.decl_range.end, method_end,
        "method decl_range should end at the method closing brace"
    );
}
