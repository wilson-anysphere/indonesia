#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(2);

struct Runner {
    input_tx: mpsc::SyncSender<String>,
    output_rx: Mutex<mpsc::Receiver<()>>,
}

fn runner() -> &'static Runner {
    static RUNNER: OnceLock<Runner> = OnceLock::new();
    RUNNER.get_or_init(|| {
        let (input_tx, input_rx) = mpsc::sync_channel::<String>(0);
        let (output_tx, output_rx) = mpsc::sync_channel::<()>(0);

        std::thread::spawn(move || {
            use nova_refactor::RefactorDatabase;

            for input in input_rx {
                let file = nova_refactor::FileId::new("Fuzz.java");
                let db = nova_refactor::RefactorJavaDatabase::new([(file.clone(), input.clone())]);

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
                if let Some(offset) = first_ident_offset(&input) {
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

                let _ = output_tx.send(());
            }
        });

        Runner {
            input_tx,
            output_rx: Mutex::new(output_rx),
        }
    })
}

fn first_ident_offset(text: &str) -> Option<usize> {
    text.as_bytes()
        .iter()
        .position(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$'))
}

fuzz_target!(|data: &[u8]| {
    let Some(text) = utils::truncate_utf8(data) else {
        return;
    };

    let runner = runner();
    runner
        .input_tx
        .send(text.to_owned())
        .expect("refactor_smoke worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("refactor_smoke worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("refactor_smoke fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("refactor_smoke worker thread panicked")
        }
    }
});
