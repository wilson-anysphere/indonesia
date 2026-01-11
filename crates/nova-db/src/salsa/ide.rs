use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use nova_cache::Fingerprint;
use serde::Serialize;

use crate::FileId;

use super::cancellation as cancel;
use super::semantic::NovaSemantic;
use super::stats::HasQueryStats;

#[cfg(test)]
pub(super) static UPPERCASED_FILE_WORDS_COMPUTE_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[ra_salsa::query_group(NovaIdeStorage)]
pub trait NovaIde: NovaSemantic + HasQueryStats {
    /// Debug query used to validate request cancellation behavior.
    ///
    /// Real queries (type-checking, indexing, etc.) should periodically call
    /// `db.unwind_if_cancelled()` while doing expensive work; this query exists
    /// as a lightweight fixture for that pattern.
    fn interruptible_work(&self, file: FileId, steps: u32) -> u64;

    /// Demo derived query that persists its result via [`crate::persistence::Persistence`].
    fn uppercased_file_words(&self, file: FileId) -> Vec<String>;
}

fn interruptible_work(db: &dyn NovaIde, file: FileId, steps: u32) -> u64 {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "interruptible_work", ?file, steps).entered();

    let mut acc: u64 = 0;
    for i in 0..steps {
        cancel::checkpoint_cancelled(db, i);
        acc = acc.wrapping_add(i as u64 ^ file.to_raw() as u64);
        std::hint::black_box(acc);
    }

    db.record_query_stat("interruptible_work", start.elapsed());
    acc
}

const UPPERCASED_FILE_WORDS_QUERY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
struct UppercasedFileWordsArgs {
    file: u32,
}

fn compute_uppercased_file_words(text: &str) -> Vec<String> {
    #[cfg(test)]
    UPPERCASED_FILE_WORDS_COMPUTE_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    text.split_whitespace()
        .map(|word| word.to_ascii_uppercase())
        .collect()
}

fn uppercased_file_words(db: &dyn NovaIde, file: FileId) -> Vec<String> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "uppercased_file_words", ?file).entered();

    cancel::check_cancelled(db);

    // Always touch the underlying Salsa inputs so dependency tracking works even
    // when we hit the persistent cache.
    let text = if db.file_exists(file) {
        db.file_content(file)
    } else {
        Arc::new(String::new())
    };

    let mut input_fingerprints = BTreeMap::new();
    input_fingerprints.insert(
        "file_content".to_string(),
        Fingerprint::from_bytes(text.as_bytes()),
    );

    let args = UppercasedFileWordsArgs { file: file.to_raw() };

    let result = db.persistence().get_or_compute_derived(
        "uppercased_file_words",
        UPPERCASED_FILE_WORDS_QUERY_SCHEMA_VERSION,
        &args,
        &input_fingerprints,
        || compute_uppercased_file_words(text.as_str()),
    );

    db.record_query_stat("uppercased_file_words", start.elapsed());
    result
}
