#![no_main]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use lsp_types::Position;
use nova_fuzz_utils::{FuzzRunner, MAX_INPUT_SIZE};
use nova_db::InMemoryFileStore;

const TIMEOUT: Duration = Duration::from_secs(2);

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    // Decode to UTF-8 lossily so the fuzz target is resilient to arbitrary bytes.
    let text = String::from_utf8_lossy(input).to_string();

    // Pick a cursor offset derived from the raw bytes, then clamp to the text length.
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    let hash = hasher.finish() as usize;
    let offset = if text.is_empty() {
        0
    } else {
        hash % (text.len() + 1)
    };

    let position = offset_to_position(&text, offset);

    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path("/virtual/Main.java");
    db.set_file_text(file_id, text);

    // The goal is simply "never panic / never hang" on malformed input.
    let items = nova_ide::code_intelligence::completions(&db, file_id, position);

    // Guard against pathological completion explosions that could turn into OOMs.
    // This is intentionally very generous; legitimate completion lists should be far smaller than
    // this.
    assert!(items.len() <= 100_000);
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new("fuzz_completion", MAX_INPUT_SIZE, TIMEOUT, init, run_one))
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut cur: usize = 0;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    Position::new(line, col_utf16)
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});

