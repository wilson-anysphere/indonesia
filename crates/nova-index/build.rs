use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Detect whether the `nova-index` crate defines an `IndexedSymbol` type.
///
/// Task 17 changes `SymbolIndex` to store `IndexedSymbol` entries. This build
/// script emits a cfg flag so `benches/mmap_storage.rs` can compile both before
/// and after that change lands.
fn main() {
    // `rustc` (via Cargo) checks `#[cfg(...)]` names by default. Advertise our
    // custom cfg so users don't get `unexpected_cfgs` warnings when compiling
    // the benchmark target.
    println!("cargo:rustc-check-cfg=cfg(nova_index_has_indexed_symbol)");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");

    let mut found_indexed_symbol = false;
    visit_rs_files(&src_dir, &mut |path| {
        // Ensure the build script is re-run when any source file changes.
        println!("cargo:rerun-if-changed={}", path.display());

        // Best-effort scan; if this fails, just treat it as "not found".
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        if text.contains("pub struct IndexedSymbol") || text.contains("struct IndexedSymbol") {
            found_indexed_symbol = true;
        }
    });

    if found_indexed_symbol {
        println!("cargo:rustc-cfg=nova_index_has_indexed_symbol");
    }
}

fn visit_rs_files(dir: &Path, f: &mut impl FnMut(&Path)) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rs_files(&path, f);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            f(&path);
        }
    }
}
