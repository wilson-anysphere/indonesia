#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner, MAX_INPUT_SIZE};

const TIMEOUT: Duration = Duration::from_secs(2);

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    use nova_refactor::RefactorDatabase;

    let Some(text) = truncate_utf8(input) else {
        return;
    };

    let file = nova_refactor::FileId::new("Fuzz.java");
    let db = nova_refactor::RefactorJavaDatabase::new([(file.clone(), text.to_owned())]);

    // Organize imports is explicitly best-effort: errors are fine, panics aren't.
    if let Ok(edit) = nova_refactor::organize_imports(
        &db,
        nova_refactor::OrganizeImportsParams { file: file.clone() },
    ) {
        // Applying edits is part of the normal refactoring pipeline; exercise it too.
        if let Some(text) = db.file_text(&file) {
            let edits: Vec<nova_refactor::WorkspaceTextEdit> = edit
                .edits_by_file()
                .get(&file)
                .map(|edits| edits.iter().copied().cloned().collect())
                .unwrap_or_else(Vec::new);
            let _ = nova_refactor::apply_text_edits(text, &edits);
        }
    }

    // If we can locate any symbol-like span, try a rename. Refactor errors are expected.
    if let Some(offset) = first_ident_offset(text) {
        if let Some(symbol) = db.symbol_at(&file, offset) {
            if let Ok(edit) = nova_refactor::rename(
                &db,
                nova_refactor::RenameParams {
                    symbol,
                    new_name: "renamed".to_string(),
                },
            ) {
                if let Some(text) = db.file_text(&file) {
                    let edits: Vec<nova_refactor::WorkspaceTextEdit> = edit
                        .edits_by_file()
                        .get(&file)
                        .map(|edits| edits.iter().copied().cloned().collect())
                        .unwrap_or_else(Vec::new);
                    let _ = nova_refactor::apply_text_edits(text, &edits);
                }
            }
        }
    }
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new("refactor_smoke", MAX_INPUT_SIZE, TIMEOUT, init, run_one))
}

fn first_ident_offset(text: &str) -> Option<usize> {
    text.as_bytes()
        .iter()
        .position(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$'))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
