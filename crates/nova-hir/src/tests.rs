use crate::token_item_tree::{TokenItem, TokenItemKind, TokenItemTree, TokenSymbolSummary};
use bincode::Options as _;
use nova_syntax::TextRange;
use rkyv::Deserialize as _;

fn fnv1a64(mut hash: u64, bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001B3;

    if hash == 0 {
        hash = FNV_OFFSET_BASIS;
    }

    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    hash
}

fn hir_persisted_schema_fingerprint() -> u64 {
    let mut hash = 0u64;

    // Hash the bincode serialization of a small sample of the persisted types.
    // This is intentionally cheap and deterministic; any structural/serde change
    // to `TokenItemTree`/`TokenSymbolSummary` will almost certainly change these bytes.
    let sample = (
        TokenItemTree {
            items: vec![TokenItem {
                kind: TokenItemKind::Class,
                name: "Foo".to_string(),
                name_range: TextRange { start: 10, end: 13 },
            }],
        },
        Some(TokenSymbolSummary {
            names: vec!["Foo".to_string()],
        }),
    );

    // Keep this aligned with `nova-cache` (`bincode_options`).
    let bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&sample)
        .expect("bincode serialization should succeed");
    hash = fnv1a64(hash, b"nova-hir persisted schema\n");
    hash = fnv1a64(hash, &bytes);
    hash
}

// NOTE: If this fails, update the constant and *consider* bumping
// `HIR_SCHEMA_VERSION` in `lib.rs`.
const EXPECTED_HIR_PERSISTED_SCHEMA_FINGERPRINT: u64 = 0x8130_201c_c2ad_7153;

#[test]
fn hir_persisted_schema_fingerprint_guardrail() {
    let actual = hir_persisted_schema_fingerprint();
    let expected = EXPECTED_HIR_PERSISTED_SCHEMA_FINGERPRINT;

    assert_eq!(
        actual, expected,
        "HIR persisted schema fingerprint changed.\n\
\n\
This is a guardrail for Nova's on-disk AST cache:\n\
- Review whether the change impacts the serialized `TokenItemTree`/\n\
  `TokenSymbolSummary` format.\n\
- Bump `nova_hir::HIR_SCHEMA_VERSION` if old caches would fail to\n\
  deserialize or become semantically invalid.\n\
- Update `EXPECTED_HIR_PERSISTED_SCHEMA_FINGERPRINT` in\n\
  `crates/nova-hir/src/tests.rs`.\n\
\n\
expected: {expected:#018x}\n\
actual:   {actual:#018x}\n"
    );
}

#[test]
fn token_item_tree_rkyv_archive_roundtrip() {
    let item_tree = TokenItemTree {
        items: vec![TokenItem {
            kind: TokenItemKind::Class,
            name: "Foo".to_string(),
            name_range: TextRange { start: 10, end: 13 },
        }],
    };
    let symbol_summary = Some(TokenSymbolSummary::from_item_tree(&item_tree));
    let sample = (item_tree, symbol_summary);

    let bytes = rkyv::to_bytes::<_, 256>(&sample).expect("rkyv serialization should succeed");
    let archived = rkyv::check_archived_root::<(TokenItemTree, Option<TokenSymbolSummary>)>(
        bytes.as_ref(),
    )
    .expect("rkyv validation should succeed");

    let mut deserializer = rkyv::de::deserializers::SharedDeserializeMap::default();
    let roundtrip: (TokenItemTree, Option<TokenSymbolSummary>) = archived
        .deserialize(&mut deserializer)
        .expect("rkyv deserialization should succeed");

    assert_eq!(roundtrip, sample);
}
