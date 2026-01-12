use nova_cache::DerivedArtifactCache;
use nova_core::Range;
use nova_decompile::{decompile_classfile, decompile_classfile_cached, SymbolKey};
use std::fs;
use tempfile::TempDir;

mod suite;

const FOO_CLASS: &[u8] = include_bytes!("fixtures/com/example/Foo.class");

#[test]
fn decompiled_output_contains_expected_signatures() {
    let decompiled = decompile_classfile(FOO_CLASS).expect("decompile");
    let text = decompiled.text;

    assert!(text.contains("package com.example;"));
    assert!(text.contains("@Deprecated"));
    assert!(text.contains("public class Foo implements Runnable"));
    assert!(text.contains("public static final int ANSWER;"));

    assert!(text.contains("public Foo(String arg0)"));
    assert!(text.contains("public String getName()"));
    assert!(text.contains("public int add(int arg0)"));
    assert!(text.contains("public int add(int arg0, int arg1)"));
    assert!(text.contains("public String[] echo(String[] arg0)"));
}

#[test]
fn range_mapping_points_at_identifier() {
    let decompiled = decompile_classfile(FOO_CLASS).expect("decompile");

    let method = SymbolKey::Method {
        name: "add".to_string(),
        descriptor: "(II)I".to_string(),
    };
    let range = decompiled.range_for(&method).expect("range for add(II)I");

    assert_eq!(slice_range(&decompiled.text, range), "add");

    // Keep at least one hard assertion to ensure mapping stability.
    assert_eq!(range.start.line, 10);
}

#[test]
fn cache_round_trip() {
    let temp = TempDir::new().unwrap();
    let cache = DerivedArtifactCache::new(temp.path());

    let first = decompile_classfile_cached(FOO_CLASS, &cache).expect("decompile");
    assert!(!first.from_cache);

    let second = decompile_classfile_cached(FOO_CLASS, &cache).expect("decompile");
    assert!(second.from_cache);
    assert_eq!(first.text, second.text);
    assert_eq!(first.mappings, second.mappings);

    // Ensure cache files were written.
    let entries: Vec<_> = fs::read_dir(temp.path().join("nova-decompile"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(!entries.is_empty());
}

fn slice_range(text: &str, range: Range) -> String {
    let line = text.lines().nth(range.start.line as usize).expect("line");
    let start = utf16_col_to_byte_offset(line, range.start.character);
    let end = utf16_col_to_byte_offset(line, range.end.character);
    line[start..end].to_string()
}

fn utf16_col_to_byte_offset(s: &str, col: u32) -> usize {
    let mut utf16 = 0u32;
    for (idx, ch) in s.char_indices() {
        if utf16 == col {
            return idx;
        }
        utf16 += ch.len_utf16() as u32;
        if utf16 > col {
            return idx;
        }
    }
    if utf16 == col {
        s.len()
    } else {
        s.len()
    }
}
