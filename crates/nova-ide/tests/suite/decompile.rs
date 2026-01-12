use nova_decompile::{decompile_classfile, parse_decompiled_uri, SymbolKey};
use nova_ide::canonical_decompiled_definition_location;

const FOO_CLASS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../nova-decompile/tests/fixtures/com/example/Foo.class"
));
const FOO_INTERNAL_NAME: &str = "com/example/Foo";

#[test]
fn canonical_decompiled_definition_location_uses_adr0006_uri_and_correct_range() {
    let decompiled = decompile_classfile(FOO_CLASS).expect("decompile");
    let symbol = SymbolKey::Class {
        internal_name: FOO_INTERNAL_NAME.to_string(),
    };
    let expected_range = decompiled.range_for(&symbol).expect("symbol range");

    let location = canonical_decompiled_definition_location(
        FOO_INTERNAL_NAME,
        FOO_CLASS,
        &decompiled,
        &symbol,
    )
    .expect("definition location");

    assert!(location.uri.starts_with("nova:///decompiled/"));
    assert!(
        parse_decompiled_uri(&location.uri).is_some(),
        "canonical URI should parse"
    );
    assert_eq!(location.range, expected_range);
}
